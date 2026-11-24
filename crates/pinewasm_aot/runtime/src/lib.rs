//! The AOT runtime module: [`pinewasm_num`]'s pure numeric surface,
//! compiled to WebAssembly.
//!
//! `pinewasm_aot` splices the resulting module's functions into every
//! emitted artifact that needs one, which is how an artifact that used to
//! import `host.pure` now imports nothing at all. See `docs/aot.md` §5 and
//! `crates/pinewasm_aot/src/runtime.rs` for the splice.
//!
//! Every export takes and returns plain `f64` with the interpreter's na
//! (NaN) convention already applied, so the emitter lowers a `CallPure` to a
//! single `call` with no surrounding normalization — the exact shape the
//! `host.pure` import had.
//!
//! Unary exports ignore their second parameter. The uniform `(f64, f64) ->
//! f64` signature means the emitter stages arguments identically for every
//! id and one wasm type serves all twelve.
//!
//! Three further export families are gated behind cargo features: [`ta`], the
//! stateful `ta.*` engine that replaced the `host.ta` import; [`standalone_wasm`], the
//! JSON/serialization/allocator machinery behind the standalone WebAssembly consumer ABI; and
//! [`broker`], the shared strategy emulator used by standalone WebAssembly strategy artifacts.
//! The crate therefore builds to four checked-in blobs — base, `_ta`, `_standalone_wasm`
//! (which implies `ta`), and `_broker` (which implies `standalone_wasm`) — through `cargo
//! xtask aot-runtime-all`, because the splice copies
//! data segments verbatim: one combined blob would drag every family's
//! rodata into every artifact, breaking reproducible rebuilds for modules
//! that never call them.

// `forbid(unsafe_code)` would reject `#[no_mangle]`, which rustc classifies
// as an unsafe attribute. The numerics themselves live in
// `pinewasm_num`, which does forbid unsafe; [`standalone_wasm`] uses it for raw
// linear-memory access, with the soundness argument at each site.
//
// `aeneas_host` is a rustc `--cfg` that only the Charon/Aeneas extraction
// passes (`--rustc-arg=--cfg=aeneas_host`): Charon analyses the crate on the
// host, where a `no_std` cdylib cannot use the host's unwinding core, so that
// run links `std` and drops the custom panic handler. It is a cfg, not a cargo
// feature, so `--all-features` builds (CI's wasm32 clippy and docs) keep
// checking the real `no_std` crate; no checked-in blob sets it.
#![cfg_attr(not(any(test, aeneas_host, feature = "chart")), no_std)]

#[cfg(feature = "aeneas")]
pub mod aeneas_roots_aot_runtime;
#[cfg(any(feature = "standalone_wasm", feature = "aeneas"))]
mod arena_bounds;
#[cfg(feature = "broker")]
pub mod broker;
#[cfg(feature = "standalone_wasm")]
pub mod calendar;
#[cfg(feature = "chart")]
pub mod chart;
#[cfg(feature = "standalone_wasm")]
pub mod standalone_wasm;
#[cfg(feature = "ta")]
pub mod ta;
#[cfg(feature = "ta")]
mod ta_offsets;

#[cfg(feature = "broker")]
extern crate alloc;

#[cfg(not(any(test, aeneas_host, feature = "chart")))]
use core::panic::PanicInfo;

/// The `broker` build's global allocator: a bump allocator over the standalone WebAssembly arena
/// band ([`standalone_wasm`]'s allocator, the one the artifact exports as `alloc`).
///
/// The broker emulator ([`pinewasm_broker`]) is `no_std + alloc`; its
/// order book, snapshot lane, and equity lanes live in whatever the global
/// allocator hands out. Backing that with the arena band is safe by the standalone WebAssembly
/// entry point's sequencing: the band is relocated above the whole run
/// layout *before* `run()` starts, the input JSON is dropped before `run()`
/// (so the band base is free), and the OUT document is written above
/// whatever the run allocated. The next evaluation's `pinewasm_rt_broker_init`
/// forgets the stale broker state and rewinds the whole band to its floor
/// (nothing in it is live by then), which is what keeps a resident
/// instance's memory flat and preserves the documented statelessness contract
/// instead of stacking each run's dead broker under the next.
///
/// Failure discipline: [`standalone_wasm::arena_alloc_aligned`] **traps** on exhausted
/// memory instead of returning null. Returning null would route through
/// `alloc`'s `handle_alloc_error` → `panic!` → `core::fmt`, whose vtables
/// need an element section the runtime-blob splice rejects; a trap is the
/// infrastructure-class abort the standalone WebAssembly host already maps, and the
/// never-null result lets LTO discard the error plumbing entirely.
#[cfg(feature = "broker")]
mod broker_alloc {
    use core::alloc::{GlobalAlloc, Layout};

