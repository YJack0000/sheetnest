//! Link-time shims for the C++ geometry kernel.
//!
//! `clipper2` -> `clipper2c-sys` compiles Clipper2 with the WASI SDK's
//! `clang++` against the libc++ headers, because `wasm32-unknown-unknown` has
//! no C++ standard library of its own. That leaves a handful of undefined
//! symbols in the archive. Linking WASI's `libc++`/`libc` to satisfy them
//! pulls in `wasi_snapshot_preview1` imports (`fd_write`, `proc_exit`, …) that
//! a browser cannot provide, so instead the few symbols the kernel actually
//! reaches for are defined here, on top of Rust's own allocator.
//!
//! `scripts/build.sh` fails the build if the finished `.wasm` still imports
//! anything from the `env` module, so a Clipper2 or toolchain bump that starts
//! using a new symbol shows up as a clear error rather than a module that
//! silently refuses to instantiate.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

/// `__STDCPP_DEFAULT_NEW_ALIGNMENT__` is 16 on wasm32, and `operator delete`
/// is not told how big the block was, so the same 16 bytes carry the size.
const ALIGN: usize = 16;

fn cxx_alloc(size: usize) -> *mut u8 {
    let Some(total) = size.checked_add(ALIGN) else {
        panic!("clipper2: allocation size overflow");
    };
    let Ok(layout) = Layout::from_size_align(total, ALIGN) else {
        panic!("clipper2: invalid allocation layout");
    };
    // SAFETY: `total >= ALIGN > 0`, so the layout is non-zero-sized.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        handle_alloc_error(layout);
    }
    // SAFETY: `base` is a fresh 16-aligned block of at least `size + ALIGN`
    // bytes, so the header write and the offset are both in bounds.
    unsafe {
        base.cast::<usize>().write(total);
        base.add(ALIGN)
    }
}

/// # Safety
/// `ptr` must be null or have come from [`cxx_alloc`].
unsafe fn cxx_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: by contract `ptr` is `base + ALIGN` of a live [`cxx_alloc`]
    // block, so the header holds the size it was allocated with.
    unsafe {
        let base = ptr.sub(ALIGN);
        let total = base.cast::<usize>().read();
        dealloc(base, Layout::from_size_align_unchecked(total, ALIGN));
    }
}

// ---- operator new / delete ------------------------------------------------
// Symbol names are the Itanium C++ ABI manglings clang emits for wasm32.

/// `operator new(size_t)`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _Znwm(size: usize) -> *mut u8 {
    cxx_alloc(size)
}

/// `operator new[](size_t)`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _Znam(size: usize) -> *mut u8 {
    cxx_alloc(size)
}

/// `operator new(size_t, const std::nothrow_t&)`. Out of memory aborts here
/// rather than returning null; on wasm the allocator only fails when the
/// module is already out of address space.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZnwmRKSt9nothrow_t(size: usize, _tag: *const u8) -> *mut u8 {
    cxx_alloc(size)
}

/// `operator delete(void*)`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZdlPv(ptr: *mut u8) {
    // SAFETY: C++ only passes back pointers `operator new` returned.
    unsafe { cxx_free(ptr) }
}

/// `operator delete[](void*)`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZdaPv(ptr: *mut u8) {
    // SAFETY: C++ only passes back pointers `operator new[]` returned.
    unsafe { cxx_free(ptr) }
}

// ---- the throw path -------------------------------------------------------
// A C++ exception cannot cross the `extern "C"` boundary into Rust anyway, so
// the whole path ends in a panic: with the panic hook installed that surfaces
// as a readable `console.error` instead of a bare `unreachable` trap.

#[unsafe(no_mangle)]
extern "C" fn __cxa_allocate_exception(size: usize) -> *mut u8 {
    cxx_alloc(size)
}

#[unsafe(no_mangle)]
extern "C" fn __cxa_throw(_exception: *mut u8, _type_info: *mut u8, _dtor: *mut u8) -> ! {
    panic!("clipper2 raised a C++ exception; the geometry kernel cannot continue");
}

// Constructors and destructors for the exception types Clipper2's containers
// can throw. They only ever run on the way to `__cxa_throw` above, which never
// returns, so nothing observes the object they leave behind.

/// `std::logic_error::logic_error(const char*)`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZNSt11logic_errorC2EPKc(_this: *mut u8, _what: *const u8) {}

/// `std::length_error::~length_error()`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZNSt12length_errorD1Ev(_this: *mut u8) {}

/// `std::bad_array_new_length::bad_array_new_length()`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZNSt20bad_array_new_lengthC1Ev(_this: *mut u8) {}

/// `std::bad_array_new_length::~bad_array_new_length()`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZNSt20bad_array_new_lengthD1Ev(_this: *mut u8) {}

/// `std::__2::bad_function_call::~bad_function_call()`
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn _ZNSt3__217bad_function_callD1Ev(_this: *mut u8) {}
