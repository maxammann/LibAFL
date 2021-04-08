use hashbrown::HashMap;
use nix::{
    libc::{memmove, memset},
    sys::mman::{mmap, mprotect, MapFlags, ProtFlags},
};

use libc::{siginfo_t, ucontext_t, pthread_atfork, sysconf, _SC_PAGESIZE};
use std::{
    cell::RefCell,
    cell::RefMut,
    ffi::c_void,
    fs::File,
    io::{BufRead, BufReader},
    pin::Pin,
};
use regex::Regex;
use rangemap::RangeSet;
use gothook::GotHookLibrary;
use libafl::bolts::os::unix_signals::{setup_signal_handler, Signal, Handler};
use backtrace::resolve;
use frida_gum::Backtracer;
use dynasmrt::{DynasmApi, DynasmLabelApi, ExecutableBuffer, dynasm};

static mut ALLOCATOR_SINGLETON: Option<RefCell<Allocator>> = None;

struct Allocator {
    page_size: usize,
    shadow_offset: usize,
    allocations: HashMap<usize, usize>,
    shadow_pages: RangeSet<usize>,
}

impl Allocator {
    pub fn new() -> Self {
        Self {
            page_size: unsafe { sysconf(_SC_PAGESIZE) as usize },
            shadow_offset: 1 << 36,
            allocations: HashMap::new(),
            shadow_pages: RangeSet::new(),
        }
    }

    pub fn get() -> RefMut<'static, Allocator> {
        unsafe {
            if ALLOCATOR_SINGLETON.as_mut().is_none() {
                ALLOCATOR_SINGLETON = Some(RefCell::new(Allocator::new()));
            }

            // we need to loop in case there is a race between threads at init time.
            //loop {
            //if let Ok(allocref) = ALLOCATOR_SINGLETON.as_mut().unwrap().try_borrow_mut() {
            //return allocref;
            //}
            //}
            ALLOCATOR_SINGLETON
                .as_mut()
                .unwrap()
                .try_borrow_mut()
                .unwrap()
        }
    }

    pub fn init(&self) {
        unsafe extern "C" fn atfork() {
            ALLOCATOR_SINGLETON = None;
            Allocator::get();
        }
        unsafe {
            pthread_atfork(None, None, Some(atfork));
        }
    }

    #[inline]
    fn round_up_to_page(&self, size: usize) -> usize {
        ((size + self.page_size) / self.page_size) * self.page_size
    }

    #[inline]
    fn round_down_to_page(&self, value: usize) -> usize {
        (value / self.page_size) * self.page_size
    }

    pub unsafe fn alloc(&mut self, size: usize, _alignment: usize) -> *mut c_void {
        let rounded_up_size = self.round_up_to_page(size);

        let mapping = match mmap(
            std::ptr::null_mut(),
            rounded_up_size + 2 * self.page_size,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
            -1,
            0,
        ) {
            Ok(mapping) => mapping as usize,
            Err(err) => {
                println!("An error occurred while mapping memory: {:?}", err);
                return std::ptr::null_mut();
            }
        };

        let (shadow_mapping_start, _shadow_mapping_size) = self.map_shadow_for_region(
            mapping,
            mapping + rounded_up_size + 2 * self.page_size,
            false,
        );

        // unpoison the shadow memory for the allocation itself
        self.unpoison(shadow_mapping_start + self.page_size / 8, size);

        self.allocations.insert(mapping + self.page_size, size);

        (mapping + self.page_size) as *mut c_void
    }

    pub unsafe fn release(&self, ptr: *mut c_void) {
        let size = match self.allocations.get(&(ptr as usize)) {
            Some(size) => size,
            None => return,
        };
        let shadow_mapping_start = (ptr as usize >> 3) + self.shadow_offset;

        // poison the shadow memory for the allocation
        //println!("poisoning {:x} for {:x}", shadow_mapping_start, size / 8 + 1);
        memset(shadow_mapping_start as *mut c_void, 0x00, size / 8);
        let remainder = size % 8;
        if remainder > 0 {
            memset((shadow_mapping_start + size / 8) as *mut c_void, 0x00, 1);
        }
    }

    pub fn get_usable_size(&self, ptr: *mut c_void) -> usize {
        *self.allocations.get(&(ptr as usize)).unwrap()
    }

    fn unpoison(&self, start: usize, size: usize) {
        //println!("unpoisoning {:x} for {:x}", start, size / 8 + 1);
        unsafe {
            //println!("memset: {:?}", start as *mut c_void);
            memset(start as *mut c_void, 0xff, size / 8);

            let remainder = size % 8;
            if remainder > 0 {
                //println!("remainder: {:x}, offset: {:x}", remainder, start + size / 8);
                memset(
                    (start + size / 8) as *mut c_void,
                    (0xff << (8 - remainder)) & 0xff,
                    1,
                );
            }
        }
    }

    /// Map shadow memory for a region, and optionally unpoison it
    pub fn map_shadow_for_region(
        &mut self,
        start: usize,
        end: usize,
        unpoison: bool,
    ) -> (usize, usize) {
        //println!("start: {:x}, end {:x}, size {:x}", start, end, end - start);

        let shadow_mapping_start = (start >> 3) + self.shadow_offset;
        let shadow_start = self.round_down_to_page(shadow_mapping_start);
        let shadow_end = self.round_up_to_page((end - start) / 8) + self.page_size + shadow_start;

        for range in self.shadow_pages.gaps(&(shadow_start..shadow_end)) {
            //println!("mapping: {:x} - {:x}", mapping_start * self.page_size, (mapping_end + 1) * self.page_size);
            unsafe {
                mmap(
                    range.start as *mut c_void,
                    range.end - range.start,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED | MapFlags::MAP_PRIVATE,
                    -1,
                    0,
                )
                .expect("An error occurred while mapping shadow memory");
            }
        }

        self.shadow_pages.insert(shadow_start..shadow_end);

        //println!("shadow_mapping_start: {:x}, shadow_size: {:x}", shadow_mapping_start, (end - start) / 8);
        if unpoison {
            self.unpoison(shadow_mapping_start, end - start);
        }

        (shadow_mapping_start, (end - start) / 8)
    }
}

