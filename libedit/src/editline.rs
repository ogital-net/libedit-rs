//! Safe wrapper around libedit's `EditLine` editor.

use libc::{c_uchar, FILE};
use libedit_sys::*;
use std::ffi::{c_void, CStr, CString};
use std::io::Write;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use std::path::Path;

use crate::error::{Error, Result};
use crate::history::{path_to_cstring, History};
use crate::wstr::{char_from_c, WCStr, WCString, WChar};
use crate::{shim, term};

/// Delimiter byte marking non-printing (zero-width) regions of a prompt for
/// libedit's `EL_PROMPT_ESC` / `EL_RPROMPT_ESC` modes. Bytes enclosed between
/// two of these are excluded from the prompt's computed display width. We wrap
/// ANSI escape sequences in this delimiter so colored prompts and hints keep
/// the cursor math correct. `0x01` (SOH) matches readline's convention and is
/// vanishingly unlikely to appear in real prompt text.
const PROMPT_ESC_DELIM: WChar = 0x01;

/// Look-ahead FIFO for bytes read while matching a hint-accept key sequence
/// (right-arrow / `^F`) that turned out to be something else; they are
/// replayed to libedit so normal editing is unaffected. Escape sequences are
/// ASCII, so bytes suffice; capacity 8 covers the longest we peek at
/// (6-byte modified arrows / paste markers) with headroom.
#[derive(Default)]
struct Pending {
    buf: [u8; 8],
    head: u8,
    len: u8,
}

impl Pending {
    /// Append a byte; silently drops on overflow (unreachable in practice).
    fn push(&mut self, b: u8) {
        debug_assert!((self.len as usize) < self.buf.len(), "pending overflow");
        if (self.len as usize) >= self.buf.len() {
            return;
        }
        let tail = (self.head as usize + self.len as usize) % self.buf.len();
        self.buf[tail] = b;
        self.len += 1;
    }

    /// Remove and return the front byte, or `None` if empty.
    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let b = self.buf[self.head as usize];
        self.head = ((self.head as usize + 1) % self.buf.len()) as u8;
        self.len -= 1;
        Some(b)
    }
}

/// Per-editor state that libedit callbacks reach via the editor's client
/// data. Heap-allocated and owned by the `EditLine` through a raw pointer so
/// its address is stable for the lifetime of the editor (the trampolines
/// recover `&mut Context` from the client-data pointer).
struct Context {
    prompt_wide: WCString,
    last_prompt: String,
    line_buf: String,
    insert_buf: WCString,
    /// `FILE*` streams and their fds handed to `el_init`. Each wraps a
    /// *duplicated* copy of fd 0/1/2 (via `dup`), so closing them in `Drop`
    /// frees the stream buffers and the duplicated descriptors without
    /// touching the process's real standard streams.
    streams: [(*mut FILE, i32); 3],
    complete_handler: Option<Box<dyn EventHandler>>,
    help_handler: Option<Box<dyn EventHandler>>,
    hinter: Option<Box<dyn Hinter>>,
    hint_buf: WCString,
    /// Scratch buffer for assembling bracketed-paste content before it is
    /// inserted into libedit's line. Reused across pastes.
    paste_buf: WCString,
    /// When `true`, `readline` puts the terminal into bracketed-paste mode so
    /// pasted text is delimited and inserted literally (no keymap actions).
    bracketed_paste: bool,
    /// Look-ahead queue replayed to libedit before the next tty read.
    pending: Pending,
    /// When `true`, `readline` automatically adds non-empty lines to the
    /// attached history.
    auto_add_history: bool,
    /// When `true` (and `auto_add_history` is enabled), lines starting with
    /// a space are not added to history.
    ignore_space: bool,
    /// Cached terminal width in columns, sampled once per `readline` call.
    term_cols: u16,
    /// Owned history buffer. Set by `set_history`; accessed by
    /// `auto_add_history`. `None` until a history is attached.
    history: Option<History>,
    /// Guard that installed no-op handlers for terminating signals before
    /// `EL_SIGNAL` was enabled. Held until `Drop` to restore the original
    /// dispositions.
    #[cfg(unix)]
    _signal_guard: Option<SignalGuard>,
}

impl Context {
    /// Open `FILE*` streams over duplicated copies of fds 0/1/2, returning
    /// `None` if any fail.
    fn new() -> Option<Self> {
        // Open independent `FILE*` streams over duplicated copies of the
        // standard fds. `fdopen` does not dup, so we must `dup` first;
        // otherwise closing these streams would close fds 0/1/2.
        let stdin = unsafe { open_std_stream(0, c"r") };
        let stdout = unsafe { open_std_stream(1, c"w") };
        let stderr = unsafe { open_std_stream(2, c"w") };
        let streams = [stdin, stdout, stderr];

        // If any stream failed to open, clean up the ones that succeeded.
        if stdin.0.is_null() || stdout.0.is_null() || stderr.0.is_null() {
            unsafe { close_streams(&streams) };
            return None;
        }

        Some(Context {
            prompt_wide: WCString::default(),
            last_prompt: String::new(),
            line_buf: String::new(),
            insert_buf: WCString::default(),
            streams,
            pending: Pending::default(),
            complete_handler: None,
            help_handler: None,
            hinter: None,
            hint_buf: WCString::default(),
            paste_buf: WCString::default(),
            bracketed_paste: true,
            auto_add_history: false,
            ignore_space: false,
            term_cols: 0,
            history: None,
            #[cfg(unix)]
            _signal_guard: None,
        })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { close_streams(&self.streams) };
    }
}

