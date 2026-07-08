//! Integration tests for the safe `libedit` wrapper crate.
//!
//! These tests don't require a terminal -- they test the non-interactive
//! parts of the API that can be exercised programmatically.
//!
//! **Important:** libedit is **not thread-safe**. It uses global signal
//! handlers and static state internally. All tests must be serialized.

#[cfg(target_os = "linux")]
use libedit::hint::Hint;
use libedit::suggestion::Suggestion;
#[cfg(target_os = "linux")]
use libedit::Completion;
use libedit::{EditLine, History, LineContext, Tokenizer};
use std::sync::Mutex;

static LIBEDIT_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LIBEDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn editline_init_and_drop() {
    let _guard = lock();
    let el = EditLine::new("test").unwrap();
    drop(el);
}

/// Count the process's currently open file descriptors (Linux-only).
#[cfg(target_os = "linux")]
fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

/// Regression test: creating and dropping many editors must not leak file
/// descriptors. Each `EditLine::new` dups fds 0/1/2 into `FILE*` streams;
/// `Drop` must `fclose` them so the descriptor count stays flat.
#[cfg(target_os = "linux")]
#[test]
fn editline_does_not_leak_fds() {
    let _guard = lock();

    // Warm up once so any one-time allocations are already done.
    drop(EditLine::new("test").unwrap());

    let before = open_fd_count();
    for _ in 0..64 {
        let el = EditLine::new("test").unwrap();
        drop(el);
    }
    let after = open_fd_count();

    assert_eq!(
        before, after,
        "leaked file descriptors: {before} before vs {after} after 64 create/drop cycles"
    );
}

#[test]
fn editline_set_and_get_editmode() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    el.set_edit_mode(true).unwrap();
    assert!(el.edit_mode().unwrap());
}

#[test]
fn editline_set_int_via_consts() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    el.set_int(libedit::consts::EL_EDITMODE as i32, 1).unwrap();
    let mode = el.get_int(libedit::consts::EL_EDITMODE as i32).unwrap();
    assert_eq!(mode, 1);
}

#[test]
fn editline_attach_history() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    let mut h = History::new();
    el.set_history(&mut h).unwrap();
}

#[test]
fn editline_set_editor_mode() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    el.set_editor(libedit::Editor::Vi).unwrap();
    el.set_editor(libedit::Editor::Emacs).unwrap();
}

#[test]
fn editline_beep_is_a_noop_without_tty() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    // Should not panic or crash even when not attached to a terminal.
    el.beep();
}

#[test]
fn editline_register_candidate_styler() {
    let _guard = lock();
    use std::fmt::Write;
    let mut el = EditLine::new("test").unwrap();
    // Registering an append-style styler must succeed.
    el.set_candidate_styler(|cand: &str, out: &mut String| {
        let _ = write!(out, "[{cand}]");
    });
}

#[test]
fn editline_register_suggester() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    // Registering a suggester (which binds many keys) must succeed.
    el.set_suggester(|ctx: &LineContext| {
        if ctx.line() == "he" {
            Some(Suggestion::new("lp"))
        } else {
            None
        }
    })
    .unwrap();
    // Styling should succeed. (`set_suggester` already exercised `bind_key`
    // internally by binding the printable keys and accept keys to our
    // registered functions.)
    el.set_suggestion_style("\x1b[2m", "\x1b[0m");
}

#[test]
fn editline_bind_key_rejects_unknown_function() {
    let _guard = lock();
    let mut el = EditLine::new("test").unwrap();
    // Binding to a function name that isn't registered must return an error
    // rather than panicking or silently succeeding.
    assert!(el.bind_key("^L", "no-such-editor-function-xyz").is_err());
}

#[test]
fn editline_writer_write_and_flush() {
    let _guard = lock();
    use std::io::Write;
    let mut el = EditLine::new("test").unwrap();
    // The Write impl over libedit's output stream must accept writes and
    // flush without error, even when not attached to a real terminal.
    {
        let mut out = el.output();
        write!(out, "hello {}", 42).unwrap();
        out.flush().unwrap();
    }
    {
        let mut err = el.error_output();
        writeln!(err, "diagnostic").unwrap();
        err.flush().unwrap();
    }
}

