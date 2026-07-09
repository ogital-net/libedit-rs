//! Safe wrapper around libedit's `EditLine` editor.

use libedit_sys::*;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_uchar};
use std::panic::{catch_unwind, AssertUnwindSafe};

use std::path::Path;

use crate::completion::{longest_common_prefix, CandidateStyler, Completer, LineContext};
use crate::error::{Error, Result};
use crate::hint::Hinter;
use crate::history::{path_to_cstring, History};
use crate::shim;
use crate::suggestion::Suggester;

/// Maximum number of user-defined actions that can be registered via
/// [`EditLine::add_action`]. Each slot uses a pre-generated `extern "C"`
/// trampoline (libedit requires a distinct function pointer per `EL_ADDFN`).
const MAX_USER_ACTIONS: usize = 8;

/// Delimiter byte marking non-printing (zero-width) regions of a prompt for
/// libedit's `EL_PROMPT_ESC` / `EL_RPROMPT_ESC` modes. Bytes enclosed between
/// two of these are excluded from the prompt's computed display width. We wrap
/// ANSI escape sequences in this delimiter so colored prompts and hints keep
/// the cursor math correct. `0x01` (SOH) matches readline's convention and is
/// vanishingly unlikely to appear in real prompt text.
#[allow(clippy::unnecessary_cast)]
const PROMPT_ESC_DELIM: c_char = 0x01;

/// Type alias for a boxed user-action handler stored in each action slot.
type ActionHandler = Box<dyn FnMut(&ActionContext) -> Action>;

/// Per-editor state that libedit callbacks reach via the editor's client
/// data. Heap-allocated and owned by the `EditLine` through a raw pointer so
/// its address is stable for the lifetime of the editor (the trampolines
/// recover `&mut Context` from the client-data pointer).
struct Context {
    /// The current prompt encoded as a NUL-terminated `wchar_t` array.
    /// The prompt trampoline returns a pointer into this buffer. Using wide
    /// chars (via `el_wset` with `p_wide=1`) avoids libedit's internal
    /// `mbstowcs` conversion, which fails on non-ASCII in the "C" locale.
    prompt_wide: Vec<wchar_t>,
    /// The user's completer, if one has been registered.
    completer: Option<Box<dyn Completer>>,
    /// The user's hinter, if one has been registered.
    hinter: Option<Box<dyn Hinter>>,
    /// Optional styler applied to each candidate before it is listed when
    /// completion is ambiguous. `None` lists candidates verbatim.
    candidate_styler: Option<Box<dyn CandidateStyler>>,
    /// Reusable scratch buffer for assembling the ambiguous-candidate listing,
    /// so repeated Tab presses don't reallocate.
    list_buf: String,
    /// libedit's output stream (`fout`, a `FILE*` over a dup of fd 1). The
    /// completion listing is written through this same stream libedit draws
    /// on, then flushed, so the two stay correctly ordered. Not owned here --
    /// it aliases `EditLine::streams[1]`, which is closed in `Drop`.
    out: *mut FILE,
    /// The raw file descriptor libedit reads input from -- `fileno` of the
    /// stdin `FILE*` handed to `el_init` (a *dup* of fd 0, not fd 0 itself).
    /// The suggestion get-character trampoline reads from this fd so it stays
    /// in lockstep with libedit's own `read(el_infd, ...)`, rather than
    /// assuming fd 0 (which may be reassigned after the editor is created).
    in_fd: i32,
    /// Backing storage for the current hint text as a NUL-terminated wchar_t
    /// array. The right-prompt trampoline returns a pointer into this buffer.
    /// Using wide chars avoids locale-dependent mbstowcs conversion.
    hint_wide: Vec<wchar_t>,
    /// The user's inline autosuggestion source, if one has been registered.
    suggester: Option<Box<dyn Suggester>>,
    /// ANSI prefix/suffix wrapped around suggestion ghost text (e.g. a "faint"
    /// SGR pair). Empty by default; set via `set_suggestion_style`.
    suggest_prefix: String,
    suggest_suffix: String,
    /// Combined column width (line + suggestion) most recently drawn on the
    /// suggestion row, so the next keystroke can erase any leftover tail with
    /// spaces. Mirrors LLDB's `m_previous_autosuggestion_size`.
    prev_suggest_total: usize,
    /// Visible column width of the current prompt, computed in `readline`
    /// (excluding ANSI escapes wrapped in `PROMPT_ESC_DELIM`). Needed to place
    /// the cursor after drawing ghost text.
    prompt_cols: usize,
    /// Reusable scratch buffer for assembling the suggestion ghost output,
    /// so per-keystroke draws don't reallocate.
    suggest_buf: String,
    /// User-defined actions registered via [`EditLine::add_action`]. Indexed
    /// by the slot number encoded in the trampoline that libedit invokes.
    actions: Vec<ActionHandler>,
    /// When `true`, `readline` automatically adds non-empty lines to the
    /// attached history. Requires a history pointer stored via `set_history`.
    auto_add_history: bool,
    /// When `true` (and `auto_add_history` is enabled), lines starting with a
    /// space are not added to history.
    ignore_space: bool,
    /// Raw history pointer for auto-add. Set by `set_history`.
    history_ptr: *mut libedit_sys::History,
}

/// The key-binding style used for line editing.
///
/// Corresponds to libedit's `EL_EDITOR` parameter and the `editor` line in
/// `.editrc`. The actual default depends on how the system's libedit was
/// compiled (see [`EditLine::set_editor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    /// Emacs-style key bindings (Ctrl-A/E/F/B, etc.). Historically libedit's
    /// default, but not guaranteed on all distributions.
    Emacs,
    /// vi-style key bindings, with distinct insert and command modes.
    Vi,
}

/// A libedit line editor instance.
///
/// This wraps the C `EditLine` pointer and manages its lifecycle.
/// The editor is not `Send` or `Sync` -- libedit uses global signal
/// handlers and static state internally.
pub struct EditLine {
    inner: *mut libedit_sys::EditLine,
    // Heap-stable per-editor context. Stored as a raw pointer (via
    // `Box::into_raw`) rather than a `Box` field so the trampolines can form
    // `&mut Context` from the client-data pointer without aliasing a Rust
    // reference held elsewhere. Freed in `Drop`.
    context: *mut Context,
    // `FILE*` streams handed to `el_init`. Each wraps a *duplicated* copy of
    // fd 0/1/2 (via `dup`), so closing them in `Drop` frees the stream
    // buffers and the duplicated descriptors without touching the process's
    // real standard streams.
    streams: [*mut FILE; 3],
}

// SAFETY: We ensure single-threaded access at the application level.
// libedit itself is not thread-safe due to global signal handlers.

impl EditLine {
    /// Create a new editline editor instance.
    ///
    /// `app_name` is used for configuration file lookup (e.g., `.editrc`).
    ///
    /// **Note:** If your prompts or hints contain non-ASCII characters, call
    /// [`EditLine::new_with_locale`] instead, or ensure `setlocale(LC_CTYPE, "")`
    /// has been called before the first [`readline`](Self::readline). Without a
    /// UTF-8 locale, libedit cannot output characters outside ASCII.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `app_name` contains an interior NUL byte,
    /// or [`Error::Null`] if opening the standard streams or initializing
    /// libedit fails (for example, when stdin/stdout/stderr are not available).
    pub fn new(app_name: impl AsRef<str>) -> Result<Self> {
        let name = CString::new(app_name.as_ref())?;

        // Open independent `FILE*` streams over duplicated copies of the
        // standard fds. `fdopen` does not dup, so we must `dup` first;
        // otherwise closing these streams would close fds 0/1/2.
        let stdin = unsafe { open_std_stream(0, c"r") };
        let stdout = unsafe { open_std_stream(1, c"w") };
        let stderr = unsafe { open_std_stream(2, c"w") };
        let streams = [stdin, stdout, stderr];

        // If any stream failed to open, clean up the ones that succeeded.
        if stdin.is_null() || stdout.is_null() || stderr.is_null() {
            unsafe { close_streams(&streams) };
            return Err(Error::Null);
        }

        let inner = unsafe { el_init(name.as_ptr(), stdin, stdout, stderr) };

        if inner.is_null() {
            unsafe { close_streams(&streams) };
            return Err(Error::Null);
        }

        // Recover the descriptor libedit will read from: `fileno` of the
        // stdin stream we just handed it. This is the dup of fd 0 created by
        // `open_std_stream`, and it is what libedit's `read(el_infd, ...)`
        // uses -- so the suggestion trampoline must read from the same fd.
        let in_fd = unsafe { libc::fileno(stdin as *mut libc::FILE) };

        // Allocate the context on the heap and register it as client data so
        // the prompt/completion trampolines can find it. `default_prompt`
        // starts empty; `readline` overwrites it before each call.
        let context = Box::into_raw(Box::new(Context {
            prompt_wide: vec![0], // NUL-terminated empty wide string
            completer: None,
            hinter: None,
            candidate_styler: None,
            list_buf: String::new(),
            out: stdout,
            in_fd,
            hint_wide: vec![0], // NUL-terminated empty wide string
            suggester: None,
            suggest_prefix: String::new(),
            suggest_suffix: String::new(),
            prev_suggest_total: 0,
            prompt_cols: 0,
            suggest_buf: String::new(),
            actions: Vec::new(),
            auto_add_history: false,
            ignore_space: false,
            history_ptr: std::ptr::null_mut(),
        }));
        unsafe {
            shim::el_set_clientdata(inner, context as *mut c_void);
            // Register the prompt trampoline once, in ESC-aware mode so ANSI
            // color escapes wrapped in `PROMPT_ESC_DELIM` are not counted
            // toward the prompt width. It reads the current prompt from the
            // context on demand.
            shim::el_set_prompt_esc_fn(inner, prompt_trampoline, PROMPT_ESC_DELIM);
            // Register the right-prompt (hint) trampoline once, likewise in
            // ESC-aware mode. It is a no-op until a hinter is registered via
            // `set_hinter`.
            shim::el_set_rprompt_esc_fn(inner, rprompt_trampoline, PROMPT_ESC_DELIM);
        }

        Ok(EditLine {
            inner,
            context,
            streams,
        })
    }