/// The key-binding style used for line editing.
///
/// Corresponds to libedit's `EL_EDITOR` parameter and the `editor` line in
/// `.editrc`. The actual default depends on how the system's libedit was
/// compiled (see [`EditLine::set_editor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    /// Emacs-style key bindings.
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
    inner: NonNull<libedit_sys::EditLine>,
    // Heap-stable per-editor context. Stored as a raw pointer (via
    // `Box::into_raw`) rather than a `Box` field so the trampolines can form
    // `&mut Context` from the client-data pointer without aliasing a Rust
    // reference held elsewhere. Freed in `Drop`.
    context: *mut Context,
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

        let context = Context::new().ok_or(Error::Null)?;

        let inner = unsafe {
            el_init(
                name.as_ptr(),
                context.streams[0].0,
                context.streams[1].0,
                context.streams[2].0,
            )
        };

        let inner = NonNull::new(inner).ok_or(Error::Null)?;

        let context = Box::into_raw(Box::new(context));
        unsafe {
            shim::el_set_clientdata(inner.as_ptr(), context as *mut c_void);
            // Register the prompt trampoline once, in ESC-aware mode so ANSI
            // color escapes wrapped in `PROMPT_ESC_DELIM` are not counted
            // toward the prompt width. It reads the current prompt from the
            // context on demand.
            shim::el_set_prompt_esc_fn(inner.as_ptr(), prompt_trampoline, PROMPT_ESC_DELIM);
            // Install our get-character trampoline unconditionally. It powers
            // both bracketed paste (on by default) and, when a hinter is set,
            // inline suggestions. With neither active it is a transparent
            // passthrough that just decodes one character per call.
            shim::el_set_getcfn(inner.as_ptr(), getcfn_trampoline);
        }

        Ok(EditLine { inner, context })
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
    /// The returned line has its trailing newline removed. Input that is not
    /// valid UTF-8 is converted lossily (invalid sequences become the U+FFFD
    /// replacement character).
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
    /// # Errors
    ///
    /// Returns [`Error::Eof`] on end-of-file (e.g. the user pressed Ctrl-D on
    /// an empty line), or [`Error::Interrupted`] if the read was interrupted
    /// by a signal such as Ctrl-C (only when signal handling is enabled via
    /// [`set_signal_handling`](Self::set_signal_handling)).
    pub fn readline(&mut self, prompt: impl AsRef<str>) -> Result<String> {
        // SAFETY: `self.context` was created in `new` via `Box::into_raw` and
        // is only mutated here and freed in `Drop`; no other reference to it
        // is live at this point.
        //
        // Cache the prompt conversion: many callers loop on readline with
        // the same prompt string, so we avoid reconstructing the wide
        // representation and recomputing the display width on every
        // iteration.
        let prompt_str = prompt.as_ref();
        let context = unsafe { &mut *self.context };
        if context.last_prompt.as_str() != prompt_str {
            context.prompt_wide.clear();
            prompt_to_wide(prompt_str, &mut context.prompt_wide);
            context.last_prompt.clear();
            context.last_prompt.push_str(prompt_str);
        }
        // Sample the terminal width once per readline call so the
        // per-keystroke suggestion path can use the cached value
        // instead of issuing an ioctl on every character.
        context.term_cols = terminal_width(context.streams[1].1);

        // Put the terminal into bracketed-paste mode for the duration of this
        // read. The guard emits the enable sequence now and the disable
        // sequence on drop, so every return path below (including EOF and
        // interrupt) restores the terminal.
        let _paste_guard = context
            .bracketed_paste
            .then(|| BracketedPasteGuard::enable(context.streams[1].0));

        let mut count: i32 = 0;
        let mut err: i32 = 0;
        let line_ptr = unsafe { shim::el_wgets_err(self.inner.as_ptr(), &mut count, &mut err) };

        if line_ptr.is_null() || count < 0 {
            // libedit returns NULL for both EOF (Ctrl-D) and a signal-
            // interrupted read (Ctrl-C, with EL_SIGNAL enabled). Only errno
            // distinguishes them: a signal leaves errno == EINTR.
            if err == libc::EINTR {
                return Err(Error::Interrupted);
            }
            return Err(Error::Eof);
        }

        if count == 0 {
            return Ok(String::new());
        }

        // Trim the trailing newline in place in libedit's wide buffer
        // (zero copy), then borrow the result. The same wide pointer
        // feeds both auto-add history and the return value.
        let wcstr = unsafe { WCStr::from_ptr_mut(line_ptr as *mut WChar) };
        wcstr.trim_end();

        // Auto-add to history if enabled, from the wide buffer directly.
        self.maybe_auto_add_history(wcstr);
        wcstr.trim_start();

        Ok(wcstr.to_string_lossy())
    }

    /// Conditionally add a line to the attached history (for auto_add_history).
    ///
    /// The line has already been trimmed of trailing whitespace/newlines.
    fn maybe_auto_add_history(&mut self, line: &WCStr) {
        let ctx = unsafe { &mut *self.context };
        if !ctx.auto_add_history {
            return;
        }
        if line.is_empty() {
            return;
        }
        if ctx.ignore_space && line.units().first() == Some(&(' ' as WChar)) {
            return;
        }
        if let Some(ref mut history) = ctx.history {
            history.add_wide(line);
        }
    }

    /// Register a Tab-completion handler.
    ///
    /// The handler is invoked when the user presses Tab. It receives a
    /// [`LineContext`] with the current line contents, cursor position,
    /// and writers for inserting text or printing output.
    pub fn set_complete_handler<H: EventHandler + 'static>(&mut self, handler: H) -> Result<()> {
        self.set_complete_handler_boxed(Box::new(handler))
    }

    /// Like [`set_complete_handler`](Self::set_complete_handler) but takes
    /// an already‑boxed trait object, avoiding an extra allocation when the
    /// caller already owns a `Box<dyn EventHandler>`.
    pub fn set_complete_handler_boxed(&mut self, handler: Box<dyn EventHandler>) -> Result<()> {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).complete_handler = Some(handler);
        }
        // Register and bind the completion trampoline to Tab. Idempotent to
        // call more than once (EL_ADDFN/EL_BIND simply re-register).
        let name = c"ed-complete";
        let help = c"rust completion handler";
        let key = c"\t";
        let rc = unsafe {
            shim::el_addfn_bind(self.inner.as_ptr(), name, help, key, completion_trampoline)
        };
        if rc != 0 {
            return Err(Error::operation(EL_ADDFN as i32, rc));
        }
        Ok(())
    }

    /// Register a context-help handler bound to `?`.
    ///
    /// The handler is invoked when the user presses `?`. It receives a
    /// [`LineContext`] identical to the completion handler.
    pub fn set_help_handler<H: EventHandler + 'static>(&mut self, handler: H) -> Result<()> {
        self.set_help_handler_boxed(Box::new(handler))
    }

    /// Like [`set_help_handler`](Self::set_help_handler) but takes an
    /// already‑boxed trait object.
    pub fn set_help_handler_boxed(&mut self, handler: Box<dyn EventHandler>) -> Result<()> {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).help_handler = Some(handler);
        }
        // Register and bind the help trampoline to ?. Idempotent to
        // call more than once (EL_ADDFN/EL_BIND simply re-register).
        let name = c"context-help";
        let help = c"rust help handler";
        let key = c"?";
        let rc =
            unsafe { shim::el_addfn_bind(self.inner.as_ptr(), name, help, key, help_trampoline) };
        if rc != 0 {
            return Err(Error::operation(EL_ADDFN as i32, rc));
        }
        Ok(())
    }

    /// Register an inline hint (fish‑style suggestion) handler.
    ///
    /// The hinter is called on each keystroke while the cursor sits at the
    /// end of a non‑empty line. Its output is drawn as dimmed ghost text
    /// immediately after the cursor. Pressing right‑arrow or Ctrl‑F inserts
    /// the hint into the input line (accepting the suggestion); any other
    /// key simply dismisses it.
    ///
    /// At most one hinter may be active at a time; calling this method again
    /// replaces the previous one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects installing the
    /// character‑read callback.
    pub fn set_hinter<H: Hinter + 'static>(&mut self, hinter: H) -> Result<()> {
        self.set_hinter_boxed(Box::new(hinter))
    }

    /// Like [`set_hinter`](Self::set_hinter) but takes an already‑boxed
    /// trait object.
    pub fn set_hinter_boxed(&mut self, hinter: Box<dyn Hinter>) -> Result<()> {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).hinter = Some(hinter);
        }
        Ok(())
    }

    /// Enable or disable bracketed paste mode.
    ///
    /// When enabled (the default), [`readline`](Self::readline) puts the
    /// terminal into bracketed-paste mode for the duration of the read.
    /// Pasted text is then delimited by the terminal and inserted into the
    /// line *literally* -- a pasted newline or Tab is treated as text, not as
    /// "submit" or "complete". Disable it if you need the raw legacy behavior
    /// where pasted control characters trigger their bindings.
    pub fn set_bracketed_paste(&mut self, enabled: bool) {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        unsafe {
            (*self.context).bracketed_paste = enabled;
        }
    }

    /// Enable or disable signal handling by libedit.
    ///
    /// When enabled, libedit installs handlers for `SIGWINCH` (terminal
    /// resize), `SIGCONT` (continue), and several terminating signals
    /// (`SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGTERM`, `SIGTSTP`).
    ///
    /// # Ctrl-C (`SIGINT`)
    ///
    /// This method automatically installs no-op handlers for the terminating
    /// signals before handing control to libedit. As a result, pressing
    /// Ctrl-C during [`readline`](Self::readline) is trapped by libedit,
    /// re-delivered to the no-op handler via libedit's save-restore-raise
    /// protocol, and returned as [`Error::Interrupted`] -- **without** killing
    /// the process. No application-side `sigaction` is required.
    ///
    /// The no-op handlers are removed (restoring the original dispositions)
    /// when signal handling is disabled *or* when the editor is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Operation`] if libedit rejects the parameter change.
    pub fn set_signal_handling(&mut self, enabled: bool) -> Result<()> {
        #[cfg(unix)]
        {
            let ctx = unsafe { &mut *self.context };
            if enabled {
                ctx._signal_guard = Some(SignalGuard::install());
            } else {
                ctx._signal_guard = None;
            }
        }
        self.set_int(EL_SIGNAL as i32, enabled as i32)
    }

    /// Query the current terminal dimensions `(columns, rows)`.
    pub fn terminal_size(&self) -> (usize, usize) {
        let ctx = unsafe { &*self.context };
        term::size(ctx.streams[1].1).unwrap_or((80, 24))
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
        let rc = unsafe { shim::el_set_editor(self.inner.as_ptr(), name.as_ptr()) };
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
                unsafe { el_source(self.inner.as_ptr(), c_path.as_ptr()) }
            }
            None => unsafe { el_source(self.inner.as_ptr(), std::ptr::null()) },
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

    /// Attach a [`History`] buffer for interactive recall (up/down arrows).
    ///
    /// This transfers ownership of the history buffer into the editor. Use
    /// [`history`](Self::history) or [`history_mut`](Self::history_mut) to
    /// access it afterward. Passing a new `History` replaces any previously
    /// attached one.
    ///
    /// This uses libedit's wide history API (`el_wset(EL_HIST, history_w, ...)`),
    /// which avoids a narrow↔wide conversion penalty on every history lookup
    /// when the locale is multibyte.
    pub fn set_history(&mut self, history: History) {
        // SAFETY: `self.context` is valid for the editor's lifetime.
        let ctx = unsafe { &mut *self.context };
        unsafe { shim::el_wset_hist(self.inner.as_ptr(), history.as_ptr()) };
        ctx.history = Some(history);
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
    /// When enabled (and
    /// [`set_auto_add_history`](Self::set_auto_add_history) is active), lines
    /// whose first character is an ASCII space are not added to history. This
    /// mirrors bash/zsh `HIST_IGNORE_SPACE`.
    ///
    /// Disabled by default.
    pub fn set_history_ignore_space(&mut self, enabled: bool) {
        unsafe {
            (*self.context).ignore_space = enabled;
        }
    }

    /// Return a shared reference to the attached [`History`], if any.
    ///
    /// Returns `None` if no history has been set via
    /// [`set_history`](Self::set_history).
    pub fn history(&self) -> Option<&History> {
        unsafe { (*self.context).history.as_ref() }
    }

    /// Return a mutable reference to the attached [`History`], if any.
    ///
    /// Returns `None` if no history has been set via
    /// [`set_history`](Self::set_history).
    pub fn history_mut(&mut self) -> Option<&mut History> {
        unsafe { (*self.context).history.as_mut() }
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
        let ret = unsafe { shim::el_set_int(self.inner.as_ptr(), op, val) };
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
        unsafe { shim::el_get_int(self.inner.as_ptr(), op) }.ok_or_else(|| Error::operation(op, -1))
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
    pub fn output(&mut self) -> OutWriter<'_> {
        // SAFETY: `streams[1]` is libedit's output `FILE*`, valid for the
        // editor's lifetime. The returned `Writer` borrows `self`, so it can't
        // outlive the stream.
        let ctx = unsafe { &*self.context };
        OutWriter::new(ctx.streams[1].0)
    }

    /// A [`std::io::Write`] handle to libedit's error stream.
    ///
    /// Like [`output`](Self::output) but targets the stream passed as
    /// `ferr` to libedit (a dup of fd 2). Use it for diagnostics that should
    /// go to standard error while still staying ordered with libedit's
    /// drawing.
    pub fn error_output(&mut self) -> OutWriter<'_> {
        // SAFETY: as `output`, but for the error stream (`streams[2]`).
        let ctx = unsafe { &*self.context };
        OutWriter::new(ctx.streams[2].0)
    }

    /// Return a raw pointer to the underlying `EditLine`.
    ///
    /// # Safety
    /// The caller must not call `el_end` or otherwise free this pointer, and
    /// must not use it after this `EditLine` is dropped.
    pub unsafe fn as_ptr(&self) -> *mut libedit_sys::EditLine {
        self.inner.as_ptr()
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
pub struct OutWriter<'a> {
    stream: *mut FILE,
    _marker: std::marker::PhantomData<&'a mut EditLine>,
}