#[test]
fn history_init_and_drop() {
    let _guard = lock();
    let h = History::new();
    assert!(h.is_empty());
    drop(h);
}

#[test]
fn history_add_and_retrieve() {
    let _guard = lock();
    let mut h = History::new();
    h.add("first command").unwrap();
    assert_eq!(h.len(), 1);
    assert!(!h.is_empty());
    let first = h.first().unwrap();
    assert_eq!(first, "first command");
}

#[test]
fn history_clear() {
    let _guard = lock();
    let mut h = History::new();
    h.add("temp").unwrap();
    assert_eq!(h.len(), 1);
    h.clear();
    assert!(h.is_empty());
    assert_eq!(h.len(), 0);
    assert!(h.first().is_err());
}

#[test]
fn history_empty_first_error() {
    let _guard = lock();
    let h = History::new();
    assert!(h.first().is_err());
}

#[test]
fn history_with_size_eviction() {
    let _guard = lock();
    let mut h = History::with_size(2);
    h.add("one").unwrap();
    h.add("two").unwrap();
    h.add("three").unwrap();
    // Capped at 2 entries; oldest ("one") evicted.
    assert_eq!(h.len(), 2);
    assert_eq!(h.first().unwrap(), "three");
}

#[test]
fn history_unique_suppresses_consecutive_duplicates() {
    let _guard = lock();
    let mut h = History::with_size(100);
    h.set_unique(true);
    h.add("same").unwrap();
    h.add("same").unwrap();
    h.add("same").unwrap();
    // Consecutive duplicates collapse to a single entry.
    assert_eq!(h.len(), 1);
    assert_eq!(h.first().unwrap(), "same");
}

#[test]
fn history_unique_keeps_nonadjacent_duplicates() {
    let _guard = lock();
    let mut h = History::with_size(100);
    h.set_unique(true);
    h.add("a").unwrap();
    h.add("b").unwrap();
    h.add("a").unwrap();
    // Only *consecutive* dups are suppressed; "a" reappears after "b".
    assert_eq!(h.len(), 3);
    assert_eq!(h.first().unwrap(), "a");
}

#[test]
fn history_without_unique_keeps_all_duplicates() {
    let _guard = lock();
    let mut h = History::with_size(100);
    // Default (unique disabled): every add is retained.
    h.add("dup").unwrap();
    h.add("dup").unwrap();
    assert_eq!(h.len(), 2);
}

#[test]
fn history_save_and_load_roundtrip() {
    let _guard = lock();

    // Unique temp path for this test.
    let mut path = std::env::temp_dir();
    path.push(format!("libedit_hist_test_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // Save three entries.
    {
        let mut h = History::with_size(100);
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();
        h.save(&path).expect("save");
    }

    // Load them into a fresh history.
    {
        let mut h = History::with_size(100);
        assert!(h.is_empty());
        h.load(&path).expect("load");
        assert_eq!(h.len(), 3, "all entries should be restored");
        // Most-recent-first: `first` returns the last one added/loaded.
        assert_eq!(h.first().unwrap(), "charlie");
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn history_load_missing_file_errors() {
    let _guard = lock();
    let mut h = History::new();
    let missing = std::path::Path::new("/nonexistent/dir/does_not_exist_hist");
    assert!(h.load(missing).is_err());
}

#[test]
fn tokenizer_basic() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize("one two three").unwrap();
    assert_eq!(words, vec!["one", "two", "three"]);
}

#[test]
fn tokenizer_empty() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize("").unwrap();
    assert!(words.is_empty());
}

#[test]
fn tokenizer_reset_and_reuse() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize("a b").unwrap();
    assert_eq!(words, vec!["a", "b"]);
    t.reset();
    let words = t.tokenize("x y z").unwrap();
    assert_eq!(words, vec!["x", "y", "z"]);
}

#[test]
fn tokenizer_double_quotes_group_words() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize(r#"deploy "my server" now"#).unwrap();
    assert_eq!(words, vec!["deploy", "my server", "now"]);
}

#[test]
fn tokenizer_single_quotes_group_words() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize("set tag='v1 rc'").unwrap();
    assert_eq!(words, vec!["set", "tag=v1 rc"]);
}