    /// Create a new editline editor instance, ensuring the process locale
    /// supports UTF-8 output.
    ///
    /// This is equivalent to [`new`](Self::new) but first calls
    /// `setlocale(LC_CTYPE, "")` (once, via an internal guard) so that
    /// libedit can render non-ASCII characters in prompts, hints, and
    /// completions. This is the recommended constructor for interactive CLIs
    /// that may display Unicode text.
    ///
    /// The locale is set at most once per process, regardless of how many
    /// editors are created.
    ///
    /// # Errors
    ///
    /// See [`new`](Self::new) for the possible error conditions.
    pub fn new_with_locale(app_name: impl AsRef<str>) -> Result<Self> {
        use std::sync::Once;
        static LOCALE_INIT: Once = Once::new();
        LOCALE_INIT.call_once(|| unsafe {
            libc::setlocale(libc::LC_CTYPE, c"".as_ptr());
        });
        Self::new(app_name)
    }

    /// Read a line from the user, displaying the given prompt.
    ///
    /// Returns `Ok(None)` on end-of-file (e.g. the user pressed Ctrl-D on an
    /// empty line).
    ///
    /// The returned line has its trailing newline removed. Input that is not
    /// valid UTF-8 is converted lossily (invalid sequences become the U+FFFD
    /// replacement character). If you need byte-exact input, use
    /// [`readline_bytes`](Self::readline_bytes).
    ///
    /// # Colored prompts
    ///
    /// The prompt may contain ANSI color escape sequences; they are
    /// automatically marked as non-printing so libedit's cursor positioning
    /// stays correct. **The prompt must end with a visible character**
    /// (typically a space) *after* any trailing reset sequence. For example,
    /// use `"\x1b[1;32m>\x1b[0m "` (space after `\x1b[0m`), not
    /// `"\x1b[1;32m> \x1b[0m"` (space before it). libedit silently drops
    /// the last escape if no printable character follows it, causing the
    /// color to leak into typed text.
    ///
    /// On macOS, Apple ships a 2012-era libedit that predates the
    /// `re_putliteral` mechanism for rendering escape sequences within the
    /// prompt. ANSI escapes are correctly excluded from the width calculation
    /// (cursor positioning is accurate), but they are not emitted to the
    /// terminal, so the prompt appears uncolored. Colored prompts work
    /// correctly on Linux and modern BSD systems.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Interrupted`] if the read was interrupted by a
    /// signal such as Ctrl-C (only when signal handling is enabled via
    /// [`set_signal_handling`](Self::set_signal_handling)).
    pub fn readline(&mut self, prompt: impl AsRef<str>) -> Result<Option<String>> {
        Ok(self
            .readline_bytes(prompt)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Read a line from the user as raw bytes, displaying the given prompt.
    ///
    /// Behaves identically to [`readline`](Self::readline) except the bytes
    /// are returned exactly as libedit produced them, with no UTF-8
    /// validation or lossy conversion. See [`readline`](Self::readline) for
    /// full documentation on prompt formatting, colored prompts, and error
    /// conditions.
    pub fn readline_bytes(&mut self, prompt: impl AsRef<str>) -> Result<Option<Vec<u8>>> {
        // Store the prompt in the context as a wide (wchar_t) string. The
        // prompt trampoline (registered once in `new` via el_wset with
        // p_wide=1) returns a pointer into this buffer during `el_gets`.
        // Using wide strings avoids libedit's internal mbstowcs which fails
        // on non-ASCII in the "C" locale.
        //
        // SAFETY: `self.context` was created in `new` via `Box::into_raw` and
        // is only mutated here and freed in `Drop`; no other reference to it
        // is live at this point.
        // Precompute the prompt's visible column width for the suggestion
        // renderer, and reset per-line suggestion state so a leftover width
        // from a previous line can't cause a stray erase.
        let prompt_cols = display_width(prompt.as_ref());
        unsafe {
            (*self.context).prompt_wide = prompt_to_wide(prompt.as_ref());
            (*self.context).prompt_cols = prompt_cols;
            (*self.context).prev_suggest_total = 0;
        }

        let mut count: i32 = 0;
        let mut err: i32 = 0;
        let line_ptr = unsafe { shim::el_gets_err(self.inner, &mut count, &mut err) };

        if line_ptr.is_null() || count < 0 {
            // libedit returns NULL for both EOF (Ctrl-D) and a signal-
            // interrupted read (Ctrl-C, with EL_SIGNAL enabled). Only errno
            // distinguishes them: a signal leaves errno == EINTR.
            if err == libc::EINTR {
                return Err(Error::Interrupted);
            }
            return Ok(None); // EOF
        }

        if count == 0 {
            return Ok(Some(Vec::new()));
        }

        let cstr = unsafe { CStr::from_ptr(line_ptr) };
        let mut bytes = cstr.to_bytes().to_vec();
        // el_gets includes the trailing newline; strip a single one.
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }

        // Auto-add to history if enabled.
        self.maybe_auto_add_history(&bytes);

        Ok(Some(bytes))
    }

    /// Conditionally add a line to the attached history (for auto_add_history).
    fn maybe_auto_add_history(&self, bytes: &[u8]) {
        let ctx = unsafe { &*self.context };
        if !ctx.auto_add_history || ctx.history_ptr.is_null() || bytes.is_empty() {
            return;
        }
        // Skip lines starting with a space when ignore_space is enabled.
        if ctx.ignore_space && bytes.first() == Some(&b' ') {
            return;
        }
        // Convert to a C string for the history API. Lines with interior NULs
        // are silently skipped (unlikely in practice).
        if let Ok(cstr) = CString::new(bytes) {
            unsafe { shim::history_enter(ctx.history_ptr, &cstr) };
        }
    }

    /// Attach a [`History`] to this editor so that up/down arrow recall and
    /// other history bindings work during [`readline`](Self::readline).
    ///
    /// The `History` must remain alive (not dropped) for as long as this
    /// editor is used, since libedit stores a pointer to it internally.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects the history
    /// attachment.
    pub fn set_history(&mut self, history: &mut History) -> Result<()> {
        let ret = unsafe { shim::el_set_hist(self.inner, history.as_mut_ptr()) };
        if ret != 0 {
            return Err(Error::operation(EL_HIST as i32, ret));
        }
        // Store the raw pointer so auto_add_history can call H_ENTER later.
        unsafe {
            (*self.context).history_ptr = history.as_mut_ptr();
        }
        Ok(())
    }

    /// Enable or disable automatic history addition.
    ///
    /// When enabled, each non-empty line returned by
    /// [`readline`](Self::readline) is automatically added to the attached
    /// [`History`]. This removes the need to call `history.add(line)` manually
    /// after every readline. A history must be attached via
    /// [`set_history`](Self::set_history) for this to take effect.
    ///
    /// Disabled by default.
    pub fn set_auto_add_history(&mut self, enabled: bool) {
        unsafe {
            (*self.context).auto_add_history = enabled;
        }
    }

    /// Enable or disable skipping lines that start with a space in auto-add.
    ///
    /// When enabled (and [`set_auto_add_history`](Self::set_auto_add_history)
    /// is active), lines whose first character is an ASCII space are not added
    /// to history. This is equivalent to bash/zsh `HIST_IGNORE_SPACE`.
    ///
    /// Disabled by default.
    pub fn set_history_ignore_space(&mut self, enabled: bool) {
        unsafe {
            (*self.context).ignore_space = enabled;
        }
    }

    /// Register a tab-completion handler.
    ///
    /// The `completer` is invoked when the user presses the completion key
    /// (Tab) during [`readline`](Self::readline). See the
    /// [`completion`](crate::completion) module for the completer contract
    /// and behavior. Any previously registered completer is replaced.
    ///
    /// A bare closure `FnMut(&LineContext) -> Completion` also implements
    /// [`Completer`] and can be passed directly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit fails to register or bind
    /// the completion handler (unlikely in practice).
    pub fn set_completer<C: Completer + 'static>(&mut self, completer: C) -> Result<()> {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).completer = Some(Box::new(completer));
        }
        // Register and bind the completion trampoline to Tab. Idempotent to
        // call more than once (EL_ADDFN/EL_BIND simply re-register).
        let name = c"ed-complete";
        let help = c"rust completion handler";
        let key = c"\t";
        let rc = unsafe { shim::el_addfn_bind(self.inner, name, help, key, completion_trampoline) };
        if rc != 0 {
            return Err(Error::operation(EL_ADDFN as i32, rc));
        }
        Ok(())
    }

    /// Register an inline hint handler.
    ///
    /// The `hinter` is invoked on every keystroke during
    /// [`readline`](Self::readline) and its result is rendered to the right of
    /// the input line (see the [`hint`](crate::hint) module for details on
    /// positioning). Any previously registered hinter is replaced.
    ///
    /// A bare closure `FnMut(&LineContext) -> Option<Hint>` also implements
    /// [`Hinter`] and can be passed directly.
    ///
    /// # Choosing between a hinter and a suggester
    ///
    /// A hint renders at the **right edge** of the terminal line via libedit's
    /// right-hand prompt, so it reads as a persistent status or help string
    /// (e.g. "-- show stored history"), not as text spliced in at the cursor.
    /// For fish-style ghost text that continues the line immediately after the
    /// cursor, use [`set_suggester`](Self::set_suggester) instead. The two are
    /// independent and may both be registered.
    pub fn set_hinter<H: Hinter + 'static>(&mut self, hinter: H) {
        // SAFETY: `self.context` is valid for the editor's lifetime. The
        // rprompt trampoline was registered in `new`.
        unsafe {
            (*self.context).hinter = Some(Box::new(hinter));
        }
    }

    /// Register an inline autosuggestion source (fish-style ghost text).
    ///
    /// The `suggester` is invoked on every keystroke during
    /// [`readline`](Self::readline); its suggested suffix is drawn dimmed
    /// immediately after the cursor and can be accepted with the accept key
    /// (Ctrl-F, or Right-arrow at end of line, by default). The suggestion is
    /// never part of the edit buffer, so Enter/End/kill-line won't capture it.
    /// See the [`suggestion`](crate::suggestion) module for the full contract.
    ///
    /// This installs a get-character hook (`EL_GETCFN`) and binds the accept
    /// keys (Ctrl-F and Right-arrow at EOL). Any previously registered
    /// suggester is replaced.
    ///
    /// A bare closure `FnMut(&LineContext) -> Option<Suggestion>` also
    /// implements [`Suggester`] and can be passed
    /// directly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit fails to install the
    /// get-character hook (`EL_GETCFN`) or register/ bind the accept keys.
    pub fn set_suggester<S: Suggester + 'static>(&mut self, suggester: S) -> Result<()> {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).suggester = Some(Box::new(suggester));
        }
        self.install_suggestion_bindings()
    }

    /// Set the ANSI escape sequences wrapped around suggestion ghost text.
    ///
    /// `prefix` is emitted before the suggestion and `suffix` after it -- for
    /// example `"\x1b[2m"` (faint) and `"\x1b[0m"` (reset). The crate supplies
    /// no styling of its own; pass empty strings for undecorated text. These
    /// escapes are display-only and never enter the edit buffer.
    pub fn set_suggestion_style(&mut self, prefix: impl Into<String>, suffix: impl Into<String>) {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).suggest_prefix = prefix.into();
            (*self.context).suggest_suffix = suffix.into();
        }
    }

    /// Remove the current inline suggester, disabling ghost text.
    ///
    /// The `EL_GETCFN` hook remains installed but becomes a transparent
    /// passthrough (read a byte, return it -- no drawing). This is effectively
    /// free.
    pub fn clear_suggester(&mut self) {
        unsafe {
            (*self.context).suggester = None;
            (*self.context).prev_suggest_total = 0;
        }
    }

    /// Register a custom key action and return its internal name for use with
    /// [`bind_key`](Self::bind_key).
    ///
    /// The closure receives an [`ActionContext`] with the current line and
    /// cursor, plus a [`Writer`] for output. It returns an [`Action`] that
    /// tells libedit how to proceed (redisplay, refresh, beep, etc.).
    ///
    /// Up to 8 user actions may be registered (limited by pre-generated
    /// trampolines). Exceeding this returns an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `name` contains an interior NUL byte, or
    /// [`Error::Operation`] if the action slot limit has been reached or
    /// libedit fails to register the handler.
    ///
    /// # Example: Juniper-style `?` context help
    ///
    /// ```no_run
    /// # use libedit::{EditLine, Action, ActionContext};
    /// use std::io::Write;
    /// let mut el = EditLine::new("cli").unwrap();
    /// let name = el.add_action("cli-help", |ctx: &ActionContext| {
    ///     let mut out = ctx.output();
    ///     writeln!(out, "\nPossible completions:").unwrap();
    ///     writeln!(out, "  show    Display state").unwrap();
    ///     out.flush().unwrap();
    ///     Action::Redisplay
    /// }).unwrap();
    /// el.bind_key("?", &name).unwrap();
    /// ```
    pub fn add_action<F>(&mut self, name: &str, handler: F) -> Result<String>
    where
        F: FnMut(&ActionContext) -> Action + 'static,
    {
        let slot = unsafe { (*self.context).actions.len() };
        if slot >= MAX_USER_ACTIONS {
            return Err(Error::operation(EL_ADDFN as i32, -1));
        }
        unsafe {
            (*self.context).actions.push(Box::new(handler));
        }
        // The internal name encodes the slot so bind_key can reference it.
        let internal_name = format!("led-user-{slot}");
        let c_name = CString::new(internal_name.as_str())?;
        let c_help = CString::new(format!("user action: {name}"))?;
        let trampoline = ACTION_TRAMPOLINES[slot];
        let rc = unsafe { shim::el_addfn(self.inner, &c_name, &c_help, trampoline) };
        if rc != 0 {
            // Roll back the push.
            unsafe {
                (*self.context).actions.pop();
            }
            return Err(Error::operation(EL_ADDFN as i32, rc));
        }
        Ok(internal_name)
    }

    /// Bind a key sequence to a named libedit editor function.
    ///
    /// `key` is a libedit key specification (e.g. `"^R"`, `"\\e[A"`, `"\t"`)
    /// and `fn_name` is a built-in editor command (such as
    /// `"em-inc-search-prev"` for reverse history search) or a function
    /// previously registered by this crate. This is an escape hatch for
    /// customizing the keymap beyond the defaults; most callers won't need it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `key` or `fn_name` contains an interior
    /// NUL byte, or [`Error::Operation`] if libedit rejects the binding
    /// (e.g. the function name is not recognised).
    pub fn bind_key(&mut self, key: &str, fn_name: &str) -> Result<()> {
        let key_c = CString::new(key)?;
        let fn_c = CString::new(fn_name)?;
        let rc = unsafe { shim::el_bind(self.inner, &key_c, &fn_c) };
        if rc != 0 {
            return Err(Error::operation(EL_BIND as i32, rc));
        }
        Ok(())
    }

    /// Install the `EL_GETCFN` get-character wrapper and the accept key
    /// binding. The get-char hook draws the suggestion ghost *after* libedit's
    /// refresh on the previous keystroke, so it's synchronized with libedit's
    /// display model regardless of which key was pressed (typing, backspace,
    /// Tab, etc.).
    fn install_suggestion_bindings(&mut self) -> Result<()> {
        // Install our get-character trampoline. This is the core of the
        // suggestion architecture: it replaces libedit's default stdin read,
        // draws the ghost at the right moment, then reads and returns the next
        // byte. Transparent passthrough when no suggester is registered.
        let rc = unsafe { shim::el_set_getcfn(self.inner, getcfn_trampoline) };
        if rc != 0 {
            return Err(Error::operation(EL_GETCFN as i32, rc));
        }

        // Register and bind the accept key (Ctrl-F, Right-arrow at EOL).
        let apply = c"led-suggest-apply";
        let rc = unsafe {
            shim::el_addfn(
                self.inner,
                apply,
                c"accept inline suggestion",
                suggest_apply_trampoline,
            )
        };
        if rc != 0 {
            return Err(Error::operation(EL_ADDFN as i32, rc));
        }
        // Only bind Ctrl-F to suggestion-accept. Right-arrow (`\e[C`) is
        // intentionally left on libedit's native `ed-next-char` so that
        // cursor movement is handled by libedit itself -- this keeps behavior
        // consistent across platforms (Apple's older libedit and modern
        // Linux/BSD libedit have the same `ed-next-char` semantics at the
        // key-binding level) and avoids reimplementing cursor-motion logic
        // that would need access to private internals.
        let rc = unsafe { shim::el_bind(self.inner, c"^F", apply) };
        if rc != 0 {
            return Err(Error::operation(EL_BIND as i32, rc));
        }
        Ok(())
    }

    /// Register a styler for the candidate list shown when a Tab completion is
    /// ambiguous (more than one candidate and no further common prefix).
    ///
    /// The styler is called once per candidate and *appends* the display text
    /// for it into the provided buffer. This crate applies no ANSI styling of
    /// its own; whatever is appended is written verbatim, so the consumer is
    /// free to wrap candidates in color escapes, add annotations, pad columns,
    /// etc. Appending (rather than returning a `String`) lets callers use
    /// `write!` into the shared buffer and avoids a per-candidate allocation.
    /// Without a styler, candidates are listed unchanged.
    ///
    /// The styling affects *display* only; the value inserted into the line on
    /// a unique/prefix match is always the raw candidate.
    ///
    /// A bare closure `FnMut(&str, &mut String)` also implements
    /// [`CandidateStyler`] and can be passed directly.
    pub fn set_candidate_styler<S: CandidateStyler + 'static>(&mut self, styler: S) {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).candidate_styler = Some(Box::new(styler));
        }
    }

    /// Enable or disable signal handling by libedit.
    ///
    /// When enabled, libedit installs handlers for signals such as
    /// `SIGWINCH` so the display stays correct across terminal resizes.
    ///
    /// # Recovering from Ctrl-C (`SIGINT`)
    ///
    /// Enabling this is **not sufficient on its own** to make Ctrl-C return
    /// [`Error::Interrupted`] from [`readline`](Self::readline). While a line
    /// is being read, libedit traps `SIGINT`, restores whatever `SIGINT`
    /// disposition existed *before* the read, and then re-raises the signal.
    /// If the application never installed one, that prior disposition is the
    /// default (`SIG_DFL`), so the re-raised `SIGINT` **terminates the
    /// process** before `readline` can return -- Ctrl-C kills the program
    /// instead of surfacing as an error.
    ///
    /// To make Ctrl-C recoverable, the application must install its own
    /// non-terminating `SIGINT` handler (it may be a no-op) **before** reading,
    /// and crucially install it *without* `SA_RESTART`. With `SA_RESTART` the
    /// interrupted `read(2)` is automatically restarted and never reports
    /// `EINTR`, so libedit cannot detect the interruption. Installed correctly
    /// (e.g. via `sigaction` with `sa_flags == 0`), the re-raised signal runs
    /// the handler, the read fails with `EINTR`, and `readline` returns
    /// [`Error::Interrupted`]. See the `repl` example for a complete
    /// implementation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects the parameter change.
    pub fn set_signal_handling(&mut self, enabled: bool) -> Result<()> {
        self.set_int(EL_SIGNAL as i32, enabled as i32)
    }

    /// Ring the terminal bell.
    ///
    /// Useful from a completer or custom action to signal "no match" without
    /// printing anything.
    pub fn beep(&mut self) {
        unsafe { el_beep(self.inner) };
    }

    /// Query the current terminal dimensions `(columns, rows)`.
    ///
    /// Uses `ioctl(TIOCGWINSZ)` on stdout. Returns `(80, 24)` as a fallback
    /// if the query fails (e.g. when stdout is not a terminal).
    pub fn terminal_size(&self) -> (usize, usize) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
                (ws.ws_col as usize, ws.ws_row as usize)
            } else {
                (80, 24)
            }
        }
    }

    /// Select the key-binding style ([`Editor::Emacs`] or [`Editor::Vi`]).
    ///
    /// # Important: no guaranteed default
    ///
    /// libedit's default editor mode is a **compile-time decision** by the
    /// system packager (`#ifdef VIDEFAULT` in libedit's `map.c`). Some
    /// distributions (notably Debian/Ubuntu) compile with `VIDEFAULT`,
    /// making vi mode the default, which has fundamentally different cursor
    /// semantics and key bindings from emacs mode. The user's `~/.editrc`
    /// can also override the mode at runtime.
    ///
    /// **Applications that depend on a specific editing style should call
    /// this method explicitly** rather than assuming the default. For example,
    /// a CLI that expects Ctrl-A/Ctrl-E/Ctrl-F should call `set_editor(Editor::Emacs)`
    /// after [`new`](Self::new).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects the editor mode change.
    pub fn set_editor(&mut self, editor: Editor) -> Result<()> {
        let name = match editor {
            Editor::Emacs => c"emacs",
            Editor::Vi => c"vi",
        };
        let rc = unsafe { shim::el_set_editor(self.inner, name.as_ptr()) };
        if rc != 0 {
            return Err(Error::operation(EL_EDITOR as i32, rc));
        }
        Ok(())
    }

    /// Read editor configuration from an `.editrc`-style file.
    ///
    /// With `path = None`, libedit loads the user's default configuration
    /// (`$EDITRC`, or `~/.editrc`), keyed by the `app_name` given to
    /// [`new`](Self::new). With `path = Some(..)`, that specific file is read.
    /// Applies key bindings, the editor mode, and other `bind`/`setty`
    /// directives, so call it after [`new`](Self::new) but before the first
    /// [`readline`](Self::readline).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `path` contains an interior NUL byte, or
    /// [`Error::Operation`] if the file cannot be read or parsed.
    pub fn source_config(&mut self, path: Option<&Path>) -> Result<()> {
        let rc = match path {
            Some(p) => {
                let c_path = path_to_cstring(p)?;
                unsafe { el_source(self.inner, c_path.as_ptr()) }
            }
            None => unsafe { el_source(self.inner, std::ptr::null()) },
        };
        if rc != 0 {
            return Err(Error::operation(0, rc));
        }
        Ok(())
    }

    /// Select the editing mode: `true` for interactive line editing (the
    /// default), `false` to disable it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects the parameter change.
    pub fn set_edit_mode(&mut self, enabled: bool) -> Result<()> {
        self.set_int(EL_EDITMODE as i32, enabled as i32)
    }

    /// Returns `true` if interactive line editing is currently enabled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit fails to query the parameter.
    pub fn edit_mode(&self) -> Result<bool> {
        Ok(self.get_int(EL_EDITMODE as i32)? != 0)
    }

    /// Set an integer-valued editor parameter.
    ///
    /// `op` is a libedit `EL_*` operation code (re-exported at the crate
    /// root). Prefer the typed helpers such as
    /// [`set_edit_mode`](Self::set_edit_mode) where available; this is an
    /// escape hatch for parameters without a dedicated method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] with the operation code and return value
    /// if libedit rejects the parameter.
    pub fn set_int(&mut self, op: i32, val: i32) -> Result<()> {
        let ret = unsafe { shim::el_set_int(self.inner, op, val) };
        if ret != 0 {
            return Err(Error::operation(op, ret));
        }
        Ok(())
    }

    /// Get an integer-valued editor parameter.
    ///
    /// `op` is a libedit `EL_*` operation code (re-exported at the crate
    /// root). Prefer the typed helpers where available.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit fails to query the parameter.
    pub fn get_int(&self, op: i32) -> Result<i32> {
        unsafe { shim::el_get_int(self.inner, op) }.ok_or_else(|| Error::operation(op, -1))
    }

    /// A [`std::io::Write`] handle to libedit's output stream.
    ///
    /// This is the standard, correctly-ordered way to emit text from a CLI
    /// built on libedit: it writes through the very same `FILE*` libedit uses
    /// to draw the prompt and line, so your output and libedit's redraws share
    /// one stdio buffer and never interleave out of order. Prefer this over
    /// `println!`/`print!` for anything emitted while a line is being edited
    /// (e.g. from a completer or a custom key action).
    ///
    /// The handle borrows the editor, so it cannot outlive it. Writes are
    /// buffered by stdio; call [`flush`](std::io::Write::flush) (or drop the
    /// handle and rely on libedit's own flushing) to force them out.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use libedit::EditLine;
    /// use std::io::Write;
    /// let mut el = EditLine::new("cli").unwrap();
    /// let mut out = el.output();
    /// writeln!(out, "status: {}", 42).unwrap();
    /// out.flush().unwrap();
    /// ```
    pub fn output(&mut self) -> Writer<'_> {
        // SAFETY: `streams[1]` is libedit's output `FILE*`, valid for the
        // editor's lifetime. The returned `Writer` borrows `self`, so it can't
        // outlive the stream.
        Writer {
            stream: self.streams[1],
            _marker: std::marker::PhantomData,
        }
    }

    /// A [`std::io::Write`] handle to libedit's error stream.
    ///
    /// Like [`output`](Self::output) but targets the stream passed as
    /// `ferr` to libedit (a dup of fd 2). Use it for diagnostics that should
    /// go to standard error while still staying ordered with libedit's
    /// drawing.
    pub fn error_output(&mut self) -> Writer<'_> {
        // SAFETY: as `output`, but for the error stream (`streams[2]`).
        Writer {
            stream: self.streams[2],
            _marker: std::marker::PhantomData,
        }
    }

    /// Return a raw pointer to the underlying `EditLine`.
    ///
    /// # Safety
    /// The caller must not call `el_end` or otherwise free this pointer, and
    /// must not use it after this `EditLine` is dropped.
    pub unsafe fn as_ptr(&self) -> *mut libedit_sys::EditLine {
        self.inner
    }
}

