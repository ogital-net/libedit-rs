//! C shim functions that safely wrap libedit's variadic functions.
//!
//! Calling C variadic functions from Rust is undefined behavior. These
//! small C helpers bridge the gap by exposing fixed-argument wrappers
//! around the common variadic call patterns.

use libc::{c_char, c_uchar, wchar_t};
use libedit_sys::*;
use std::ffi::CStr;

/// Trampoline type for the libedit prompt callback: `wchar_t *(*)(EditLine *)`.
pub(crate) type PromptFn = unsafe extern "C" fn(*mut EditLine) -> *mut wchar_t;

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
    pub(crate) fn shim_el_wset_prompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: wchar_t)
        -> i32;
    pub(crate) fn shim_el_wset_rprompt_esc_fn(
        el: *mut EditLine,
        f: PromptFn,
        delim: wchar_t,
    ) -> i32;
    pub(crate) fn shim_el_wgets(
        el: *mut EditLine,
        count: *mut i32,
        err: *mut i32,
    ) -> *const wchar_t;
    pub(crate) fn shim_el_addfn(
        el: *mut EditLine,
        name: *const c_char,
        help: *const c_char,
        f: ElFn,
    ) -> i32;
    pub(crate) fn shim_el_bind(el: *mut EditLine, key: *const c_char, fnname: *const c_char)
        -> i32;
    pub(crate) fn shim_el_set_getcfn(el: *mut EditLine, f: GetcFn) -> i32;
    pub(crate) fn shim_el_wset_hist(el: *mut EditLine, h: *mut HistoryW) -> i32;

    pub(crate) fn shim_history_w_op(h: *mut HistoryW, ev: *mut HistEventW, op: i32) -> i32;
    pub(crate) fn shim_history_w_op_int(
        h: *mut HistoryW,
        ev: *mut HistEventW,
        op: i32,
        arg: i32,
    ) -> i32;
    pub(crate) fn shim_history_w_op_wstr(
        h: *mut HistoryW,
        ev: *mut HistEventW,
        op: i32,
        s: *const wchar_t,
    ) -> i32;
    pub(crate) fn shim_history_w_op_str(
        h: *mut HistoryW,
        ev: *mut HistEventW,
        op: i32,
        s: *const c_char,
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

/// Register the prompt trampoline `f` in ESC-aware mode via `el_wset`,
/// so bytes enclosed in `delim` are ignored when computing the prompt's
/// visible width. The callback returns a `wchar_t*`.
pub(crate) unsafe fn el_set_prompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: wchar_t) -> i32 {
    unsafe { shim_el_wset_prompt_esc_fn(el, f, delim) }
}

/// Register the right-prompt trampoline `f` in ESC-aware mode via `el_wset`.
pub(crate) unsafe fn el_set_rprompt_esc_fn(el: *mut EditLine, f: PromptFn, delim: wchar_t) -> i32 {
    unsafe { shim_el_wset_rprompt_esc_fn(el, f, delim) }
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
pub(crate) unsafe fn el_wgets_err(
    el: *mut EditLine,
    count: *mut i32,
    err: *mut i32,
) -> *const wchar_t {
    unsafe { shim_el_wgets(el, count, err) }
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

pub(crate) unsafe fn el_wset_hist(el: *mut EditLine, h: *mut HistoryW) -> i32 {
    unsafe { shim_el_wset_hist(el, h) }
}

// -- History helpers (wide / HistoryW API) --

/// Helper: call a no-arg history op and return the event if successful.
unsafe fn history_op(h: *mut HistoryW, op: i32) -> Option<HistEventW> {
    let mut ev = HistEventW::default();
    let ret = unsafe { shim_history_w_op(h, &mut ev, op) };
    if ret < 0 || ev.str_.is_null() {
        None
    } else {
        Some(ev)
    }
}

/// Helper: call an int-arg history op and return the event if successful.
unsafe fn history_op_int(h: *mut HistoryW, op: i32, arg: i32) -> Option<HistEventW> {
    let mut ev = HistEventW::default();
    let ret = unsafe { shim_history_w_op_int(h, &mut ev, op, arg) };
    if ret < 0 || ev.str_.is_null() {
        None
    } else {
        Some(ev)
    }
}

pub(crate) unsafe fn history_w_setsize(h: *mut HistoryW, n: i32) {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op_int(h, &mut ev, H_SETSIZE as i32, n) };
}

pub(crate) unsafe fn history_w_setunique(h: *mut HistoryW, unique: bool) {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op_int(h, &mut ev, H_SETUNIQUE as i32, unique as i32) };
}

/// Add an entry via `H_ENTER`. Returns:
/// - `1` if a new entry was inserted
/// - `0` if the entry was suppressed by unique mode (duplicate)
/// - `-1` on allocation failure
pub(crate) unsafe fn history_w_enter(h: *mut HistoryW, s: *const wchar_t) -> i32 {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op_wstr(h, &mut ev, H_ENTER as i32, s) }
}

pub(crate) unsafe fn history_w_first(h: *mut HistoryW) -> Option<HistEventW> {
    unsafe { history_op(h, H_FIRST as i32) }
}

pub(crate) unsafe fn history_w_last(h: *mut HistoryW) -> Option<HistEventW> {
    unsafe { history_op(h, H_LAST as i32) }
}

pub(crate) unsafe fn history_w_next(h: *mut HistoryW) -> Option<HistEventW> {
    unsafe { history_op(h, H_NEXT as i32) }
}

pub(crate) unsafe fn history_w_prev(h: *mut HistoryW) -> Option<HistEventW> {
    unsafe { history_op(h, H_PREV as i32) }
}

pub(crate) unsafe fn history_w_curr(h: *mut HistoryW) -> Option<HistEventW> {
    unsafe { history_op(h, H_CURR as i32) }
}

pub(crate) unsafe fn history_w_len(h: *mut HistoryW) -> usize {
    let mut ev = HistEventW::default();
    let ret = unsafe { shim_history_w_op(h, &mut ev, H_GETSIZE as i32) };
    if ret < 0 {
        0
    } else {
        ev.num as usize
    }
}

pub(crate) unsafe fn history_w_clear_all(h: *mut HistoryW) {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op(h, &mut ev, H_CLEAR as i32) };
}

pub(crate) unsafe fn history_w_load(h: *mut HistoryW, path: &CStr) -> i32 {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op_str(h, &mut ev, H_LOAD as i32, path.as_ptr()) }
}

pub(crate) unsafe fn history_w_save(h: *mut HistoryW, path: &CStr) -> i32 {
    let mut ev = HistEventW::default();
    unsafe { shim_history_w_op_str(h, &mut ev, H_SAVE as i32, path.as_ptr()) }
}

pub(crate) unsafe fn history_w_next_event(h: *mut HistoryW, num: i32) -> Option<HistEventW> {
    unsafe { history_op_int(h, H_NEXT_EVENT as i32, num) }
}

pub(crate) unsafe fn history_w_prev_event(h: *mut HistoryW, num: i32) -> Option<HistEventW> {
    unsafe { history_op_int(h, H_PREV_EVENT as i32, num) }
}