#[test]
fn tokenizer_backslash_escapes_space() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize(r"a\ b c").unwrap();
    assert_eq!(words, vec!["a b", "c"]);
}

#[test]
fn tokenizer_backslash_in_double_quotes_is_kept() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    // Inside double quotes a backslash before an ordinary char is literal, so
    // both the backslash and the char are kept (matching sh / libedit).
    let words = t.tokenize(r#""a\b""#).unwrap();
    assert_eq!(words, vec![r"a\b"]);
}

#[test]
fn tokenizer_empty_quotes_produce_empty_token() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let words = t.tokenize(r#"'' x"#).unwrap();
    assert_eq!(words, vec!["", "x"]);
}

#[test]
fn tokenizer_unterminated_quote_errors() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    assert!(t.tokenize(r#"open "unfinished"#).is_err());
}

#[test]
fn tokenizer_preserves_multibyte_tokens() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    // Non-ASCII content must round-trip intact (no lossy conversion).
    let words = t.tokenize("café \"naïve über\"").unwrap();
    assert_eq!(words, vec!["café", "naïve über"]);
}

#[test]
fn tokenizer_rejects_non_ascii_separators() {
    let _guard = lock();
    assert!(Tokenizer::new(Some("\u{2003}")).is_err());
}

#[test]
fn tokenizer_accepts_owned_string_input() {
    let _guard = lock();
    let mut t = Tokenizer::new(None).unwrap();
    let owned = String::from("one two");
    let words = t.tokenize(owned).unwrap();
    assert_eq!(words, vec!["one", "two"]);
}

/// Regression test for a prompt-rendering segfault.
///
/// `EL_PROMPT` expects a function pointer, not a string. An earlier version
/// passed the prompt as a string, which made libedit call into the string's
/// bytes as code and crash -- but ONLY when a real terminal caused the prompt
/// to be rendered. Piped (non-TTY) input never renders the prompt, so a
/// simple pipe test misses it. This test drives `readline` under a real PTY
/// via `forkpty` and asserts the child exits normally rather than dying from
/// SIGSEGV.
#[cfg(target_os = "linux")]
#[test]
fn readline_prompt_does_not_segfault_on_tty() {
    let _guard = lock();

    let mut master: libc::c_int = 0;
    // SAFETY: forkpty allocates a PTY and forks. In the child, fds 0/1/2 are
    // wired to the PTY slave -- exactly what `EditLine` reads/writes.
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(pid >= 0, "forkpty failed");

    if pid == 0 {
        // Child: render a prompt and read one line. This is the code path
        // that used to segfault. Any panic/abort here shows up as a non-zero
        // or signalled exit status observed by the parent.
        let mut el = EditLine::new("test").expect("init");
        let _ = el.readline("prompt> ").expect("readline");
        // Exit immediately without running the normal test harness teardown.
        unsafe { libc::_exit(0) };
    }

    // Parent: feed one line, then close the PTY to signal EOF.
    let line = b"hello\n";
    unsafe {
        libc::write(master, line.as_ptr() as *const libc::c_void, line.len());
    }

    // Reap the child and inspect how it terminated.
    let mut status: libc::c_int = 0;
    // Give the child a moment, then wait for it.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(master) };
    assert_eq!(waited, pid, "waitpid failed");

    // WIFSIGNALED: child was killed by a signal (e.g. SIGSEGV = 11).
    let signalled = (status & 0x7f) != 0 && (status & 0x7f) != 0x7f;
    let term_sig = status & 0x7f;
    assert!(
        !signalled,
        "child terminated by signal {term_sig} (SIGSEGV is 11) -- prompt handling regressed"
    );
}