/// A [`std::io::Write`] adapter over one of libedit's stdio output streams.
///
/// Created by [`EditLine::output`] and [`EditLine::error_output`]. Writing
/// through this handle (rather than a bare `write(2)` or Rust's `println!`)
/// keeps application output correctly ordered with libedit's prompt/line
/// redraws, because it shares the same underlying `FILE*` and its buffer.
///
/// The handle borrows the [`EditLine`] it came from and so cannot outlive it.
pub struct Writer<'a> {
    stream: *mut FILE,
    _marker: std::marker::PhantomData<&'a mut EditLine>,
}

impl std::fmt::Debug for Writer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("stream", &self.stream)
            .finish()
    }
}

impl std::io::Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(fwrite_bytes(self.stream, buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: `stream` is a live `FILE*` for the editor's lifetime.
        if !self.stream.is_null() {
            unsafe { libc::fflush(self.stream as *mut libc::FILE) };
        }
        Ok(())
    }
}

/// Write `bytes` to a libedit output `FILE*`, returning the number written.
///
/// Falls back to a raw `write(2)` on fd 2 if `stream` is null. Shared by the
/// [`Writer`] adapter and the internal candidate-listing path so both go
/// through the same, correctly-ordered stdio buffer.
fn fwrite_bytes(stream: *mut FILE, bytes: &[u8]) -> usize {
    if stream.is_null() {
        // SAFETY: best-effort write of a byte buffer to fd 2.
        let n = unsafe { libc::write(2, bytes.as_ptr() as *const c_void, bytes.len()) };
        return n.max(0) as usize;
    }
    // SAFETY: `stream` is libedit's live output `FILE*`, used single-threaded.
    // `fwrite` respects the stream's buffer so ordering with libedit's own
    // writes is preserved (flush to force it out).
    unsafe {
        libc::fwrite(
            bytes.as_ptr() as *const c_void,
            1,
            bytes.len(),
            stream as *mut libc::FILE,
        )
    }
}