impl<'a> OutWriter<'a> {
    /// Wrap a raw libedit `FILE*` stream for ordered output.
    ///
    /// The `PhantomData` lifetime ties the writer to the editor that owns
    /// the stream; use `PhantomData` to produce a writer with no lifetime
    /// constraint when called from a trampoline or free function that holds
    /// a raw pointer.
    fn new(stream: *mut FILE) -> Self {
        OutWriter {
            stream,
            _marker: PhantomData,
        }
    }
}

impl std::fmt::Debug for OutWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("stream", &self.stream)
            .finish()
    }
}

impl std::io::Write for OutWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: `stream` is a live `FILE*` for the editor's lifetime.
        if !self.stream.is_null() {
            unsafe {
                Ok(libc::fwrite(
                    buf.as_ptr() as *const c_void,
                    1,
                    buf.len(),
                    self.stream,
                ))
            }
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: `stream` is a live `FILE*` for the editor's lifetime.
        if !self.stream.is_null() {
            unsafe { libc::fflush(self.stream) };
        }
        Ok(())
    }
}

/// A [`std::io::Write`] / [`std::fmt::Write`] adapter that appends directly
/// into a [`WCString`], avoiding an intermediate `String`/`Vec<u8>`
/// allocation.
///
/// Each written [`char`] becomes one wide (`wchar_t`) unit. The writer only
/// *appends*; it never adds a NUL terminator, so the owner must call
/// [`WCString::terminate`](crate::wstr::WCString::terminate) before handing the
/// buffer to libedit's wide FFI.
///
/// # Encoding
///
/// * [`fmt::Write`] input (`write!`, `write_fmt`, `write_str`) is guaranteed
///   valid UTF-8 by the standard library, so it is appended verbatim with no
///   conversion or validation.
/// * [`io::Write`] input (`write`, `write_all`) is an arbitrary byte buffer
///   with no UTF-8 guarantee and may even split a multi-byte sequence across
///   calls, so it is decoded **best-effort** via
///   [`String::from_utf8_lossy`]; malformed bytes become U+FFFD rather than
///   erroring out.
///
/// [`fmt::Write`]: std::fmt::Write
/// [`io::Write`]: std::io::Write
struct WCStringWriter<'a> {
    wcstring: &'a mut WCString,
}

impl<'a> WCStringWriter<'a> {
    fn new(wcstring: &'a mut WCString) -> Self {
        WCStringWriter { wcstring }
    }
}

impl std::fmt::Write for WCStringWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        // `s` is guaranteed valid UTF-8, so append each scalar directly.
        self.wcstring.push_str(s);
        Ok(())
    }

    fn write_char(&mut self, c: char) -> std::fmt::Result {
        self.wcstring.push(c);
        Ok(())
    }
}

