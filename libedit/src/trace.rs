//! Env-gated diagnostic tracing for the editor's output stream.
//!
//! When the `LIBEDIT_TRACE` environment variable is set to a file path, the
//! crate appends a human-readable log of everything it emits to libedit's
//! output stream, plus decision-level notes from the suggestion renderer.
//! Control bytes are rendered visibly (e.g. `<ESC>`, `\r`, `\n`) so an ANSI
//! escape stream can be read and diffed. When the variable is unset, every
//! entry point is a cheap early return -- there is no runtime cost.
//!
//! This is a debugging aid, not part of the public API. It exists so that
//! rendering bugs (stray cursor moves, leftover ghost bytes) can be inspected
//! from a replayable byte log rather than by eyeballing a live terminal.
//!
//! # Usage
//!
//! ```sh
//! LIBEDIT_TRACE=/tmp/libedit.trace cargo run --example repl
//! # ... reproduce the issue, then:
//! cat /tmp/libedit.trace
//! ```

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// The trace sink: `None` when tracing is disabled, `Some(file)` otherwise.
/// Resolved once from `LIBEDIT_TRACE` on first use.
static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Return the trace file if `LIBEDIT_TRACE` names one, opening (truncating) it
/// on first use. Returns `None` when tracing is disabled or the file can't be
/// opened.
fn sink() -> Option<&'static Mutex<File>> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("LIBEDIT_TRACE")?;
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Returns `true` if tracing is active. Callers may use this to skip building
/// expensive trace arguments.
pub(crate) fn enabled() -> bool {
    sink().is_some()
}

/// Render `bytes` with control characters made visible.
fn visualize(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 8);
    for &b in bytes {
        match b {
            0x1b => out.push_str("<ESC>"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x01 => out.push_str("<\\x01>"), // PROMPT_ESC_DELIM
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("<\\x{other:02x}>")),
        }
    }
    out
}

/// Log a chunk of raw output bytes under `tag`, with escapes visualized.
pub(crate) fn bytes(tag: &str, data: &[u8]) {
    let Some(sink) = sink() else { return };
    let line = format!("[{tag}] {}\n", visualize(data));
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

/// Log a free-form decision-level note (already formatted by the caller).
pub(crate) fn note(msg: &str) {
    let Some(sink) = sink() else { return };
    let line = format!("* {msg}\n");
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}
