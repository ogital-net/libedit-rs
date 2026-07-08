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
const char *shim_el_gets(EditLine *el, int *count, int *err) {
    errno = 0;
    const char *line = el_gets(el, count);
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

// Prompt callbacks. libedit's prompt ops expect a function pointer of type
// `char *(*)(EditLine *)`, NOT a string. Passing a string makes libedit call
// into the string's bytes as code -> segfault. The Rust side supplies a
// trampoline that returns the current prompt from client data.
//
// We use the ESC-aware ops (EL_PROMPT_ESC / EL_RPROMPT_ESC). The extra
// `delim` character marks "literal" (non-printing) regions: libedit ignores
// any bytes enclosed between two `delim` characters when computing the
// prompt's visible width. This lets the prompt/hint embed ANSI color escapes
// without corrupting cursor math. Same mechanism as readline's
// RL_PROMPT_START_IGNORE / RL_PROMPT_END_IGNORE.
typedef char *(*shim_prompt_fn)(EditLine *);
int shim_el_set_prompt_esc_fn(EditLine *el, shim_prompt_fn fn, char delim) {
    // Use el_wset (wide API) so p_wide=1 -- the trampoline returns wchar_t*
    // directly. This avoids ct_decode_string/mbstowcs and the locale
    // dependency that causes crashes with non-ASCII text in the C locale.
    return el_wset(el, EL_PROMPT_ESC, fn, (wchar_t)delim);
}
int shim_el_set_rprompt_esc_fn(EditLine *el, shim_prompt_fn fn, char delim) {
    return el_wset(el, EL_RPROMPT_ESC, fn, (wchar_t)delim);
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
typedef int (*shim_getcfn)(EditLine *, wchar_t *);
int shim_el_set_getcfn(EditLine *el, shim_getcfn fn) {
    return el_set(el, EL_GETCFN, fn);
}

// Wire an EditLine to a History instance: el_set(el, EL_HIST, history, h).
// The variadic `history` function pointer must be passed exactly as libedit
// expects, so this is done in C to avoid Rust variadic UB.
int shim_el_set_hist(EditLine *el, History *h) {
    return el_set(el, EL_HIST, history, h);
}

// -- History shims --

int shim_history_enter(History *h, HistEvent *ev, const char *s) {
    return history(h, ev, H_ENTER, s);
}

int shim_history_first(History *h, HistEvent *ev) {
    return history(h, ev, H_FIRST);
}

int shim_history_getsize(History *h, HistEvent *ev) {
    return history(h, ev, H_GETSIZE);
}

int shim_history_setsize(History *h, HistEvent *ev, int n) {
    return history(h, ev, H_SETSIZE, n);
}

// Toggle "unique" mode. When enabled, entering a line identical to the most
// recent entry is a no-op, so consecutive duplicates aren't stored.
int shim_history_setunique(History *h, HistEvent *ev, int unique) {
    return history(h, ev, H_SETUNIQUE, unique);
}

int shim_history_clear(History *h, HistEvent *ev) {
    return history(h, ev, H_CLEAR);
}

int shim_history_load(History *h, HistEvent *ev, const char *path) {
    return history(h, ev, H_LOAD, path);
}

int shim_history_save(History *h, HistEvent *ev, const char *path) {
    return history(h, ev, H_SAVE, path);
}