impl std::io::Write for WCStringWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // `io::Write` gives us raw bytes with no UTF-8 guarantee (and a
        // multi-byte sequence may be split across calls). Decode lossily so a
        // stray/partial byte becomes U+FFFD instead of failing the write. We
        // always report the whole buffer as consumed.
        let text = String::from_utf8_lossy(buf);
        self.wcstring.push_str(&text);
        Ok(buf.len())
    }

    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        // Formatting output is guaranteed valid UTF-8, so route it through the
        // `fmt::Write` path: each format piece is appended straight into the
        // `WCString` with no lossy conversion and no intermediate `String`.
        // Our `fmt::Write` impl is infallible, so this cannot actually error.
        std::fmt::Write::write_fmt(self, args)
            .map_err(|_| std::io::Error::other("formatting error"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Signal guard -- ensures libedit's save-restore-raise protocol hits no-ops
// ---------------------------------------------------------------------------

/// Installs no-op handlers for the terminating signals that libedit traps
/// when `EL_SIGNAL` is enabled.
///
/// libedit's signal protocol: `sig_set` saves the old handler for each signal,
/// then ``sig_handler`` restores that old handler and re-raises the signal.
/// If the old handler was `SIG_DFL`, the re-raise kills the process. By
/// installing no-op handlers *before* enabling `EL_SIGNAL`, we ensure libedit
/// saves our no-op (not `SIG_DFL`), so the re-raise is harmless and `read(2)`
/// can return `EINTR` -> `Error::Interrupted`.
///
/// Dropping the guard restores the original dispositions.
#[cfg(unix)]
struct SignalGuard {
    orig: [libc::sigaction; SignalGuard::NSIGS],
}

/// Set by [`SignalGuard::noop`] when a *terminating* signal (Ctrl-C etc.) is
/// re-raised through libedit's save-restore protocol. The get-character read
/// loop reads and clears this to tell an interrupt that should abort the line
/// (flag set) apart from a benign `SIGWINCH`/`SIGCONT` that should just retry
/// the read (flag clear) -- matching libedit's own `read_char`, whose handlers
/// carry no `SA_RESTART`.
#[cfg(unix)]
static TERMINATING_SIGNAL: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
impl SignalGuard {
    /// Signals that would terminate the process if `SIG_DFL` is restored.
    const SIGS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGTERM];
    const NSIGS: usize = Self::SIGS.len();

    /// No-op signal handler for the terminating signals. It records that one
    /// fired (async-signal-safe: a single relaxed atomic store) so the read
    /// loop can surface it as an interrupt, and is a non-`SIG_DFL` address so
    /// libedit's re-raise doesn't kill the process.
    extern "C" fn noop(_signo: libc::c_int) {
        TERMINATING_SIGNAL.store(true, Ordering::Relaxed);
    }

    /// Install `Self::noop` for every signal in [`Self::SIGS`], saving the
    /// original `sigaction`. `sa_flags` is 0 (no `SA_RESTART`) so the
    /// interrupted `read(2)` yields `EINTR`.
    fn install() -> Self {
        unsafe {
            let mut guard = SignalGuard {
                orig: std::mem::zeroed(),
            };
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = Self::noop as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            for (i, &sig) in Self::SIGS.iter().enumerate() {
                libc::sigaction(sig, &sa, &mut guard.orig[i]);
            }
            guard
        }
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (i, &sig) in Self::SIGS.iter().enumerate() {
            unsafe { libc::sigaction(sig, &self.orig[i], std::ptr::null_mut()) };
        }
    }
}

/// DECSET 2004 enable / disable sequences for bracketed-paste mode.
const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";

/// RAII guard that turns bracketed-paste mode on for the terminal and turns it
/// back off when dropped, so a `readline` that returns early (EOF, interrupt)
/// never leaves the terminal in paste mode.
struct BracketedPasteGuard {
    stream: *mut FILE,
}

impl BracketedPasteGuard {
    /// Emit the enable sequence on `stream` (libedit's output `FILE*`) and
    /// return a guard that emits the disable sequence on drop.
    fn enable(stream: *mut FILE) -> Self {
        let mut w = OutWriter::new(stream);
        let _ = w.write_all(BRACKETED_PASTE_ON.as_bytes());
        let _ = w.flush();
        BracketedPasteGuard { stream }
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let mut w = OutWriter::new(self.stream);
        let _ = w.write_all(BRACKETED_PASTE_OFF.as_bytes());
        let _ = w.flush();
    }
}

impl Drop for EditLine {
    fn drop(&mut self) {
        // Tear down the editor first; it may flush to the streams and will
        // no longer invoke our trampolines afterward.
        unsafe { el_end(self.inner.as_ptr()) };
        // Reclaim and drop the heap context. Context::drop closes the
        // streams, then all other fields are dropped naturally.
        // SAFETY: `context` came from `Box::into_raw` in `new` and is
        // dropped exactly once here.
        drop(unsafe { Box::from_raw(self.context) });
    }
}

/// Prompt trampoline registered with libedit's `EL_PROMPT_ESC` via `el_wset`
extern "C" fn prompt_trampoline(el: *mut libedit_sys::EditLine) -> *mut WChar {
    let ctx = context_from(el);
    ctx.prompt_wide.as_ptr().cast_mut()
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
    // SAFETY: context_from derives `&mut Context` from the editor's
    // client-data pointer; the borrow is exclusive for this callback.
    let ctx = context_from(el);
    // Pass individual fields rather than `&mut Context` so the borrow
    // checker can see that the handler (a shared borrow of
    // `ctx.complete_handler`) does not alias the mutable borrows of
    // `ctx.insert_buf`, `ctx.line_buf`, etc.
    let Some(handler) = ctx.complete_handler.as_deref() else {
        return Action::Error.to_cc();
    };
    exec_handler(
        el,
        &mut ctx.insert_buf,
        ctx.streams[1].0,
        &mut ctx.line_buf,
        ctx.term_cols,
        handler,
    )
    .to_cc()
}

extern "C" fn help_trampoline(el: *mut libedit_sys::EditLine, _c: i32) -> c_uchar {
    // Guard the entire FFI boundary against panics: unwinding into C is UB.
    let result = catch_unwind(AssertUnwindSafe(|| help_impl(el)));
    result.unwrap_or(CC_ERROR as c_uchar)
}

fn help_impl(el: *mut libedit_sys::EditLine) -> c_uchar {
    let ctx = context_from(el);
    let Some(handler) = ctx.help_handler.as_deref() else {
        return Action::Error.to_cc();
    };
    exec_handler(
        el,
        &mut ctx.insert_buf,
        ctx.streams[1].0,
        &mut ctx.line_buf,
        ctx.term_cols,
        handler,
    )
    .to_cc()
}

/// Control-F: emacs forward-char, and our hint-accept key.
const CTRL_F: u8 = 0x06;
/// Escape: lead byte of arrow-key and other CSI/SS3 sequences.
const ESC: u8 = 0x1b;
/// Timeout (ms) for the byte after `ESC`, so a lone `ESC` doesn't stall.
/// Heuristic; roughly matches editors' escape timeout (e.g. Neovim's 50ms).
const ESC_SEQ_TIMEOUT_MS: i32 = 50;

/// Get-character trampoline installed via `EL_GETCFN`.
///
/// This is the FFI entry point libedit calls; it guards the whole body against
/// panics (unwinding across the C boundary is UB) and returns `-1` (read
/// error) if one occurs. The real logic lives in [`getcfn_impl`].
unsafe extern "C" fn getcfn_trampoline(el: *mut libedit_sys::EditLine, out: *mut WChar) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| unsafe { getcfn_impl(el, out) }));
    result.unwrap_or(-1)
}

