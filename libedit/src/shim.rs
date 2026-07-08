//! C shim functions that safely wrap libedit's variadic functions.
//!
//! Calling C variadic functions from Rust is undefined behavior. These
//! small C helpers bridge the gap by exposing fixed-argument wrappers
//! around the common variadic call patterns.

use libedit_sys::*;
use std::ffi::CStr;
use std::os::raw::{c_char, c_uchar};
use std::ptr;

/// Trampoline type for the libedit prompt callback: `char *(*)(EditLine *)`.
pub(crate) type PromptFn = unsafe extern "C" fn(*mut EditLine) -> *mut c_char;

/// Trampoline type for a libedit editor function bound via `EL_ADDFN`:
/// `unsigned char (*)(EditLine *, int)`.
pub(crate) type ElFn = unsafe extern "C" fn(*mut EditLine, i32) -> c_uchar;

/// Trampoline type for the libedit get-character callback (`EL_GETCFN`):
/// `int (*)(EditLine *, wchar_t *)` (el_rfunc_t). Returns 1 on success
/// (character written to the out-param), 0 on EOF, -1 on error.
/// libedit uses `wchar_t` internally on Linux.
pub(crate) type GetcFn = unsafe extern "C" fn(*mut EditLine, *mut wchar_t) -> i32;

extern "C" {
    pub(crate) fn shim_el_set(el: *mut EditLine, op: i32, arg: usize) -> i32;
    pub(crate) fn shim_el_get(el: *mut EditLine, op: i32, arg: *mut usize) -> i32;
    pub(crate) fn shim_el_set_clientdata(el: *mut EditLine, data: *mut std::ffi::c_void) -> i32;
    pub(crate) fn shim_el_set_prompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: c_char) -> i32;
    pub(crate) fn shim_el_set_rprompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: c_char) -> i32;
    pub(crate) fn shim_el_gets(el: *mut EditLine, count: *mut i32, err: *mut i32) -> *const c_char;
    pub(crate) fn shim_el_addfn(
        el: *mut EditLine,
        name: *const c_char,
        help: *const c_char,
        f: ElFn,
    ) -> i32;
    pub(crate) fn shim_el_bind(el: *mut EditLine, key: *const c_char, fnname: *const c_char)
        -> i32;
    pub(crate) fn shim_el_set_getcfn(el: *mut EditLine, f: GetcFn) -> i32;
    pub(crate) fn shim_el_set_hist(el: *mut EditLine, h: *mut History) -> i32;

    pub(crate) fn shim_history_enter(h: *mut History, ev: *mut HistEvent, s: *const c_char) -> i32;
    pub(crate) fn shim_history_first(h: *mut History, ev: *mut HistEvent) -> i32;
    pub(crate) fn shim_history_getsize(h: *mut History, ev: *mut HistEvent) -> i32;
    pub(crate) fn shim_history_setsize(h: *mut History, ev: *mut HistEvent, n: i32) -> i32;
    pub(crate) fn shim_history_setunique(h: *mut History, ev: *mut HistEvent, unique: i32) -> i32;
    pub(crate) fn shim_history_clear(h: *mut History, ev: *mut HistEvent) -> i32;
    pub(crate) fn shim_history_load(
        h: *mut History,
        ev: *mut HistEvent,
        path: *const c_char,
    ) -> i32;
    pub(crate) fn shim_history_save(
        h: *mut History,
        ev: *mut HistEvent,
        path: *const c_char,
    ) -> i32;
}

// -- EditLine helpers --

pub(crate) unsafe fn el_set_int(el: *mut EditLine, op: i32, val: i32) -> i32 {
    unsafe { shim_el_set(el, op, val as usize) }
}

pub(crate) unsafe fn el_get_int(el: *mut EditLine, op: i32) -> Option<i32> {
    let mut val: usize = 0;
    let ret = unsafe { shim_el_get(el, op, &mut val) };
    if ret == 0 {
        Some(val as i32)
    } else {
        None
    }
}

/// Read a pointer-valued editor parameter (e.g. `EL_CLIENTDATA`) into `out`
/// as a `usize`. Returns the raw libedit return code.
pub(crate) unsafe fn el_get_ptr(el: *mut EditLine, op: i32, out: *mut usize) -> i32 {
    unsafe { shim_el_get(el, op, out) }
}

/// Store `data` as the editor's client-data pointer.
pub(crate) unsafe fn el_set_clientdata(el: *mut EditLine, data: *mut std::ffi::c_void) -> i32 {
    unsafe { shim_el_set_clientdata(el, data) }
}

