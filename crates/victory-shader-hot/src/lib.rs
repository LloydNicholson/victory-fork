//! The shader translator (`victory_shader::emit::translate`) as a DLL that
//! the running game loads again after each change, so a fix to the
//! translation shows in the game without a restart (`VICTORY_SHADER_HOT`,
//! `victory-gpu`'s `pipeline::hot`).
//!
//! The game and this DLL are built by the same compiler from the same
//! definition of `HostShader`, which the game reads through a pointer and
//! clones into its own memory; the DLL frees its copy
//! ([`victory_hot_free`]: the game's diagnostics build counts its heap
//! through its own allocator, which must not free what it did not hand
//! out). [`victory_hot_layout`] lets the game refuse a DLL whose types
//! differ from its own.
//!
//! Build: `cargo build --profile diag -p victory-shader-hot --features diagnostics`
//! (the target folder's `victory_shader_hot.dll`).

use victory_shader::emit::{HostShader, StaticSampler, UserData};

/// Sizes and alignments of the types that cross, folded together. The game
/// works out the same from its own copies of them.
#[unsafe(no_mangle)]
pub extern "C" fn victory_hot_layout() -> u64 {
    let parts = [
        size_of::<HostShader>(),
        align_of::<HostShader>(),
        size_of::<UserData>(),
        size_of::<StaticSampler>(),
        size_of::<String>(),
    ];
    parts.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3))
}

/// What the game's device said, which the translation depends on.
#[unsafe(no_mangle)]
pub extern "C" fn victory_hot_set_facts(host_max_wave: u32, view_null_slot: u32) {
    if host_max_wave != 0 {
        victory_shader::emit::set_host_max_wave(host_max_wave);
    }
    victory_shader::emit::set_view_heap_limit(view_null_slot);
}

/// Translates the container at `ptr` (`len` bytes). On success `*out` is a
/// boxed `HostShader`, else `*error` a boxed `String`; the caller gives
/// either back to [`victory_hot_free`]. A panic in the translator counts as
/// an error.
///
/// # Safety
///
/// `ptr` points to `len` readable bytes; `out` and `error` are writable.
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn victory_hot_translate(
    ptr: *const u8,
    len: usize,
    out: *mut *mut HostShader,
    error: *mut *mut String,
) -> bool {
    // SAFETY: as the caller promises.
    let container = unsafe { std::slice::from_raw_parts(ptr, len) };
    let done = std::panic::catch_unwind(|| victory_shader::emit::translate(container).map_err(|e| e.to_string()))
        .unwrap_or_else(|p| {
            Err(p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .map_or_else(|| "the translator panicked".to_owned(), |s| format!("the translator panicked: {s}")))
        });
    // SAFETY: writable, as the caller promises.
    unsafe {
        match done {
            Ok(shader) => {
                *out = Box::into_raw(Box::new(shader));
                true
            }
            Err(e) => {
                *error = Box::into_raw(Box::new(e));
                false
            }
        }
    }
}

/// Frees what [`victory_hot_translate`] handed out (either pointer may be
/// null).
///
/// # Safety
///
/// Each pointer is null or came from [`victory_hot_translate`] and is not
/// used after.
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn victory_hot_free(shader: *mut HostShader, error: *mut String) {
    // SAFETY: as the caller promises.
    unsafe {
        if !shader.is_null() {
            drop(Box::from_raw(shader));
        }
        if !error.is_null() {
            drop(Box::from_raw(error));
        }
    }
}