impl Drop for EditLine {
    fn drop(&mut self) {
        // Tear down the editor first; it may flush to the streams and will no
        // longer invoke our trampolines afterward.
        unsafe { el_end(self.inner) };
        // Reclaim and drop the heap context (prompt, completer, hinter).
        // SAFETY: `context` came from `Box::into_raw` in `new` and is dropped
        // exactly once here.
        drop(unsafe { Box::from_raw(self.context) });
        // Then close our duplicated streams, freeing their buffers and fds.
        unsafe { close_streams(&self.streams) };
    }
}

/// Prompt trampoline registered with libedit's `EL_PROMPT_ESC` via `el_wset`
/// (wide mode, `p_wide=1`). Returns a `wchar_t*` to the context's wide prompt
/// buffer -- cast to `*mut c_char` to satisfy the `el_pfunc_t` typedef, but
/// libedit treats it as `wchar_t*` when `p_wide=1`.
extern "C" fn prompt_trampoline(el: *mut libedit_sys::EditLine) -> *mut c_char {
    static EMPTY: wchar_t = 0;
    let ctx = match context_from(el) {
        Some(c) => c,
        None => return &EMPTY as *const wchar_t as *mut c_char,
    };
    ctx.prompt_wide.as_ptr() as *mut c_char
}