/// Run `body` inside a child process attached to a fresh PTY, feed it `input`
/// bytes, and return `(exit_code, term_signal)`. The child must exit itself.
#[cfg(target_os = "linux")]
fn run_in_pty(input: &[u8], body: impl FnOnce()) -> (i32, i32) {
    let mut master: libc::c_int = 0;
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(pid >= 0, "forkpty failed");

    if pid == 0 {
        body();
        unsafe { libc::_exit(0) };
    }

    // Set master to non-blocking so drain reads don't deadlock with the
    // child's get-character trampoline (which writes ghost text between
    // blocking reads on the slave side).
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL, 0) };
    unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) };

    // Feed input one byte at a time, draining output between each to avoid
    // PTY buffer deadlocks.
    let mut buf = [0u8; 1024];
    for &byte in input {
        unsafe {
            libc::write(master, [byte].as_ptr() as *const libc::c_void, 1);
        }
        // Use poll to wait for available output with a short timeout.
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready <= 0 {
                break;
            }
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }

    // Final drain -- wait a bit longer for the child to finish processing.
    let mut pfd = libc::pollfd {
        fd: master,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready <= 0 {
            break;
        }
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }

    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(master) };
    assert_eq!(waited, pid, "waitpid failed");
    let signal = status & 0x7f;
    let code = (status >> 8) & 0xff;
    (code, signal)
}

/// Like [`run_in_pty`], but drives `input` one byte at a time (with a short
/// settle read between bytes so the child's per-keystroke output is captured
/// in order) and returns everything the child wrote to the terminal.
///
/// This is the capture harness for debugging the suggestion renderer: feed a
/// keystroke script, get back the exact byte stream (including ANSI escapes),
/// and decode it with [`visualize_bytes`] to see what was emitted per key.
#[cfg(target_os = "linux")]
fn capture_pty(input: &[u8], body: impl FnOnce()) -> Vec<u8> {
    let mut master: libc::c_int = 0;
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(pid >= 0, "forkpty failed");

    if pid == 0 {
        body();
        unsafe { libc::_exit(0) };
    }

    // Set master to non-blocking so drain reads don't deadlock with the
    // child's get-character trampoline.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL, 0) };
    unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) };

    let mut captured = Vec::new();
    let mut buf = [0u8; 1024];
    // Feed one byte, then drain whatever the child echoed/rendered for it.
    for &byte in input {
        unsafe {
            libc::write(master, [byte].as_ptr() as *const libc::c_void, 1);
        }
        // Use poll to wait for available output (with a short timeout so we
        // don't deadlock if the child consumed the byte without producing any
        // output yet).
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready <= 0 {
                break;
            }
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            captured.extend_from_slice(&buf[..n as usize]);
        }
    }

    // Close the master to send EOF to the child so it exits readline.
    unsafe { libc::close(master) };

    let mut status: libc::c_int = 0;
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    captured
}

/// Render captured terminal bytes with control characters made visible, so a
/// test failure prints a readable escape stream instead of raw control codes.
#[cfg(target_os = "linux")]
fn visualize_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 16);
    for &b in bytes {
        match b {
            0x1b => out.push_str("<ESC>"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x07 => out.push_str("<BEL>"),
            0x08 => out.push_str("<BS>"),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("<\\x{other:02x}>")),
        }
    }
    out
}

/// Diagnostic capture: type a full word one key at a time with an inline
/// suggester active, and dump the exact terminal byte stream. This is not a
/// strict pass/fail assertion of pixels -- it's a replayable record used to
/// debug the ghost-text cursor math. Run with `--nocapture` to see the stream:
///
/// ```sh
/// cargo test -p libedit --test integration suggestion_capture -- --nocapture
/// ```
#[cfg(target_os = "linux")]
#[test]
fn suggestion_capture_stream() {
    let _guard = lock();
    // Type "history" char-by-char; suggester completes "hist" -> "history".
    let captured = capture_pty(b"history", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_suggester(|ctx: &LineContext| {
            let line = ctx.line();
            let full = "history";
            if !line.is_empty() && full.starts_with(line) && full.len() > line.len() {
                Some(Suggestion::new(&full[line.len()..]))
            } else {
                None
            }
        })
        .expect("set_suggester");
        el.set_suggestion_style("\x1b[2m", "\x1b[0m");
        let _ = el.readline("repl> ");
        unsafe { libc::_exit(0) };
    });

    // Print the decoded stream for inspection (visible with --nocapture).
    println!("---- captured terminal stream ----");
    println!("{}", visualize_bytes(&captured));
    println!("----------------------------------");

    // Sanity: the child must have emitted our faint SGR at least once, proving
    // the suggestion render path ran. (Pixel-accurate placement is verified by
    // manual inspection of the decoded stream above.)
    assert!(
        captured.windows(4).any(|w| w == b"\x1b[2m"),
        "expected a faint SGR (ghost text) in the captured stream"
    );
}