/// Select the editor key-binding style via `EL_EDITOR`. `name` must be a
/// NUL-terminated `"emacs"` or `"vi"` and must outlive the call. Routed through
/// `shim_el_set`, which passes the pointer to libedit as a `void*`.
pub(crate) unsafe fn el_set_editor(el: *mut EditLine, name: *const c_char) -> i32 {
    unsafe { shim_el_set(el, EL_EDITOR as i32, name as usize) }
}

/// Register the prompt trampoline `f` in ESC-aware mode, so bytes enclosed in
/// `delim` are ignored when computing the prompt's visible width.
pub(crate) unsafe fn el_set_prompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: c_char) -> i32 {
    unsafe { shim_el_set_prompt_esc_fn(el, f, delim) }
}

/// Register the right-prompt (hint) trampoline `f` in ESC-aware mode.
pub(crate) unsafe fn el_set_rprompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: c_char) -> i32 {
    unsafe { shim_el_set_rprompt_esc_fn(el, f, delim) }
}

/// Override libedit's get-character function via `EL_GETCFN`. The Rust side
/// supplies a trampoline that reads a byte, returns it via the out-param, and
/// optionally draws suggestion ghost text after libedit's refresh. Passing
/// this installs our read loop; removal is not needed (the trampoline is a
/// no-op passthrough when no suggester is registered).
pub(crate) unsafe fn el_set_getcfn(el: *mut EditLine, f: GetcFn) -> i32 {
    unsafe { shim_el_set_getcfn(el, f) }
}

/// Call `el_gets`, capturing `errno` immediately afterward so the caller can
/// distinguish a signal-interrupted read (`errno == EINTR`) from a genuine
/// end-of-file. Returns the raw line pointer; `count` and `err` are written
/// with libedit's returned char count and the captured errno respectively.
pub(crate) unsafe fn el_gets_err(
    el: *mut EditLine,
    count: *mut i32,
    err: *mut i32,
) -> *const c_char {
    unsafe { shim_el_gets(el, count, err) }
}

/// Register editor function `name` backed by trampoline `f`, then bind `key`
/// to it.
pub(crate) unsafe fn el_addfn_bind(
    el: *mut EditLine,
    name: &CStr,
    help: &CStr,
    key: &CStr,
    f: ElFn,
) -> i32 {
    let rc = unsafe { shim_el_addfn(el, name.as_ptr(), help.as_ptr(), f) };
    if rc != 0 {
        return rc;
    }
    unsafe { shim_el_bind(el, key.as_ptr(), name.as_ptr()) }
}

/// Register editor function `name` backed by trampoline `f`, without binding
/// any key. Pair with [`el_bind`] to attach one or more key sequences.
pub(crate) unsafe fn el_addfn(el: *mut EditLine, name: &CStr, help: &CStr, f: ElFn) -> i32 {
    unsafe { shim_el_addfn(el, name.as_ptr(), help.as_ptr(), f) }
}

/// Bind key sequence `key` to the already-registered editor function
/// `fnname`. Multiple keys may be bound to the same function.
pub(crate) unsafe fn el_bind(el: *mut EditLine, key: &CStr, fnname: &CStr) -> i32 {
    unsafe { shim_el_bind(el, key.as_ptr(), fnname.as_ptr()) }
}

pub(crate) unsafe fn el_set_hist(el: *mut EditLine, h: *mut History) -> i32 {
    unsafe { shim_el_set_hist(el, h) }
}

// -- History helpers --

pub(crate) unsafe fn history_setsize(h: *mut History, n: i32) {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_setsize(h, &mut ev, n) };
}

pub(crate) unsafe fn history_setunique(h: *mut History, unique: bool) {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_setunique(h, &mut ev, unique as i32) };
}

pub(crate) unsafe fn history_enter(h: *mut History, s: &CStr) -> bool {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_enter(h, &mut ev, s.as_ptr()) >= 0 }
}

pub(crate) unsafe fn history_first(h: *mut History) -> Option<String> {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    let ret = unsafe { shim_history_first(h, &mut ev) };
    if ret < 0 || ev.str_.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ev.str_) }
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) unsafe fn history_len(h: *mut History) -> usize {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    let ret = unsafe { shim_history_getsize(h, &mut ev) };
    if ret < 0 {
        0
    } else {
        ev.num as usize
    }
}

pub(crate) unsafe fn history_clear_all(h: *mut History) {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_clear(h, &mut ev) };
}

pub(crate) unsafe fn history_load(h: *mut History, path: &CStr) -> i32 {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_load(h, &mut ev, path.as_ptr()) }
}

pub(crate) unsafe fn history_save(h: *mut History, path: &CStr) -> i32 {
    let mut ev = HistEvent {
        num: 0,
        str_: ptr::null(),
    };
    unsafe { shim_history_save(h, &mut ev, path.as_ptr()) }
}