/// Right-prompt (hint) trampoline registered with libedit's `EL_RPROMPT_ESC`
/// via `el_wset` (wide mode, `p_wide=1`). Returns `wchar_t*` directly --
/// no locale-dependent `mbstowcs` conversion needed.
///
/// libedit calls this on every redraw. We recover the context, run the
/// hinter against the current line, store the result in `hint_wide` (so the
/// returned pointer stays valid), and return it. Panics are contained so they
/// never unwind into C.
extern "C" fn rprompt_trampoline(el: *mut libedit_sys::EditLine) -> *mut c_char {
    static EMPTY: wchar_t = 0;
    let empty = &EMPTY as *const wchar_t as *mut c_char;

    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(ctx) = context_from(el) else {
            return empty;
        };
        let Some(hinter) = ctx.hinter.as_mut() else {
            return empty;
        };
        let info = unsafe { el_line(el) };
        if info.is_null() {
            return empty;
        }
        let (line, cursor) = unsafe { line_and_cursor(&*info) };
        let lc = LineContext::new(line, cursor);
        match hinter.hint(&lc) {
            Some(h) => {
                // Convert hint text to a wide string (wchar_t array).
                // Wrap ANSI escapes in the delimiter so libedit's
                // EL_RPROMPT_ESC width calculation ignores them.
                ctx.hint_wide = prompt_to_wide(h.text());
                ctx.hint_wide.as_ptr() as *mut c_char
            }
            None => {
                ctx.hint_wide = vec![0];
                empty
            }
        }
    }));
    result.unwrap_or(empty)
}

/// Completion trampoline registered via `EL_ADDFN` and bound to Tab.
///
/// libedit calls this with signature `unsigned char (*)(EditLine *, int)`.
/// We recover the context and current line, invoke the user's completer, and
/// apply the result (insert the longest common prefix, or list ambiguous
/// candidates), returning a `CC_*` redisplay code.
extern "C" fn completion_trampoline(el: *mut libedit_sys::EditLine, _c: i32) -> c_uchar {
    // Guard the entire FFI boundary against panics: unwinding into C is UB.
    let result = catch_unwind(AssertUnwindSafe(|| complete_impl(el)));
    result.unwrap_or(CC_ERROR as c_uchar)
}