/// Body of the get-character hook.
///
/// Feeds libedit one decoded character per call. It's also where fish-style
/// hint *acceptance* happens: right-arrow / `^F` insert the showing hint (via
/// [`accept_hint`]) instead of moving the cursor; with no hint they fall
/// through to libedit's normal bindings. Unused look-ahead bytes are replayed
/// via [`Pending`].
///
/// # Safety
/// `el` must be a valid editor; `out` a valid `wchar_t` pointer.
unsafe fn getcfn_impl(el: *mut libedit_sys::EditLine, out: *mut WChar) -> i32 {
    // Recover the context once. Helpers take `ctx` by reborrow (never
    // re-derive from `el`), so only one `&mut Context` is ever live.
    let ctx = context_from(el);

    // Replay queued look-ahead bytes first, so an escape sequence reaches
    // libedit's keymap intact.
    if let Some(b) = ctx.pending.pop() {
        unsafe { *out = b as WChar };
        return 1;
    }

    // Draw the ghost before blocking: libedit has already refreshed for the
    // previous keystroke, so the line on screen is current.
    draw_hint_ghost(el, &mut *ctx);

    let in_fd = ctx.streams[0].1;

    // Read the lead byte. This blocks until input arrives, retrying across
    // benign signals (resize/continue) like libedit's own `read_char`.
    let lead = match unsafe { read_byte(in_fd) } {
        ReadByte::Byte(b) => b,
        ReadByte::Eof => return 0,
        // Both interrupt and error yield -1; errno (EINTR vs other) lets
        // `readline` tell Ctrl-C from a real failure.
        ReadByte::Interrupted | ReadByte::Error => return -1,
    };

    match lead {
        // `^F`: accept the hint, else pass `^F` through (forward-char).
        CTRL_F => {
            if unsafe { accept_hint(el, ctx, out) } {
                return 1;
            }
            unsafe { *out = CTRL_F as WChar };
            1
        }
        // `ESC`: classify the sequence that follows.
        ESC => match unsafe { peek_escape_seq(in_fd) } {
            // Right-arrow accepts the hint if one is showing; otherwise falls
            // through to the replay arm so libedit's keymap runs `ed-next-char`.
            EscSeq::RightArrow(..) if unsafe { accept_hint(el, ctx, out) } => 1,
            // Paste start: read the whole paste, insert it (bypassing the
            // keymap so pasted newlines/tabs are literal), and redraw. It
            // produces no character of its own, so recurse to fetch the next
            // real key.
            EscSeq::PasteStart => {
                unsafe { insert_paste(el, ctx, in_fd, out) };
                unsafe { getcfn_impl(el, out) }
            }
            // Not consumed (a real arrow with no hint, a Meta-key, an
            // unrecognized sequence): return `ESC` and replay the peeked
            // bytes so libedit's keymap sees the whole sequence.
            seq => {
                let (trailing, tlen) = seq.trailing();
                for &b in &trailing[..tlen] {
                    ctx.pending.push(b);
                }
                unsafe { *out = ESC as WChar };
                1
            }
        },
        // Any other byte: decode a (possibly multi-byte) UTF-8 character.
        _ => unsafe { decode_utf8_char(in_fd, lead, out) },
    }
}

/// Outcome of a blocking single-byte read that survives benign interruptions.
enum ReadByte {
    /// A byte was read.
    Byte(u8),
    /// End of file (`read` returned 0).
    Eof,
    /// A terminating signal (Ctrl-C etc.) interrupted the read; the caller
    /// should surface it as [`Error::Interrupted`].
    Interrupted,
    /// A genuine read error.
    Error,
}

/// Blocking read of one byte from `fd`, mirroring libedit's own `read_char`
/// error handling so our unconditional get-character hook has parity with the
/// builtin:
///
/// * `EINTR` from a benign signal (`SIGWINCH` resize, `SIGCONT`) is retried,
///   so resizing the window mid-line doesn't abort it. A terminating signal
///   (flagged by [`TERMINATING_SIGNAL`]) instead returns [`ReadByte::Interrupted`].
/// * `EAGAIN`/`EWOULDBLOCK` clears non-blocking mode on `fd` and retries, like
///   libedit's `read__fixio`.
///
/// # Safety
/// `fd` must be a valid, readable file descriptor.
unsafe fn read_byte(fd: i32) -> ReadByte {
    loop {
        // Clear the terminating-signal flag before each attempt so we only
        // observe a signal delivered during *this* read.
        #[cfg(unix)]
        TERMINATING_SIGNAL.store(false, Ordering::Relaxed);

        let mut b: u8 = 0;
        let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut c_void, 1) };
        if n == 1 {
            return ReadByte::Byte(b);
        }
        if n == 0 {
            return ReadByte::Eof;
        }

        // n < 0: inspect errno.
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {
                // A terminating signal (Ctrl-C) aborts the line; a benign
                // SIGWINCH/SIGCONT just means "retry the read". errno is still
                // EINTR here (nothing below clobbers it), which `el_wgetc`
                // captures and `readline` maps to `Error::Interrupted`.
                #[cfg(unix)]
                if TERMINATING_SIGNAL.load(Ordering::Relaxed) {
                    return ReadByte::Interrupted;
                }
                continue;
            }
            // `EWOULDBLOCK` is the same value as `EAGAIN` on the platforms we
            // target, so matching `EAGAIN` covers both.
            Some(libc::EAGAIN) => {
                // Non-blocking fd: clear O_NONBLOCK and retry, like read__fixio.
                if unsafe { clear_nonblocking(fd) } {
                    continue;
                }
                return ReadByte::Error;
            }
            _ => return ReadByte::Error,
        }
    }
}

/// Clear `O_NONBLOCK` on `fd`. Returns `true` on success. Mirrors the
/// `TRY_AGAIN` recovery in libedit's `read__fixio`.
///
/// # Safety
/// `fd` must be a valid file descriptor.
unsafe fn clear_nonblocking(fd: i32) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags == -1 {
        return false;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) != -1 }
}

/// Read one byte from `fd`, waiting at most `timeout_ms`. Returns `None` on
/// timeout / EOF / error. Used to peek past `ESC` without blocking on a lone
/// `ESC` keypress.
///
/// # Safety
/// `fd` must be a valid, readable file descriptor.
unsafe fn read_byte_timeout(fd: i32, timeout_ms: i32) -> Option<u8> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Retry across signal interruptions so a stray SIGWINCH doesn't
    // spuriously report "no sequence".
    let ready = loop {
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break r;
    };
    if ready <= 0 {
        return None;
    }
    let mut b: u8 = 0;
    let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut c_void, 1) };
    if n == 1 {
        Some(b)
    } else {
        None
    }
}

/// Classification of the bytes following an `ESC`.
enum EscSeq {
    /// A right-arrow (`ESC [ C` or `ESC O C`), the hint-accept key. Carries its
    /// own trailing bytes so that, with no hint to accept, the sequence can be
    /// replayed to libedit's keymap (`ed-next-char`).
    RightArrow([u8; 6], usize),
    /// A bracketed-paste start marker (`ESC [ 2 0 0 ~`).
    PasteStart,
    /// Anything else. Carries the bytes read after `ESC` (in `buf[..len]`) so
    /// the caller can replay them to libedit's keymap.
    Other([u8; 6], usize),
}

impl EscSeq {
    /// Bytes read after `ESC` that must be replayed when the sequence isn't
    /// consumed. Empty only for `PasteStart`, which is fully consumed here.
    fn trailing(&self) -> ([u8; 6], usize) {
        match *self {
            EscSeq::RightArrow(buf, len) | EscSeq::Other(buf, len) => (buf, len),
            EscSeq::PasteStart => ([0u8; 6], 0),
        }
    }
}