    struct ArenaBump;

    // SAFETY: the standalone WebAssembly entry-point protocol reserves and initializes the arena
    // band before broker code can allocate. `arena_alloc_aligned` honors every
    // `Layout` size/alignment or traps; `arena_free` and `arena_resize` take the
    // exact pointer and layout of a live allocation and size its block the same
    // way `arena_alloc_aligned` did.
    unsafe impl GlobalAlloc for ArenaBump {
        #[inline]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            crate::standalone_wasm::arena_alloc_aligned(layout.size(), layout.align())
        }

        #[inline]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // Rewind when this is the top block, else reuse it through its
            // size class; the next evaluation's `pinewasm_rt_broker_init`
            // reclaims everything wholesale.
            crate::standalone_wasm::arena_free(ptr, layout.size(), layout.align());
        }

        /// Resize in place where the block allows it. The default would
        /// copy a top block above itself, so a vector doubling to N bytes
        /// held about 2N.
        #[inline]
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if crate::standalone_wasm::arena_resize(ptr, layout.size(), new_size, layout.align()) {
                return ptr;
            }
            // SAFETY: the caller guarantees `new_size`, rounded up to
            // `layout.align()`, does not overflow `isize`.
            let moved =
                unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };
            // SAFETY: `ptr` is still allocated, so the fresh block `moved`
            // does not overlap it, and both hold the copied length.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            moved
        }
    }

    #[global_allocator]
    static ALLOCATOR: ArenaBump = ArenaBump;
}

/// Defensive final boundary required by `no_std`. Valid compiler-emitted calls
/// are designed to reach no panic path: the standalone WebAssembly and TA modules avoid unchecked
/// indexing and use checked arithmetic for representable overflow, while the
/// runtime splice rejects the table/element-section shape introduced by
/// formatting panic machinery. If an internal invariant is nevertheless
/// violated, trap immediately on Wasm rather than entering a
/// host-fuel-dependent loop in a standalone artifact.
#[cfg(not(any(test, aeneas_host, feature = "chart")))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    #[cfg(target_arch = "wasm32")]
    core::arch::wasm32::unreachable();

    #[cfg(not(target_arch = "wasm32"))]
    loop {
        core::hint::spin_loop();
    }
}

macro_rules! export {
    ($(#[$m:meta])* $name:ident => |$a:ident, $b:ident| $body:expr) => {
        $(#[$m])*
        #[no_mangle]
        pub extern "C" fn $name($a: f64, $b: f64) -> f64 {
            let _ = $b;
            $body
        }
    };
}

export!(
    /// `math.exp`
    pinewasm_rt_exp => |a, b| pinewasm_num::pure_exp(a)
);
export!(
    /// `math.log`
    pinewasm_rt_log => |a, b| pinewasm_num::pure_log(a)
);
export!(
    /// `math.log10`
    pinewasm_rt_log10 => |a, b| pinewasm_num::pure_log10(a)
);
export!(
    /// `math.sin`
    pinewasm_rt_sin => |a, b| pinewasm_num::pure_sin(a)
);
export!(
    /// `math.cos`
    pinewasm_rt_cos => |a, b| pinewasm_num::pure_cos(a)
);
export!(
    /// `math.tan`
    pinewasm_rt_tan => |a, b| pinewasm_num::pure_tan(a)
);
export!(
    /// `math.asin`
    pinewasm_rt_asin => |a, b| pinewasm_num::pure_asin(a)
);
export!(
    /// `math.acos`
    pinewasm_rt_acos => |a, b| pinewasm_num::pure_acos(a)
);
export!(
    /// `math.atan`
    pinewasm_rt_atan => |a, b| pinewasm_num::pure_atan(a)
);
export!(
    /// `math.pow(a, b)`
    pinewasm_rt_pow => |a, b| pinewasm_num::pure_pow(a, b)
);
export!(
    /// `math.round(value, precision)`
    pinewasm_rt_round2 => |a, b| pinewasm_num::pure_round2(a, b)
);
export!(
    /// The `%` operator (WebAssembly has no `f64.rem`).
    pinewasm_rt_rem => |a, b| pinewasm_num::pure_rem(a, b)
);