fn complete_impl(el: *mut libedit_sys::EditLine) -> c_uchar {
    let Some(ctx) = context_from(el) else {
        return CC_ERROR as c_uchar;
    };
    // Erase any inline suggestion ghost text still on screen before we run
    // completion. The ghost was drawn past the cursor with a plain CC_NORM (so
    // libedit's model doesn't know those cells are painted); if we don't clear
    // it, the completion redraw collides with the stale pixels (e.g. `hi`+Tab
    // rendering `hiistory`). The physical cursor is at the real caret, so a
    // clear-to-end-of-line removes exactly the ghost.
    clear_ghost(ctx);
    let Some(completer) = ctx.completer.as_mut() else {
        return CC_ERROR as c_uchar;
    };

    // Read the current line and cursor from libedit.
    let info = unsafe { el_line(el) };
    if info.is_null() {
        return CC_ERROR as c_uchar;
    }
    let info = unsafe { &*info };
    // `buffer`..`lastchar` is the line; `cursor` points within it.
    let (line, cursor) = unsafe { line_and_cursor(info) };

    let lc = LineContext::new(line, cursor);
    let word = lc.word().to_string();
    let completion = completer.complete(&lc);

    if completion.is_empty() {
        return CC_REFRESH_BEEP as c_uchar;
    }

    let candidates = &completion.candidates;
    // Determine what to insert. If the completer provided a pre-computed
    // insertion (e.g., from a trie), use it directly. Otherwise compute the
    // longest common prefix of all candidates minus what's already typed.
    let suffix = if let Some(insertion) = &completion.insertion {
        // Pre-computed path: insert verbatim. If it's empty and there are
        // multiple candidates, fall through to listing.
        if insertion.is_empty() && candidates.len() > 1 {
            String::new()
        } else {
            insertion.clone()
        }
    } else {
        // Compute LCP from candidates.
        let lcp = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            longest_common_prefix(candidates)
        };
        if lcp.len() > word.len() && lcp.starts_with(&word) {
            let mut s = lcp[word.len()..].to_string();
            // A single exact match: append a space to move to the next token.
            if candidates.len() == 1 {
                s.push(' ');
            }
            s
        } else {
            String::new()
        }
    };

    if !suffix.is_empty() {
        if let Ok(cstr) = CString::new(suffix.as_str()) {
            unsafe { el_insertstr(el, cstr.as_ptr()) };
        }
        return CC_REFRESH as c_uchar;
    }

    // Ambiguous with no further common prefix: list the candidates. Assemble
    // the listing into the context's reusable buffer (no per-Tab realloc after
    // warmup), applying the consumer's styler for display only. The raw
    // candidate strings are what would be inserted on a unique match.
    if candidates.len() > 1 {
        let Context {
            list_buf,
            candidate_styler,
            out,
            ..
        } = ctx;
        list_buf.clear();
        list_buf.push('\n');
        for (i, cand) in candidates.iter().enumerate() {
            if i > 0 {
                list_buf.push_str("  ");
            }
            match candidate_styler {
                Some(styler) => styler.style(cand, list_buf),
                None => list_buf.push_str(cand),
            }
        }
        list_buf.push('\n');
        write_candidates(*out, list_buf);
        return CC_REDISPLAY as c_uchar;
    }

    CC_NORM as c_uchar
}

/// Get-character trampoline installed via `EL_GETCFN`. This is the core of the
/// inline suggestion architecture (mirrors LLDB's `GetCharacter`).
///
/// libedit's cycle: call get-char -> process key -> refresh display -> call
/// get-char again. By drawing the ghost **at the start of each get-char call**
/// (i.e. *after* libedit's previous refresh), we guarantee our drawing sits on
/// top of libedit's clean, fully-consistent display. No manual echo, no return-
/// code battles.
///
/// When no suggester is registered this is a transparent passthrough: read a
/// character from libedit's input fd and return it.
///
/// # Character decoding
///
/// libedit's `el_rfunc_t` contract is *one complete `wchar_t` per call* (see
/// `read_char` in libedit's `read.c`, which loops on `mbrtowc` internally and
/// only returns once a full character has been assembled). We mirror that: a
/// single call reads the UTF-8 lead byte, then the exact number of
/// continuation bytes the sequence requires, and decodes them to one Unicode
/// scalar. Because a partial sequence never escapes the call, no cross-call
/// state is needed. Decoding is done directly (not via `mbrtowc`), so it is
/// independent of `LC_CTYPE` -- matching the wide strings we hand libedit for
/// prompts/hints.
///
/// # Input descriptor
///
/// Bytes are read from the editor's own input fd (`ctx.in_fd` -- `fileno` of
/// the stdin stream passed to `el_init`, a dup of fd 0), *not* a hard-coded
/// fd 0. This keeps us reading from exactly the descriptor libedit's builtin
/// `read(el_infd, ...)` uses, so the two stay consistent even if fd 0 is later
/// reassigned. Like libedit, we read the fd directly rather than through the
/// stdio `FILE*` buffer. Falls back to fd 0 only if the context is somehow
/// unavailable.
unsafe extern "C" fn getcfn_trampoline(el: *mut libedit_sys::EditLine, out: *mut wchar_t) -> i32 {
    // Draw suggestion ghost text BEFORE blocking for the next byte. At this
    // point libedit has already refreshed its display for the previous
    // keystroke (if any), so the line on screen is up-to-date and we can
    // safely draw past it.
    let result = catch_unwind(AssertUnwindSafe(|| draw_suggestion_ghost(el)));
    let _ = result; // ignore panic -- just skip the ghost

    // Resolve libedit's input fd from the context; fall back to fd 0.
    let in_fd = context_from(el).map_or(0, |ctx| ctx.in_fd);

    // Read the lead byte. This blocks until input arrives.
    let mut lead: u8 = 0;
    let n = unsafe { libc::read(in_fd, &mut lead as *mut u8 as *mut c_void, 1) };
    if n <= 0 {
        return if n == 0 { 0 } else { -1 }; // EOF or error
    }

    // Determine how many bytes this UTF-8 sequence occupies from the lead
    // byte. ASCII (and, defensively, any stray continuation/invalid lead byte,
    // which `utf8_char_len` maps to 1) takes the fast path and returns as-is.
    let total = utf8_char_len(lead);
    if total == 1 {
        unsafe { *out = lead as wchar_t };
        return 1;
    }

    // Multi-byte: read the remaining continuation bytes and decode. We read
    // one at a time so we never block waiting for bytes beyond this character.
    let mut buf = [0u8; 4];
    buf[0] = lead;
    for slot in buf.iter_mut().take(total).skip(1) {
        let mut b: u8 = 0;
        let n = unsafe { libc::read(in_fd, &mut b as *mut u8 as *mut c_void, 1) };
        if n <= 0 {
            return if n == 0 { 0 } else { -1 };
        }
        *slot = b;
    }

    // Decode the assembled bytes to a single scalar value. On invalid UTF-8,
    // fall back to the U+FFFD replacement character rather than failing the
    // read, so a stray byte can't wedge the editor.
    let cp = match std::str::from_utf8(&buf[..total]) {
        Ok(s) => s.chars().next().map(|c| c as u32).unwrap_or(0xFFFD),
        Err(_) => 0xFFFD,
    };
    unsafe { *out = cp as wchar_t };
    1
}

/// Draw the inline suggestion ghost text to the right of the cursor.
///
/// Called at the start of each get-char invocation (after libedit's refresh).
/// Draws the styled suffix, erases any longer previous ghost with spaces, then
/// repositions the cursor to the real caret column with CSI G. No-op when no
/// suggester is registered or the cursor isn't at end-of-line.
fn draw_suggestion_ghost(el: *mut libedit_sys::EditLine) {
    let Some(ctx) = context_from(el) else { return };
    if ctx.suggester.is_none() || ctx.out.is_null() {
        return;
    }

    let info = unsafe { el_line(el) };
    if info.is_null() {
        return;
    }
    let (line, cursor) = unsafe { line_and_cursor(&*info) };
    // Only show ghost when cursor is at the end of the line.
    let at_end = cursor >= line.len();

    let suggestion = if at_end && !line.is_empty() {
        ctx.suggester.as_mut().and_then(|s| {
            let lc = LineContext::new(line.clone(), cursor);
            s.suggest(&lc)
        })
    } else {
        None
    };

    let sug_text = match &suggestion {
        Some(s) if !s.is_empty() => s.text(),
        _ => {
            // No suggestion: erase any previous ghost and done.
            if ctx.prev_suggest_total > 0 {
                clear_ghost(ctx);
            }
            return;
        }
    };

    let stream = ctx.out;
    // Reuse the context's scratch buffer to avoid per-keystroke allocation.
    ctx.suggest_buf.clear();
    ctx.suggest_buf.push_str(&ctx.suggest_prefix);
    ctx.suggest_buf.push_str(sug_text);
    ctx.suggest_buf.push_str(&ctx.suggest_suffix);

    // Erase any tail from a previously longer suggestion.
    let new_total = line.len() + sug_text.len();
    if new_total < ctx.prev_suggest_total {
        for _ in 0..(ctx.prev_suggest_total - new_total) {
            ctx.suggest_buf.push(' ');
        }
    }
    ctx.prev_suggest_total = new_total;

    // Move cursor back to the real caret column.
    let term_cols = terminal_width(el).max(1);
    let caret_pos = ctx.prompt_cols + display_width(&line);
    let row = caret_pos / term_cols;
    let to_column = caret_pos - (row * term_cols);
    use std::fmt::Write;
    let _ = write!(ctx.suggest_buf, "\x1b[{}G", to_column + 1);

    fwrite_bytes(stream, ctx.suggest_buf.as_bytes());
    unsafe { libc::fflush(stream as *mut libc::FILE) };
}