/// After an `ESC`, read and classify the following bytes with a short timeout
/// (so a lone `ESC` keypress doesn't stall). Recognizes the right-arrow keys
/// and the bracketed-paste start marker; everything else is returned as
/// [`EscSeq::Other`] with the consumed bytes for replay.
///
/// # Safety
/// `in_fd` must be a valid, readable file descriptor.
unsafe fn peek_escape_seq(in_fd: i32) -> EscSeq {
    let mut buf = [0u8; 6];
    let mut len = 0usize;
    // Read one more byte with the escape timeout, recording it for replay.
    // Evaluates to `EscSeq::Other(..)` (an early return) on timeout/EOF.
    macro_rules! next {
        () => {
            match unsafe { read_byte_timeout(in_fd, ESC_SEQ_TIMEOUT_MS) } {
                Some(b) => {
                    buf[len] = b;
                    len += 1;
                    b
                }
                None => return EscSeq::Other(buf, len),
            }
        };
    }

    // Byte after ESC: CSI (`[`) or SS3 (`O`); anything else isn't ours.
    let b1 = next!();
    if b1 != b'[' && b1 != b'O' {
        return EscSeq::Other(buf, len);
    }

    let b2 = next!();
    match b2 {
        // `ESC [ C` / `ESC O C`: right arrow.
        b'C' => EscSeq::RightArrow(buf, len),
        // `ESC [ 2 0 0 ~`: bracketed-paste start (CSI only).
        b'2' if b1 == b'[' => {
            if next!() == b'0' && next!() == b'0' && next!() == b'~' {
                EscSeq::PasteStart
            } else {
                EscSeq::Other(buf, len)
            }
        }
        _ => EscSeq::Other(buf, len),
    }
}

/// Read a bracketed-paste body (bytes up to the `ESC [ 2 0 1 ~` end marker)
/// and insert it into libedit's line, bypassing the keymap so pasted newlines
/// and Tabs are literal text.
///
/// The whole paste is inserted at once with `el_winsertstr` (which mutates the
/// line buffer but does not draw), then the display is rebuilt: the terminal
/// cursor is parked at column 0 and `EL_REFRESH` is issued so libedit reprints
/// the prompt and line from a known position.
///
/// Returns `false` always (no character is produced via `out`); the caller
/// fetches the next real key.
///
/// # Safety
/// `in_fd` must be valid and readable; `out` must be a valid `wchar_t` pointer.
unsafe fn insert_paste(
    el: *mut libedit_sys::EditLine,
    ctx: &mut Context,
    in_fd: i32,
    _out: *mut WChar,
) -> bool {
    ctx.paste_buf.clear();
    unsafe { read_paste_body(in_fd, &mut ctx.paste_buf) };
    ctx.paste_buf.terminate();

    if ctx.paste_buf.units().is_empty() {
        return false; // empty paste
    }

    // Insert the whole paste directly into the line buffer (no keymap).
    unsafe { el_winsertstr(el, ctx.paste_buf.as_ptr()) };

    // Park the physical cursor at column 0 so libedit's redraw starts from a
    // known position, then ask libedit to rebuild the display.
    let mut w = OutWriter::new(ctx.streams[1].0);
    let _ = w.write_all(b"\r");
    let _ = w.flush();
    unsafe { shim::el_set_int(el, EL_REFRESH as i32, 0) };

    false
}

/// Read a bracketed-paste body from `in_fd` into `buf`, decoding UTF-8, until
/// the `ESC [ 2 0 1 ~` end marker (or EOF). Newlines are kept literal (libedit
/// renders them, and `readline` trims the ends). No timeout: paste bytes
/// stream continuously between the markers.
///
/// An `ESC` inside the body starts the end marker; if the bytes that follow
/// aren't `[ 2 0 1 ~`, that partial sequence is discarded (a stray escape can't
/// wedge the paste), matching the behavior of other line editors.
///
/// # Safety
/// `in_fd` must be a valid, readable file descriptor.
unsafe fn read_paste_body(in_fd: i32, buf: &mut WCString) {
    loop {
        let mut lead: u8 = 0;
        let n = unsafe { libc::read(in_fd, &mut lead as *mut u8 as *mut c_void, 1) };
        if n <= 0 {
            return; // EOF or error -- stop with what we have
        }

        if lead == ESC {
            // Either the end marker (`[ 2 0 1 ~`) or a stray escape to drop.
            if unsafe { at_paste_end(in_fd) } {
                return;
            }
            continue;
        }

        // Decode one (possibly multi-byte) UTF-8 character.
        let total = utf8_char_len(lead);
        let cp = if total == 1 {
            lead as u32
        } else {
            let mut bytes = [lead, 0, 0, 0];
            let mut ok = true;
            for slot in bytes.iter_mut().take(total).skip(1) {
                let mut b: u8 = 0;
                let m = unsafe { libc::read(in_fd, &mut b as *mut u8 as *mut c_void, 1) };
                if m <= 0 {
                    ok = false;
                    break;
                }
                *slot = b;
            }
            if ok {
                decode_utf8(&bytes[..total])
            } else {
                return; // truncated sequence at EOF
            }
        };
        buf.push_scalar(char::from_u32(cp).unwrap_or('\u{FFFD}') as WChar);
    }
}

/// After an `ESC` inside a paste body, check whether the next bytes complete
/// the end marker `[ 2 0 1 ~`. Consumes the bytes it reads either way.
///
/// # Safety
/// `in_fd` must be a valid, readable file descriptor.
unsafe fn at_paste_end(in_fd: i32) -> bool {
    for &expect in b"[201~" {
        let mut b: u8 = 0;
        let n = unsafe { libc::read(in_fd, &mut b as *mut u8 as *mut c_void, 1) };
        if n <= 0 || b != expect {
            return false;
        }
    }
    true
}

/// Decode `bytes` (a complete 1–4 byte UTF-8 sequence) into a scalar value.
fn decode_utf8(bytes: &[u8]) -> u32 {
    match bytes.len() {
        1 => bytes[0] as u32,
        2 => ((bytes[0] as u32 & 0x1F) << 6) | (bytes[1] as u32 & 0x3F),
        3 => {
            ((bytes[0] as u32 & 0x0F) << 12)
                | ((bytes[1] as u32 & 0x3F) << 6)
                | (bytes[2] as u32 & 0x3F)
        }
        _ => {
            ((bytes[0] as u32 & 0x07) << 18)
                | ((bytes[1] as u32 & 0x3F) << 12)
                | ((bytes[2] as u32 & 0x3F) << 6)
                | (bytes[3] as u32 & 0x3F)
        }
    }
}