/// Hook for malloc.
pub extern "C" fn asan_malloc(size: usize) -> *mut c_void {
    unsafe { Allocator::get().alloc(size, 0x8) }
}

/// Hook for pvalloc
pub extern "C" fn asan_pvalloc(size: usize) -> *mut c_void {
    unsafe { Allocator::get().alloc(size, 0x8) }
}

/// Hook for valloc
pub extern "C" fn asan_valloc(size: usize) -> *mut c_void {
    unsafe { Allocator::get().alloc(size, 0x8) }
}

/// Hook for calloc
pub extern "C" fn asan_calloc(nmemb: usize, size: usize) -> *mut c_void {
    unsafe { Allocator::get().alloc(size * nmemb, 0x8) }
}

/// Hook for realloc
///
/// # Safety
/// This function is inherently unsafe, as it takes a raw pointer
pub unsafe extern "C" fn asan_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    let mut allocator = Allocator::get();
    let ret = allocator.alloc(size, 0x8);
    if ptr != std::ptr::null_mut() {
        memmove(ret, ptr, allocator.get_usable_size(ptr));
    }
    allocator.release(ptr);
    ret
}

/// Hook for free
///
/// # Safety
/// This function is inherently unsafe, as it takes a raw pointer
pub unsafe extern "C" fn asan_free(ptr: *mut c_void) {
    if ptr != std::ptr::null_mut() {
        Allocator::get().release(ptr);
    }
}

/// Hook for malloc_usable_size
///
/// # Safety
/// This function is inherently unsafe, as it takes a raw pointer
pub unsafe extern "C" fn asan_malloc_usable_size(ptr: *mut c_void) -> usize {
    Allocator::get().get_usable_size(ptr)
}

/// Hook for memalign
pub extern "C" fn asan_memalign(size: usize, alignment: usize) -> *mut c_void {
    unsafe { Allocator::get().alloc(size, alignment) }
}

/// Hook for posix_memalign
///
/// # Safety
/// This function is inherently unsafe, as it takes a raw pointer
pub unsafe extern "C" fn asan_posix_memalign(
    pptr: *mut *mut c_void,
    size: usize,
    alignment: usize,
) -> i32 {
    *pptr = Allocator::get().alloc(size, alignment);
    0
}

/// Hook for mallinfo
pub extern "C" fn asan_mallinfo() -> *mut c_void {
    std::ptr::null_mut()
}

/// Allows one to walk the mappings in /proc/self/maps, caling a callback function for each
/// mapping.
/// If the callback returns true, we stop the walk.
fn walk_self_maps(visitor: &mut dyn FnMut(usize, usize, String, String) -> bool) {
    let re = Regex::new(r"^(?P<start>[0-9a-f]{8,16})-(?P<end>[0-9a-f]{8,16}) (?P<perm>[-rwxp]{4}) (?P<offset>[0-9a-f]{8}) [0-9a-f]+:[0-9a-f]+ [0-9]+\s+(?P<path>.*)$")
        .unwrap();

    let mapsfile = File::open("/proc/self/maps").expect("Unable to open /proc/self/maps");

    for line in BufReader::new(mapsfile).lines() {
        let line = line.unwrap();
        if let Some(caps) = re.captures(&line) {
            if visitor(
                usize::from_str_radix(caps.name("start").unwrap().as_str(), 16).unwrap(),
                usize::from_str_radix(caps.name("end").unwrap().as_str(), 16).unwrap(),
                caps.name("perm").unwrap().as_str().to_string(),
                caps.name("path").unwrap().as_str().to_string(),
            ) {
                break;
            };
        }
    }
}