/// Tab completion inserts the unambiguous completion into the returned line.
///
/// The child registers a completer over `{show, set}`, types `se` then Tab
/// (which should complete to `set ` since that's the only `se*` match), then
/// Enter. It exits 0 only if the resulting line starts with `set`.
#[cfg(target_os = "linux")]
#[test]
fn completion_inserts_unambiguous_match() {
    let _guard = lock();
    // "se" + Tab + Enter
    let (code, signal) = run_in_pty(b"se\t\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_completer(|ctx: &LineContext| {
            let matches: Vec<String> = ["show", "set"]
                .iter()
                .filter(|c| c.starts_with(ctx.word()))
                .map(|c| c.to_string())
                .collect();
            Completion::new(matches)
        })
        .expect("set_completer");
        let line = el.readline("> ").expect("readline").unwrap_or_default();
        // Exit code encodes success/failure for the parent to observe.
        if line.trim_start().starts_with("set") {
            unsafe { libc::_exit(0) };
        } else {
            unsafe { libc::_exit(3) };
        }
    });
    assert_eq!(signal, 0, "child died from signal {signal}");
    assert_eq!(
        code, 0,
        "completed line did not start with `set` (code {code})"
    );
}

/// An ambiguous Tab completion lists styled candidates without crashing and
/// without the styling escapes leaking into the returned line.
///
/// The child registers `{show, set}` plus a styler that wraps each candidate
/// in ANSI, types `s` then Tab (ambiguous -> list is printed via libedit's own
/// output stream), then finishes typing `et` and Enter. It exits 0 only if the
/// resulting line is exactly `set` (i.e. the listing/ANSI did not corrupt it).
#[cfg(target_os = "linux")]
#[test]
fn completion_lists_styled_candidates() {
    let _guard = lock();
    use std::fmt::Write;
    // "s" + Tab (list show/set) + "et" + Enter
    let (code, signal) = run_in_pty(b"s\tet\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_completer(|ctx: &LineContext| {
            let matches: Vec<String> = ["show", "set"]
                .iter()
                .filter(|c| c.starts_with(ctx.word()))
                .map(|c| c.to_string())
                .collect();
            Completion::new(matches)
        })
        .expect("set_completer");
        el.set_candidate_styler(|cand: &str, out: &mut String| {
            let _ = write!(out, "\x1b[36m{cand}\x1b[0m");
        });
        let line = el.readline("> ").expect("readline").unwrap_or_default();
        if line.trim() == "set" {
            unsafe { libc::_exit(0) };
        } else {
            unsafe { libc::_exit(5) };
        }
    });
    assert_eq!(signal, 0, "child died from signal {signal}");
    assert_eq!(
        code, 0,
        "styled candidate listing corrupted the line (code {code})"
    );
}

/// A completer that panics must not unwind across the C boundary (which is
/// UB). The trampoline wraps the callback in `catch_unwind`, so pressing Tab
/// should be a no-op (beep) and the session should continue and exit cleanly
/// rather than aborting or segfaulting.
#[cfg(target_os = "linux")]
#[test]
fn completion_panic_is_contained() {
    let _guard = lock();
    // Type "x", press Tab (triggers the panicking completer), then Enter.
    let (_code, signal) = run_in_pty(b"x\t\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_completer(|_ctx: &LineContext| -> Completion {
            panic!("boom inside completer");
        })
        .expect("set_completer");
        // Should return normally despite the panic during Tab.
        let _ = el.readline("> ").expect("readline");
        unsafe { libc::_exit(0) };
    });
    // The key assertion: not killed by a signal (no abort/SIGSEGV from
    // unwinding across FFI).
    assert_eq!(
        signal, 0,
        "child terminated by signal {signal} -- panic escaped the FFI boundary"
    );
}

