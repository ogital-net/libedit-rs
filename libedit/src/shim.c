// C shim -- bridges libedit's variadic functions for safe Rust FFI.
// Calling C variadic functions from Rust is undefined behavior;
// these fixed-argument wrappers avoid the issue entirely.
#include <histedit.h>
#include <stdint.h>
#include <errno.h>

// el_gets that also reports errno. libedit's el_gets returns NULL on both a
// genuine end-of-file (Ctrl-D) and a signal-interrupted read (Ctrl-C, when
// EL_SIGNAL is enabled). The two are only distinguishable by errno: a signal
// leaves errno == EINTR. We snapshot errno immediately after the call so the
// Rust side can tell an interrupt apart from EOF.
const wchar_t *shim_el_wgets(EditLine *el, int *count, int *err) {
    errno = 0;
    const wchar_t *line = el_wgets(el, count);
    *err = errno;
    return line;
}

int shim_el_set(EditLine *el, int op, uintptr_t arg) {
    return el_set(el, op, (void *)arg);
}

int shim_el_get(EditLine *el, int op, uintptr_t *arg) {
    return el_get(el, op, (void **)arg);
}

// Store the editor's client-data pointer. The Rust wrapper stashes a pointer
// to its per-editor context here; the prompt and completion trampolines read
// it back via el_get(EL_CLIENTDATA, ...).
int shim_el_set_clientdata(EditLine *el, void *data) {
    return el_set(el, EL_CLIENTDATA, data);
}

// Prompt callbacks. el_wset expects el_pfunc_t = wchar_t *(*)(EditLine *).
typedef wchar_t *(*shim_prompt_fn)(EditLine *);
int shim_el_wset_prompt_esc_fn(EditLine *el, shim_prompt_fn fn, wchar_t delim) {
    return el_wset(el, EL_PROMPT_ESC, fn, delim);
}
int shim_el_wset_rprompt_esc_fn(EditLine *el, shim_prompt_fn fn, wchar_t delim) {
    return el_wset(el, EL_RPROMPT_ESC, fn, delim);
}

// Register a named editor function (EL_ADDFN) backed by a Rust trampoline of
// type `unsigned char (*)(EditLine *, int)`, then bind a key sequence to it
// (EL_BIND). Used to wire Tab to the completion trampoline.
typedef unsigned char (*shim_el_fn)(EditLine *, int);
int shim_el_addfn(EditLine *el, const char *name, const char *help, shim_el_fn fn) {
    return el_set(el, EL_ADDFN, name, help, fn);
}
int shim_el_bind(EditLine *el, const char *key, const char *fnname) {
    return el_set(el, EL_BIND, key, fnname, (char *)NULL);
}

// Override libedit's get-character function (EL_GETCFN). The callback has
// signature `int (*)(EditLine *, wchar_t *)` (el_rfunc_t). libedit uses wide
// characters internally.
//
// We call el_wset directly (not el_set) to avoid the narrow-API wrapper
// (eln.c) which sets the NARROW_READ flag. That flag causes el_wgetc to
// truncate the wchar_t back to (signed) char after our trampoline returns,
// mangling any non-ASCII codepoint (e.g. U+00BF → U+FFBF).
typedef int (*shim_getcfn)(EditLine *, wchar_t *);
int shim_el_set_getcfn(EditLine *el, shim_getcfn fn) {
    return el_wset(el, EL_GETCFN, fn);
}

// Wire an EditLine to a HistoryW instance: el_wset(el, EL_HIST, history_w, h).
// Uses the wide history API so libedit does NOT set NARROW_HISTORY,
// eliminating a narrow<->wide conversion on every history lookup.
int shim_el_wset_hist(EditLine *el, HistoryW *h) {
    return el_wset(el, EL_HIST, history_w, h);
}

// Retrieve the FILE* for fd 0 (input), 1 (output), or 2 (error) via
// EL_GETFP. Returns 0 on success, -1 on error. The libedit call is
// variadic, so the wrapper lives here in C.
int shim_el_getfp(EditLine *el, int fd, FILE **fp) {
    return el_get(el, EL_GETFP, fd, fp);
}

// -- History shims (wide / HistoryW API) --

// Operations with no extra argument:
// H_FIRST, H_LAST, H_NEXT, H_PREV, H_CURR, H_GETSIZE, H_CLEAR
int shim_history_w_op(HistoryW *h, HistEventW *ev, int op) {
    return history_w(h, ev, op);
}

// Operations with one int argument:
// H_SETSIZE, H_SETUNIQUE, H_NEXT_EVENT, H_PREV_EVENT
int shim_history_w_op_int(HistoryW *h, HistEventW *ev, int op, int arg) {
    return history_w(h, ev, op, arg);
}

// Operations with one wide-string argument:
// H_ENTER
int shim_history_w_op_wstr(HistoryW *h, HistEventW *ev, int op, const wchar_t *s) {
    return history_w(h, ev, op, s);
}

// Operations with one narrow-string argument:
// H_LOAD, H_SAVE
int shim_history_w_op_str(HistoryW *h, HistEventW *ev, int op, const char *s) {
    return history_w(h, ev, op, s);
}