/// Get the current thread's TLS address
extern "C" {
    fn get_tls_ptr() -> *const c_void;
}

/// Get the start and end address of the mapping containing a particular address
fn mapping_containing(address: *const c_void) -> (usize, usize) {
    let mut result = (0, 0);
    walk_self_maps(&mut |start, end, _permissions, _path| {
        if start <= (address as usize) && (address as usize) < end {
            result = (start, end);
            true
        } else {
            false
        }
    });

    result
}

/// Get the start and end address of the mapping containing a particular address
fn mapping_for_library(libpath: &str) -> (usize, usize) {
    let mut libstart = 0;
    let mut libend = 0;
    walk_self_maps(&mut |start, end, _permissions, path| {
        if libpath == path {
            if libstart == 0 {
                libstart = start;
            }

            libend = end;
        }
        false
    });

    (libstart, libend)
}

pub struct AsanRuntime {
    blob_check_mem_byte: Option<Vec<u8>>,
    blob_check_mem_halfword: Option<Vec<u8>>,
    blob_check_mem_dword: Option<Vec<u8>>,
    blob_check_mem_qword: Option<Vec<u8>>,
    blob_check_mem_16bytes: Option<Vec<u8>>,
}

impl AsanRuntime {
    pub fn new() -> Self {
        let allocator = Allocator::get();
        allocator.init();

        let mut res = Self {
            blob_check_mem_byte: None,
            blob_check_mem_halfword: None,
            blob_check_mem_dword: None,
            blob_check_mem_qword: None,
            blob_check_mem_16bytes: None,
        };

        res.generate_instrumentation_blobs();

        unsafe {
            setup_signal_handler(&mut res).expect("Failed to setup Asan signal handler");
        }
        res
    }

    /// Unpoison all the memory that is currently mapped with read/write permissions.
    pub fn unpoison_all_existing_memory(&self) {
        walk_self_maps(&mut |start, end, _permissions, _path| {
            //if permissions.as_bytes()[0] == b'r' || permissions.as_bytes()[1] == b'w' {
            Allocator::get().map_shadow_for_region(start, end, true);
            //}
            false
        });
    }

    /// Register the current thread with the runtime, implementing shadow memory for its stack and
    /// tls mappings.
    pub fn register_thread(&self) {
        let mut allocator = Allocator::get();
        let (stack_start, stack_end) = Self::current_stack();
        allocator.map_shadow_for_region(stack_start, stack_end, true);

        let (tls_start, tls_end) = Self::current_tls();
        allocator.map_shadow_for_region(tls_start, tls_end, true);
        println!(
            "registering thread with stack {:x}:{:x} and tls {:x}:{:x}",
            stack_start as usize, stack_end as usize, tls_start as usize, tls_end as usize
        );
    }

    /// Determine the stack start, end for the currently running thread
    fn current_stack() -> (usize, usize) {
        let stack_var = 0xeadbeef;
        let stack_address = &stack_var as *const _ as *const c_void;

        mapping_containing(stack_address)
    }

    /// Determine the tls start, end for the currently running thread
    fn current_tls() -> (usize, usize) {
        let tls_address = unsafe { get_tls_ptr() };

        mapping_containing(tls_address)
    }

    /// Locate the target library and hook it's memory allocation functions
    pub fn hook_library(&mut self, path: &str) {
        let target_lib = GotHookLibrary::new(path, false);

        // shadow the library itself, allowing all accesses
        Allocator::get().map_shadow_for_region(target_lib.start(), target_lib.end(), true);

        // Hook all the memory allocator functions
        target_lib.hook_function("malloc", asan_malloc as *const c_void);
        target_lib.hook_function("calloc", asan_calloc as *const c_void);
        target_lib.hook_function("pvalloc", asan_pvalloc as *const c_void);
        target_lib.hook_function("valloc", asan_valloc as *const c_void);
        target_lib.hook_function("realloc", asan_realloc as *const c_void);
        target_lib.hook_function("free", asan_free as *const c_void);
        target_lib.hook_function("memalign", asan_memalign as *const c_void);
        target_lib.hook_function("posix_memalign", asan_posix_memalign as *const c_void);
        target_lib.hook_function(
            "malloc_usable_size",
            asan_malloc_usable_size as *const c_void,
        );
    }