/// Decode a UTF-8 character starting at `lead` (reading continuation bytes
/// from `in_fd`) into `*out`. Returns `1` / `0` / `-1` per the `EL_GETCFN`
/// contract (success / EOF / error).
///
/// # Safety
/// `in_fd` must be valid and readable; `out` must be a valid `wchar_t` pointer.
unsafe fn decode_utf8_char(in_fd: i32, lead: u8, out: *mut WChar) -> i32 {
    // Determine how many bytes this UTF-8 sequence occupies from the lead
    // byte. ASCII (and, defensively, any stray continuation/invalid lead byte,
    // which `utf8_char_len` maps to 1) takes the fast path and returns as-is.
    let total = utf8_char_len(lead);
    if total == 1 {
        unsafe { *out = lead as WChar };
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

    // Decode the assembled bytes into a Unicode scalar value via direct
    // UTF-8 bit extraction, then into a `char` via `char::from_u32`
    // (which rejects surrogates and values > 0x10FFFF). On any failure
    // fall back to U+FFFD so a stray byte can't wedge the editor.
    let cp = match total {
        2 => ((buf[0] as u32 & 0x1F) << 6) | (buf[1] as u32 & 0x3F),
        3 => {
            ((buf[0] as u32 & 0x0F) << 12) | ((buf[1] as u32 & 0x3F) << 6) | (buf[2] as u32 & 0x3F)
        }
        4 => {
            ((buf[0] as u32 & 0x07) << 18)
                | ((buf[1] as u32 & 0x3F) << 12)
                | ((buf[2] as u32 & 0x3F) << 6)
                | (buf[3] as u32 & 0x3F)
        }
        _ => unreachable!("total is 2, 3, or 4 here"),
    };
    unsafe { *out = char::from_u32(cp).unwrap_or('\u{FFFD}') as WChar };
    1
}

/// Accept the pending hint by feeding it back through libedit's input path, so
/// each char becomes an `ed_insert` with normal incremental refresh -- keeping
/// libedit's screen model in sync (later edits redraw correctly) and painting
/// over the ghost.
///
/// Per the one-char `EL_GETCFN` contract, the first char goes to `*out` and
/// the rest is pushed via `el_wpush` (drained above our trampoline as further
/// inserts). Returns `true` if a hint was pending (and `*out` written).
///
/// # Safety
/// `out` must be a valid, writable `wchar_t` pointer.
unsafe fn accept_hint(el: *mut libedit_sys::EditLine, ctx: &mut Context, out: *mut WChar) -> bool {
    // No hint pending. This also guards `units()` below, which derefs
    // `WCString -> WCStr` and asserts NUL-termination: `draw_hint_ghost` leaves
    // `hint_buf` empty and unterminated on lines it draws nothing for.
    if ctx.hint_buf.is_empty() {
        return false;
    }

    // Non-empty and NUL-terminated here, so `units()` is the payload without
    // the trailing NUL.
    let units = ctx.hint_buf.units();
    let (&first, rest) = units.split_first().expect("hint_buf is non-empty");

    unsafe { *out = first as WChar };

    // The suffix of a NUL-terminated buffer is itself a NUL-terminated wide
    // string, so push one scalar in -- no copy. `el_wpush` `wcsdup`s its
    // argument, so clearing `hint_buf` right after is safe.
    if !rest.is_empty() {
        unsafe { el_wpush(el, ctx.hint_buf.as_ptr().add(1)) };
    }

    ctx.hint_buf.clear();
    true
}

/// Draw the inline hint text
fn draw_hint_ghost(el: *mut libedit_sys::EditLine, ctx: &mut Context) {
    let hinter = match &ctx.hinter {
        Some(h) => h,
        None => return,
    };
    let mut line_ctx = new_line_context(el, &mut ctx.line_buf, ctx.term_cols);
    if !(line_ctx.char_count > 0 && line_ctx.cursor_at_end()) {
        return;
    }

    ctx.hint_buf.clear();
    let mut writer = WCStringWriter::new(&mut ctx.hint_buf);
    hinter.hint(&mut line_ctx, &mut writer);
    ctx.hint_buf.terminate();
    if ctx.hint_buf.is_empty() {
        return;
    }
    let mut output_writer = OutWriter::new(ctx.streams[1].0);
    let _ = write!(output_writer, "\x1b7"); // save cursor
    hinter.style(&&*ctx.hint_buf, &mut output_writer);
    let _ = write!(output_writer, "\x1b8"); // restore cursor
    let _ = output_writer.flush();
}

// ---- User-action dispatch infrastructure ----

/// The return value from a user-defined action, telling libedit what to do
/// after the action completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Completed normally - the editor continues as usual.
    Norm,
    /// End of line was entered (e.g. the user pressed Enter).
    Newline,
    /// EOF was entered (e.g. Ctrl-D on an empty line).
    Eof,
    /// Expecting further command input as arguments; do nothing visually.
    Arghack,
    /// Refresh the display.
    Refresh,
    /// Refresh the display and beep.
    RefreshBeep,
    /// Cursor moved - update and perform a refresh.
    Cursor,
    /// Redisplay the entire input line. Use this when extra output was
    /// printed (e.g. a candidate listing or help text).
    Redisplay,
    /// An error occurred - beep and flush the tty.
    Error,
    /// Fatal error - reset the tty to a known state.
    Fatal,
}

impl Action {
    fn to_cc(self) -> c_uchar {
        match self {
            Action::Norm => CC_NORM as c_uchar,
            Action::Newline => CC_NEWLINE as c_uchar,
            Action::Eof => CC_EOF as c_uchar,
            Action::Arghack => CC_ARGHACK as c_uchar,
            Action::Refresh => CC_REFRESH as c_uchar,
            Action::RefreshBeep => CC_REFRESH_BEEP as c_uchar,
            Action::Cursor => CC_CURSOR as c_uchar,
            Action::Redisplay => CC_REDISPLAY as c_uchar,
            Action::Error => CC_ERROR as c_uchar,
            Action::Fatal => CC_FATAL as c_uchar,
        }
    }
}

/// Snapshot of the current editing state passed to event handlers.
///
/// Provides read access to the line text and cursor position, plus
/// writers for inserting text into the line buffer and printing to
/// the terminal output stream.
pub struct LineContext<'a> {
    line: &'a str,
    char_count: usize,
    cursor_pos: usize,
    term_cols: u16,
}

impl<'a> LineContext<'a> {
    pub(crate) fn new(line: &'a str, char_count: usize, cursor_pos: usize, term_cols: u16) -> Self {
        LineContext {
            line,
            char_count,
            cursor_pos,
            term_cols,
        }
    }

    /// The full contents of the input line.
    pub fn line(&self) -> &str {
        self.line
    }

    /// The number of characters (not bytes) within [`line`](Self::line).
    pub fn char_count(&self) -> usize {
        self.char_count
    }

    /// The character offset of the cursor within [`line`](Self::line).
    pub fn cursor(&self) -> usize {
        self.cursor_pos
    }

    /// Returns `true` when the cursor is at the end of the line.
    pub fn cursor_at_end(&self) -> bool {
        self.cursor_pos == self.char_count
    }

    /// The current width of the terminal in columns
    pub fn term_cols(&self) -> u16 {
        self.term_cols
    }
}

impl std::fmt::Debug for LineContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineContext")
            .field("line", &self.line)
            .field("char_count", &self.char_count)
            .field("cursor_pos", &self.cursor_pos)
            .field("term_cols", &self.term_cols)
            .finish()
    }
}

/// Trait for editor event callbacks (completion, help, etc.).
///
/// Implement this on a type and pass it to
/// [`EditLine::set_complete_handler`] or [`EditLine::set_help_handler`].
pub trait EventHandler {
    /// Called when the bound key is pressed. Return an [`Action`] to tell
    /// the editor what to do next.
    fn handle(
        &self,
        line_ctx: &mut LineContext,
        insert_writer: &mut dyn std::fmt::Write,
        output_writer: &mut dyn std::io::Write,
    ) -> Action;
}

/// Inline hint (fish‑style suggestion) callback.
///
/// Implement this on a type and pass it to
/// [`EditLine::set_hinter`]. The hinter is called on each keystroke while
/// the cursor sits at the end of a non‑empty input line.
///
/// # Hint text
///
/// [`hint`](Self::hint) produces the **plain** suggestion text (e.g.
/// `"file"` when the user has typed `"fi"`). Only the suffix — what the
/// user hasn't typed yet — is rendered as ghost text. The hint text must
/// not contain ANSI escape sequences.
///
/// # Styling
///
/// [`style`](Self::style) writes the already‑buffered hint text to the
/// terminal, wrapped in whatever escapes are appropriate. The default
/// implementation uses dim mode (`\x1b[2m … \x1b[0m`). Override this
/// method to change the appearance (e.g. to use a different color or
/// intensity).
///
/// # Acceptance
///
/// Pressing right‑arrow or Ctrl‑F inserts the full buffered hint into the
/// input line; pressing any other key dismisses it without side effects.
///
/// # Example
///
/// ```no_run
/// use libedit::{EditLine, Hinter, LineContext};
///
/// struct MyHinter;
///
/// impl Hinter for MyHinter {
///     fn hint(&self, ctx: &mut LineContext, writer: &mut dyn std::fmt::Write) {
///         // Suggest "help" when the line starts with "he"
///         if ctx.line().starts_with("he") {
///             let _ = write!(writer, "lp");
///         }
///     }
/// }
///
/// let mut el = EditLine::new("cli").unwrap();
/// el.set_hinter(MyHinter).unwrap();
/// ```
pub trait Hinter {
    /// Produce the hint text (without ANSI escapes) and write it to `writer`.
    /// The hint is buffered so it can be inserted into the input line later
    /// if accepted.
    fn hint(&self, line_ctx: &mut LineContext, writer: &mut dyn std::fmt::Write);