/// A registered hinter is exercised during editing (its right-prompt
/// trampoline fires on each keystroke) and the session completes cleanly.
#[cfg(target_os = "linux")]
#[test]
fn hinter_renders_without_crashing() {
    let _guard = lock();
    // Type "se" (hinter fires per keystroke), then Enter.
    let (code, signal) = run_in_pty(b"se\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_hinter(|ctx: &LineContext| {
            if ctx.word() == "se" {
                Some(Hint::new("t  -- set a value"))
            } else {
                None
            }
        });
        let line = el.readline("> ").expect("readline").unwrap_or_default();
        // The hint is a right-prompt overlay; it must NOT end up in the line.
        if line.trim() == "se" {
            unsafe { libc::_exit(0) };
        } else {
            unsafe { libc::_exit(4) };
        }
    });
    assert_eq!(signal, 0, "child died from signal {signal}");
    assert_eq!(
        code, 0,
        "hint text leaked into the returned line (code {code})"
    );
}

/// A panicking hinter must not unwind across the C boundary. Pressing keys
/// triggers the rprompt trampoline; the session should still exit cleanly.
#[cfg(target_os = "linux")]
#[test]
fn hinter_panic_is_contained() {
    let _guard = lock();
    let (_code, signal) = run_in_pty(b"x\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_hinter(|_ctx: &LineContext| -> Option<Hint> {
            panic!("boom inside hinter");
        });
        let _ = el.readline("> ").expect("readline");
        unsafe { libc::_exit(0) };
    });
    assert_eq!(
        signal, 0,
        "child terminated by signal {signal} -- hinter panic escaped the FFI boundary"
    );
}

/// Inline autosuggestion: typing a prefix and pressing Ctrl-F (accept) commits
/// the suggested suffix into the buffer without crashing.
///
/// The child suggests "lp" for the line "he". It types "he", presses Ctrl-F to
/// accept, then Enter. It exits 0 only if the resulting line is "help" -- i.e.
/// the ghost text was committed on accept (and did not leak before accept).
#[cfg(target_os = "linux")]
#[test]
fn suggestion_accept_commits_suffix() {
    let _guard = lock();
    // "he" + Ctrl-F (accept) + Enter
    let (code, signal) = run_in_pty(b"he\x06\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_suggester(|ctx: &LineContext| {
            if ctx.line() == "he" {
                Some(Suggestion::new("lp"))
            } else {
                None
            }
        })
        .expect("set_suggester");
        el.set_suggestion_style("\x1b[2m", "\x1b[0m");
        let line = el.readline("> ").expect("readline").unwrap_or_default();
        if line.trim() == "help" {
            unsafe { libc::_exit(0) };
        } else {
            unsafe { libc::_exit(6) };
        }
    });
    assert_eq!(signal, 0, "child died from signal {signal}");
    assert_eq!(
        code, 0,
        "suggestion was not committed on accept (code {code})"
    );
}

/// Without pressing accept, the suggestion must NOT be part of the submitted
/// line -- it's purely visual ghost text.
#[cfg(target_os = "linux")]
#[test]
fn suggestion_not_in_line_without_accept() {
    let _guard = lock();
    // "he" + Enter (no accept)
    let (code, signal) = run_in_pty(b"he\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_suggester(|ctx: &LineContext| {
            if ctx.line() == "he" {
                Some(Suggestion::new("lp"))
            } else {
                None
            }
        })
        .expect("set_suggester");
        let line = el.readline("> ").expect("readline").unwrap_or_default();
        // The buffer should be exactly what was typed, not "help".
        if line.trim() == "he" {
            unsafe { libc::_exit(0) };
        } else {
            unsafe { libc::_exit(7) };
        }
    });
    assert_eq!(signal, 0, "child died from signal {signal}");
    assert_eq!(
        code, 0,
        "ghost text leaked into the submitted line (code {code})"
    );
}

/// A suggester that panics must not unwind across the C boundary. Typing a
/// character triggers the keystroke trampoline; the session should still exit
/// cleanly.
#[cfg(target_os = "linux")]
#[test]
fn suggestion_panic_is_contained() {
    let _guard = lock();
    let (_code, signal) = run_in_pty(b"x\n", || {
        let mut el = EditLine::new("test").expect("init");
        el.set_suggester(|_ctx: &LineContext| -> Option<Suggestion> {
            panic!("boom inside suggester");
        })
        .expect("set_suggester");
        let _ = el.readline("> ").expect("readline");
        unsafe { libc::_exit(0) };
    });
    assert_eq!(
        signal, 0,
        "child terminated by signal {signal} -- suggester panic escaped the FFI boundary"
    );
}