/// Accept trampoline: commit the current suggestion into the buffer. Bound to
/// Ctrl-F and Right-arrow. If there's no suggestion (or the cursor isn't at end
/// of line for Right-arrow semantics), fall through to a cursor move.
extern "C" fn suggest_apply_trampoline(el: *mut libedit_sys::EditLine, _ch: i32) -> c_uchar {
    let result = catch_unwind(AssertUnwindSafe(|| suggest_apply_impl(el)));
    result.unwrap_or(CC_ERROR as c_uchar)
}

fn suggest_apply_impl(el: *mut libedit_sys::EditLine) -> c_uchar {
    let Some(ctx) = context_from(el) else {
        return forward_char(el);
    };
    let Some(suggester) = ctx.suggester.as_mut() else {
        return forward_char(el);
    };
    let info = unsafe { el_line(el) };
    if info.is_null() {
        return forward_char(el);
    }
    let (line, cursor) = unsafe { line_and_cursor(&*info) };
    let at_end = cursor >= line.len();
    let lc = LineContext::new(line, cursor);
    let suggestion = suggester.suggest(&lc);

    match suggestion {
        Some(s) if at_end && !s.is_empty() => {
            ctx.prev_suggest_total = 0;
            if let Ok(cstr) = CString::new(s.text()) {
                unsafe { el_insertstr(el, cstr.as_ptr()) };
            }
            CC_REDISPLAY as c_uchar
        }
        // Nothing to accept (no suggestion, or the cursor isn't at end of
        // line): fall through to the key's default forward-character motion.
        // Both keys bound here -- Ctrl-F and Right-arrow -- are `forward-char`
        // in the default keymap, so binding them for suggestion-accept must
        // not disable ordinary rightward cursor movement (e.g. when editing a
        // line recalled from history).
        _ => forward_char(el),
    }
}

/// Fallback when Ctrl-F is pressed but there is no suggestion to accept.
///
/// Since Right-arrow is left on libedit's native `ed-next-char` (not rebound),
/// only Ctrl-F reaches this path. With nothing to accept, it simply does
/// nothing -- the cursor stays where it is.
fn forward_char(_el: *mut libedit_sys::EditLine) -> c_uchar {
    CC_NORM as c_uchar
}

/// Erase inline suggestion ghost text currently drawn to the right of the
/// cursor by clearing from the cursor to the end of the line (`ESC[K`).
fn clear_ghost(ctx: &mut Context) {
    if ctx.prev_suggest_total == 0 || ctx.out.is_null() {
        return;
    }
    fwrite_bytes(ctx.out, b"\x1b[K");
    unsafe { libc::fflush(ctx.out as *mut libc::FILE) };
    ctx.prev_suggest_total = 0;
}

// ---- User-action dispatch infrastructure ----

/// The return value from a user-defined action, telling libedit what to do
/// after the action completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Redraw the prompt and input line from scratch. Use after printing
    /// multi-line output below the prompt (e.g. a help listing).
    Redisplay,
    /// Refresh the input line in place (efficient single-line update).
    Refresh,
    /// Do nothing -- the line stays as-is.
    Norm,
    /// Emit the terminal bell to signal "no match" or invalid input.
    Beep,
}

impl Action {
    fn to_cc(self) -> c_uchar {
        match self {
            Action::Redisplay => CC_REDISPLAY as c_uchar,
            Action::Refresh => CC_REFRESH as c_uchar,
            Action::Norm => CC_NORM as c_uchar,
            Action::Beep => CC_REFRESH_BEEP as c_uchar,
        }
    }
}

/// Context passed to a user-defined action registered via
/// [`EditLine::add_action`]. Provides read access to the current line and a
/// [`Writer`] for output.
pub struct ActionContext {
    line: String,
    cursor: usize,
    stream: *mut FILE,
}

impl ActionContext {
    /// The full contents of the input line at the moment the action fires.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Byte offset of the cursor within the line.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The word immediately before the cursor (split on whitespace), which is
    /// typically the token a help/completion action wants to operate on.
    pub fn word(&self) -> &str {
        let before = &self.line[..self.cursor];
        let start = before
            .rfind(|c: char| c.is_ascii_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        &before[start..]
    }

    /// A [`Writer`] for emitting output ordered with libedit's display.
    ///
    /// Print your help listing or diagnostics through this, then return
    /// [`Action::Redisplay`] so libedit redraws the prompt below your output.
    pub fn output(&self) -> Writer<'_> {
        Writer {
            stream: self.stream,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Dispatch a user-action trampoline call to the handler stored at `slot`.
fn dispatch_user_action(el: *mut libedit_sys::EditLine, slot: usize) -> c_uchar {
    let Some(ctx) = context_from(el) else {
        return CC_ERROR as c_uchar;
    };
    // Clear any ghost text before the action prints.
    clear_ghost(ctx);

    let info = unsafe { el_line(el) };
    let (line, cursor) = if info.is_null() {
        (String::new(), 0)
    } else {
        unsafe { line_and_cursor(&*info) }
    };

    let action_ctx = ActionContext {
        line,
        cursor,
        stream: ctx.out,
    };

    let action = if let Some(handler) = ctx.actions.get_mut(slot) {
        handler(&action_ctx)
    } else {
        Action::Beep
    };
    action.to_cc()
}

/// Generate the 8 pre-baked `extern "C"` trampolines (one per user-action
/// slot). Each simply delegates to `dispatch_user_action(el, SLOT)`.
macro_rules! action_trampolines {
    ($($idx:literal),*) => {
        const ACTION_TRAMPOLINES: [shim::ElFn; MAX_USER_ACTIONS] = [
            $({
                extern "C" fn trampoline(
                    el: *mut libedit_sys::EditLine, _ch: i32,
                ) -> c_uchar {
                    let result = catch_unwind(AssertUnwindSafe(||
                        dispatch_user_action(el, $idx)
                    ));
                    result.unwrap_or(CC_ERROR as c_uchar)
                }
                trampoline
            },)*
        ];
    };
}

action_trampolines!(0, 1, 2, 3, 4, 5, 6, 7);

/// Query the terminal width (columns) via `ioctl(TIOCGWINSZ)` on the output
/// fd. This is portable across libedit versions (unlike `EL_GETTC "co"`, whose
/// signature varies). Falls back to 80 columns on failure. `_el` is unused but
/// kept for signature symmetry with other render helpers.
fn terminal_width(_el: *mut libedit_sys::EditLine) -> usize {
    // SAFETY: TIOCGWINSZ writes a `winsize` we zero-initialize; we read it only
    // on success (rc == 0) and guard against a zero column count.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        // Query fd 1 (stdout); the editor's streams dup these descriptors.
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

/// Visible display width of `s` in terminal columns, ignoring ANSI escape
/// sequences and counting other chars as width 1. This is a pragmatic
/// approximation that is correct for ASCII CLIs; it does not implement full
/// Unicode east-asian width.
fn display_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut width = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Skip an escape sequence: ESC .. final byte in 0x40..=0x7e.
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
            }
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        } else if bytes[i] == 0x01 {
            // Skip our own literal-region delimiter markers.
            i += 1;
        } else {
            let ch_len = utf8_char_len(bytes[i]);
            i += ch_len.max(1);
            width += 1;
        }
    }
    width
}

/// Recover `&mut Context` from an editor's client-data pointer.
///
/// Returns `None` if client data is unset. The returned reference borrows for
/// the duration of the callback; libedit is single-threaded and does not
/// re-enter our trampolines, so no aliasing occurs.
fn context_from<'a>(el: *mut libedit_sys::EditLine) -> Option<&'a mut Context> {
    let mut data: usize = 0;
    let rc = unsafe { shim::el_get_ptr(el, EL_CLIENTDATA as i32, &mut data) };
    if rc != 0 || data == 0 {
        return None;
    }
    // SAFETY: the pointer was installed in `new` from `Box::into_raw` and is
    // valid until `Drop`. Single-threaded use precludes aliasing.
    Some(unsafe { &mut *(data as *mut Context) })
}