    /// Generate the instrumentation blobs for the current arch.
    fn generate_instrumentation_blobs(&mut self) {
        macro_rules! shadow_check {
            ($ops:ident, $bit:expr) => {dynasm!($ops
                ; .arch aarch64
                ; mov x1, #1
                ; add x1, xzr, x1, lsl #36
                ; add x1, x1, x0, lsr #3
                ; ldrh w1, [x1, #0]
                ; and x0, x0, #7
                ; rev16 w1, w1
                ; rbit w1, w1
                ; lsr x1, x1, #16
                ; lsr x1, x1, x0
                ; tbnz x1, #$bit, ->done
                ; brk #$bit
                ; ->done:
            );};
        }

        let mut ops_check_mem_byte = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops_check_mem_byte, 0);
        self.blob_check_mem_byte = Some(ops_check_mem_byte.finalize().unwrap());

        let mut ops_check_mem_halfword = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops_check_mem_halfword, 1);
        self.blob_check_mem_halfword = Some(ops_check_mem_halfword.finalize().unwrap());

        let mut ops_check_mem_dword = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops_check_mem_dword, 2);
        self.blob_check_mem_dword = Some(ops_check_mem_dword.finalize().unwrap());

        let mut ops_check_mem_qword = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops_check_mem_qword, 3);
        self.blob_check_mem_qword = Some(ops_check_mem_qword.finalize().unwrap());

        let mut ops_check_mem_16bytes = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops_check_mem_16bytes, 4);
        self.blob_check_mem_16bytes = Some(ops_check_mem_16bytes.finalize().unwrap());
    }

    /// Get the blob which checks a byte access
   #[inline]
    pub fn blob_check_mem_byte(&self) -> Pin<&Vec<u8>> {
        Pin::new(self.blob_check_mem_byte.as_ref().unwrap())
    }

    /// Get the blob which checks a halfword access
   #[inline]
    pub fn blob_check_mem_halfword(&self) -> Pin<&Vec<u8>> {
        Pin::new(self.blob_check_mem_halfword.as_ref().unwrap())
    }

    /// Get the blob which checks a dword access
   #[inline]
    pub fn blob_check_mem_dword(&self) -> Pin<&Vec<u8>> {
        Pin::new(self.blob_check_mem_dword.as_ref().unwrap())
    }

    /// Get the blob which checks a qword access
   #[inline]
    pub fn blob_check_mem_qword(&self) -> Pin<&Vec<u8>> {
        Pin::new(self.blob_check_mem_qword.as_ref().unwrap())
    }

    /// Get the blob which checks a 16 byte access
   #[inline]
    pub fn blob_check_mem_16bytes(&self) -> Pin<&Vec<u8>> {
        Pin::new(self.blob_check_mem_16bytes.as_ref().unwrap())
    }
}

#[cfg(unix)]
impl Handler for AsanRuntime {
    fn handle(&mut self, _signal: Signal, _info: siginfo_t, context: &mut ucontext_t) {
        //println!("backtrace:\n {:?}", backtrace::Backtrace::new());

        let mut sigcontext = unsafe { *(((context as *mut  _ as *mut c_void as usize) + 128) as *mut ucontext_t) }.uc_mcontext;

        unsafe {
            sigcontext.regs[0] = (sigcontext.sp as *mut u64).read();
            sigcontext.regs[1] = ((sigcontext.sp + 8) as *mut u64).read();
            sigcontext.sp += 144;
        }
        for reg in 0..=30 {
            print!("x{:02}: 0x{:016x} ", reg, sigcontext.regs[reg]);
            if reg % 4 == 3 {
                println!("");
            }
        }
        print!("sp : 0x{:016x} ",  sigcontext.sp);
        println!("");
        print!("pc : 0x{:016x} ", sigcontext.pc);
        print!("pstate: 0x{:016x} ", sigcontext.pstate);
        print!("fault: 0x{:016x} ", sigcontext.fault_address);
        print!("\nstack:");
        for i in 0..0x100 {
            if i % 4 == 0 {
                print!("\n0x{:016x}: ", sigcontext.sp + i * 8)
            }
            unsafe {
                print!("0x{:016x} ", ((sigcontext.sp  + i * 8) as * mut u64).read());
            }
        }
        println!("\nbacktrace: ");

        for return_address in Backtracer::accurate_with_signal_context(context) {
            resolve(return_address as *mut c_void, |symbol|{
                if symbol.name().is_some() {
                    if symbol.filename().is_some() {
                        println!("- 0x{:016x}: {} - {:?}:{}", return_address, symbol.name().unwrap(), symbol.filename().unwrap(), symbol.lineno().unwrap());
                    } else {
                        println!("- 0x{:016x}: {}", return_address, symbol.name().unwrap());
                    }
                } else {
                    println!("- 0x{:016x}", return_address);
                }
            });
        }

        nix::sys::signal::raise(nix::sys::signal::Signal::SIGSEGV).expect("Failed to suicide");
    }

    fn signals(&self) -> Vec<Signal> {
        vec![
            Signal::SigTrap,
        ]
    }
}