    /// Style the buffered hint text for display, writing ANSI escapes and
    /// the hint content directly to `writer`.  The default implementation
    /// wraps the hint in dimming (`\x1b[2m … \x1b[0m`).
    fn style(&self, input: &dyn std::fmt::Display, writer: &mut dyn std::io::Write) {
        let _ = write!(writer, "\x1b[2m{input}\x1b[0m");
    }
}

/// Query the terminal width (columns) via `ioctl(TIOCGWINSZ)` on the output
/// fd. This is the raw syscall wrapper -- prefer the cached
/// `Context::term_cols` field for hot paths. Falls back to 80 columns on
/// failure.
fn terminal_width(fd: i32) -> u16 {
    // SAFETY: TIOCGWINSZ writes a `winsize` we zero-initialize; we read it only
    // on success (rc == 0) and guard against a zero column count.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        // Query fd 1 (stdout); the editor's streams dup these descriptors.
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col
        } else {
            80
        }
    }
}

/// Recover `&mut Context` from an editor's client-data pointer.
fn context_from<'a>(el: *mut libedit_sys::EditLine) -> &'a mut Context {
    let mut data: usize = 0;
    let rc = unsafe { shim::el_get_ptr(el, EL_CLIENTDATA as i32, &mut data) };
    debug_assert!(rc == 0 && data != 0);
    unsafe { &mut *(data as *mut Context) }
}

fn new_line_context<'a>(
    el: *mut libedit_sys::EditLine,
    line_buf: &'a mut String,
    term_cols: u16,
) -> LineContext<'a> {
    line_buf.clear();

    let (char_count, cursor_pos) = unsafe {
        let info = &*(libedit_sys::el_wline(el));
        line_and_cursor(info, line_buf)
    };
    LineContext::new(line_buf, char_count, cursor_pos, term_cols)
}

fn exec_handler(
    el: *mut libedit_sys::EditLine,
    insert_buf: &mut WCString,
    output_stream: *mut FILE,
    line_buf: &mut String,
    term_cols: u16,
    handler: &dyn EventHandler,
) -> Action {
    insert_buf.clear();
    let mut insert_writer = WCStringWriter::new(insert_buf);
    let mut output_writer = OutWriter::new(output_stream);
    let mut line_ctx = new_line_context(el, line_buf, term_cols);
    let res = handler.handle(&mut line_ctx, &mut insert_writer, &mut output_writer);
    insert_buf.terminate();
    if !insert_buf.is_empty() {
        unsafe { el_winsertstr(el, insert_buf.as_ptr()) };
    }
    res
}

/// Extract the current line text into `line` and return the cursor byte-offset
/// within it.
///
/// The wide (`wchar_t`) buffer is an array of `u32` code units; each is decoded
/// to a Rust `char` (invalid scalars become U+FFFD). libedit reports the cursor
/// as a `wchar_t`-unit offset (a char index), which we translate into a byte
/// offset into the UTF-8 `line` we build, so callers can keep using byte-based
/// slicing (`line[..cursor]`) and end-of-line checks (`cursor >= line.len()`).
///
/// `line` is cleared before use, so the caller may reuse a buffer across calls.
///
/// # Safety
/// `info` must be a valid `LineInfoW` returned by `el_wline`.
/// The pointers are guaranteed valid for the lifetime of and editline instance
unsafe fn line_and_cursor(info: &LineInfoW, line: &mut String) -> (usize, usize) {
    let buffer = info.buffer;
    let lastchar = info.lastchar;
    let cursor = info.cursor;

    // Lengths/offsets are in `wchar_t` units (i.e. char indices), not bytes.
    // Use integer-cast arithmetic rather than `.offset_from()`: the three
    // pointers come from libedit's internal C allocation and therefore do not
    // share Rust provenance
    let len = (lastchar as usize - buffer as usize) / std::mem::size_of::<WChar>();
    let cursor_pos = (cursor as usize - buffer as usize) / std::mem::size_of::<WChar>();
    let units = unsafe { std::slice::from_raw_parts(buffer, len) };

    for u in units.iter().copied() {
        line.push(char_from_c(u));
    }
    (len, cursor_pos)
}

fn prompt_to_wide(s: &str, buf: &mut WCString) {
    let bytes = s.as_bytes();
    // s.len() (byte count) >= char count for UTF-8. +5 covers the NUL
    // terminator (+1) plus up to two escape-sequence delimiter pairs (+4),
    // matching the typical colored-prompt case without reallocation.
    buf.reserve_exact(s.len() + 5);
    if !bytes.contains(&0x1b) {
        // Fast path: no escapes, just widen each char directly.
        buf.push_str(s);
        return;
    }
    let delim = PROMPT_ESC_DELIM;
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
            buf.push_scalar(delim);
            // Escape bytes are ASCII, so each byte maps 1:1 to a wchar_t.
            buf.extend(bytes[start..i].iter().map(|&b| b as WChar));
            buf.push_scalar(delim);
        } else {
            // Decode one UTF-8 character and push as wchar_t.
            let ch_len = utf8_char_len(bytes[i]);
            let end = (i + ch_len).min(bytes.len());
            // SAFETY: bytes come from s.as_bytes() above
            let s = unsafe { std::str::from_utf8_unchecked(&bytes[i..end]) };
            for c in s.chars() {
                buf.push(c);
            }
            i = end;
        }
    }
    buf.push_nul();
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
unsafe fn open_std_stream(fd: i32, mode: &CStr) -> (*mut FILE, i32) {
    let dup_fd = unsafe { libc::dup(fd) };
    if dup_fd < 0 {
        return (std::ptr::null_mut(), dup_fd);
    }
    let stream = unsafe { libc::fdopen(dup_fd, mode.as_ptr()) };
    if stream.is_null() {
        // fdopen failed: it did not take ownership of dup_fd, so close it
        // ourselves to avoid leaking the descriptor.
        unsafe { libc::close(dup_fd) };
        return (std::ptr::null_mut(), -1);
    }
    (stream, dup_fd)
}

/// Close any non-null streams in `streams`.
///
/// # Safety
/// Each non-null pointer must be a live `FILE*` previously returned by
/// [`open_std_stream`] and not already closed.
unsafe fn close_streams(streams: &[(*mut FILE, i32); 3]) {
    for &s in streams {
        if !s.0.is_null() {
            unsafe { libc::fclose(s.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delimiter as a `char` for building expected strings.
    #[allow(clippy::unnecessary_cast)]
    fn d() -> char {
        PROMPT_ESC_DELIM as u8 as char
    }

    #[test]
    fn prompt_to_wide_wraps_sequences() {
        // Bold green "> " with a reset after it.
        let input = "\x1b[1;32mcaf\u{00e9}>\x1b[0m ";
        let expected = format!("{d}\x1b[1;32m{d}caf\u{00e9}>{d}\x1b[0m{d} ", d = d());
        let mut buf = WCString::new();
        prompt_to_wide(input, &mut buf);
        assert_eq!(buf.to_string_lossy(), expected);
    }
}