/// Extract the current line text and cursor byte-offset from a `LineInfo`.
///
/// # Safety
/// `info` must be a valid `LineInfo` returned by `el_line`.
unsafe fn line_and_cursor(info: &LineInfo) -> (String, usize) {
    let buffer = info.buffer;
    let lastchar = info.lastchar;
    let cursor = info.cursor;
    if buffer.is_null() || lastchar.is_null() {
        return (String::new(), 0);
    }
    let len = unsafe { lastchar.offset_from(buffer) }.max(0) as usize;
    let cursor_off = if cursor.is_null() {
        len
    } else {
        (unsafe { cursor.offset_from(buffer) }.max(0) as usize).min(len)
    };
    let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), len) };
    let line = String::from_utf8_lossy(bytes).into_owned();
    // Cursor offset is in bytes of the original buffer; lossy conversion keeps
    // byte length for valid UTF-8, which is the common case for a CLI.
    (line, cursor_off)
}

/// Write the pre-assembled candidate listing below the current line so the
/// user can see the available options; libedit redraws the prompt/line
/// afterward via the returned `CC_REDISPLAY`.
///
/// `listing` is the already-formatted block (leading/trailing newlines and any
/// styler-supplied ANSI included). It is written through `stream` -- the same
/// `FILE*` libedit draws on -- then flushed, so our output and libedit's redraw
/// stay correctly ordered on the shared stdio buffer. Falls back to a raw
/// `write(2)` only if the stream is somehow null.
fn write_candidates(stream: *mut FILE, listing: &str) {
    fwrite_bytes(stream, listing.as_bytes());
    // Flush so the listing is visible before libedit redraws the prompt/line.
    // SAFETY: `stream` is libedit's live output `FILE*` when non-null.
    if !stream.is_null() {
        unsafe { libc::fflush(stream as *mut libc::FILE) };
    }
}

/// Converts a prompt string directly to a NUL-terminated `Vec<wchar_t>` with
/// ANSI escape sequences wrapped in `PROMPT_ESC_DELIM`, in a single pass with
/// one allocation.
fn prompt_to_wide(s: &str) -> Vec<wchar_t> {
    let bytes = s.as_bytes();
    // s.len() (byte count) >= char count for UTF-8. +5 covers the NUL
    // terminator (+1) plus up to two escape-sequence delimiter pairs (+4),
    // matching the typical colored-prompt case without reallocation.
    let mut v: Vec<wchar_t> = Vec::with_capacity(s.len() + 5);
    if !bytes.contains(&0x1b) {
        // Fast path: no escapes, just widen each char directly.
        v.extend(s.chars().map(|c| c as wchar_t));
        v.push(0);
        return v;
    }
    #[allow(clippy::unnecessary_cast)]
    let delim = PROMPT_ESC_DELIM as wchar_t;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Find the end of the escape sequence (final byte in 0x40..=0x7e).
            let start = i;
            i += 1;
            // Skip CSI introducer (`[`) so it isn't mistaken for the final byte.
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
            }
            // Consume parameter/intermediate bytes up to the final byte.
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // include the terminator byte
            }
            v.push(delim);
            // Escape bytes are ASCII, so each byte maps 1:1 to a wchar_t.
            v.extend(bytes[start..i].iter().map(|&b| b as wchar_t));
            v.push(delim);
        } else {
            // Decode one UTF-8 character and push as wchar_t.
            let ch_len = utf8_char_len(bytes[i]);
            let end = (i + ch_len).min(bytes.len());
            if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
                for c in s.chars() {
                    v.push(c as wchar_t);
                }
            }
            i = end;
        }
    }
    v.push(0);
    v
}

/// Length in bytes of a UTF-8 character given its leading byte.
fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1, // continuation/invalid byte: advance by one to make progress
    }
}

/// Open a `FILE*` stream over a *duplicated* copy of `fd`.
///
/// Returns a null pointer on failure. The caller owns the returned stream
/// and must `fclose` it. Because the underlying fd is a `dup`, closing the
/// stream does not affect the original `fd`.
///
/// # Safety
/// `mode` must be a valid, NUL-terminated stdio mode string.
unsafe fn open_std_stream(fd: i32, mode: &CStr) -> *mut FILE {
    let dup_fd = unsafe { libc::dup(fd) };
    if dup_fd < 0 {
        return std::ptr::null_mut();
    }
    let stream = unsafe { libc::fdopen(dup_fd, mode.as_ptr()) };
    if stream.is_null() {
        // fdopen failed: it did not take ownership of dup_fd, so close it
        // ourselves to avoid leaking the descriptor.
        unsafe { libc::close(dup_fd) };
        return std::ptr::null_mut();
    }
    stream as *mut FILE
}

/// Close any non-null streams in `streams`.
///
/// # Safety
/// Each non-null pointer must be a live `FILE*` previously returned by
/// [`open_std_stream`] and not already closed.
unsafe fn close_streams(streams: &[*mut FILE; 3]) {
    for &s in streams {
        if !s.is_null() {
            unsafe { libc::fclose(s as *mut libc::FILE) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{display_width, prompt_to_wide, PROMPT_ESC_DELIM};
    use libedit_sys::wchar_t;

    /// Convert a NUL-terminated wide buffer back to a String for readable
    /// assertions. Each wchar_t is cast to a char (valid for BMP + ASCII).
    #[allow(clippy::unnecessary_cast)]
    fn wide_to_string(v: &[wchar_t]) -> String {
        v.iter()
            .take_while(|&&w| w != 0)
            .map(|&w| char::from_u32(w as u32).unwrap_or('\u{FFFD}'))
            .collect()
    }

    /// The delimiter as a `char` for building expected strings.
    #[allow(clippy::unnecessary_cast)]
    fn d() -> char {
        PROMPT_ESC_DELIM as u8 as char
    }

    #[test]
    fn plain_text_is_unchanged() {
        let wide = prompt_to_wide("prompt> ");
        assert_eq!(wide_to_string(&wide), "prompt> ");
    }

    #[test]
    fn display_width_counts_visible_chars() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn display_width_ignores_ansi_escapes() {
        // Faint prefix + reset should contribute zero columns.
        assert_eq!(display_width("\x1b[2mghost\x1b[0m"), 5);
        assert_eq!(display_width("\x1b[1;36mx\x1b[0m"), 1);
    }

    #[test]
    fn display_width_ignores_prompt_delim() {
        #[allow(clippy::unnecessary_cast)]
        let delim = PROMPT_ESC_DELIM as u8 as char;
        let s = format!("{delim}\x1b[32m{delim}> ");
        // Delimiters and the escape they wrap are zero-width; "> " is 2.
        assert_eq!(display_width(&s), 2);
    }

    #[test]
    fn display_width_counts_multibyte_as_one() {
        // Pragmatic: each char is width 1 (no east-asian width handling).
        assert_eq!(display_width("café"), 4);
    }

    #[test]
    fn wraps_a_single_sgr_sequence() {
        // "\x1b[32m> " -> the escape is bracketed, the "> " is left alone.
        let input = "\x1b[32m> ";
        let expected = format!("{d}\x1b[32m{d}> ", d = d());
        assert_eq!(wide_to_string(&prompt_to_wide(input)), expected);
    }

    #[test]
    fn wraps_leading_and_trailing_sequences() {
        // Bold green "> " with a reset after it.
        let input = "\x1b[1;32m> \x1b[0m";
        let expected = format!("{d}\x1b[1;32m{d}> {d}\x1b[0m{d}", d = d());
        assert_eq!(wide_to_string(&prompt_to_wide(input)), expected);
    }

    #[test]
    fn preserves_multibyte_text() {
        // A non-ASCII char adjacent to an escape must not be split.
        let input = "caf\u{00e9} \x1b[0m";
        let expected = format!("caf\u{00e9} {d}\x1b[0m{d}", d = d());
        assert_eq!(wide_to_string(&prompt_to_wide(input)), expected);
    }

    #[test]
    fn handles_escape_at_end_without_terminator() {
        // A lone trailing ESC (malformed) is still bracketed and doesn't panic.
        let input = "x\x1b";
        let expected = format!("x{d}\x1b{d}", d = d());
        assert_eq!(wide_to_string(&prompt_to_wide(input)), expected);
    }
}
