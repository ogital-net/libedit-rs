//! Terminal capability queries -- no extra dependencies needed.
//!
//! These helpers answer the three questions every CLI asks before emitting
//! ANSI escapes: *Am I a terminal? How big? Should I use color?* They use
//! only `libc` (already a dependency) and standard environment variables, so
//! consumers don't need to pull in `atty`, `terminal_size`, or
//! `supports-color`.
//!
//! For the terminal dimensions while an editor is active, prefer
//! [`EditLine::terminal_size`](crate::EditLine::terminal_size) -- it uses the
//! same underlying syscall but is documented alongside the other editor
//! methods.

/// Returns `true` if file descriptor `fd` is connected to a terminal (TTY).
///
/// Common usage: `is_tty(1)` for stdout, `is_tty(0)` for stdin.
///
/// This is a thin wrapper over `libc::isatty`. Rust's
/// [`std::io::IsTerminal`] trait provides the same check on `Stdout` /
/// `Stdin` handles; this function is useful when you have a raw fd (e.g.
/// from libedit's stream) or want to avoid importing `std::io`.
pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) != 0 }
}

/// Returns `true` if the terminal likely supports ANSI color output.
///
/// Checks, in order:
/// 1. **`$NO_COLOR`** -- if set (to any value), returns `false`.
///    See <https://no-color.org/>.
/// 2. **stdout is not a TTY** -- returns `false` (piped output shouldn't
///    contain escapes).
/// 3. **`$TERM` is `"dumb"`** -- returns `false`.
/// 4. Otherwise returns `true`.
///
/// This covers the standard conventions and is sufficient for nearly all
/// real terminals (iTerm2, Terminal.app, xterm, Alacritty, Windows Terminal,
/// etc.). It does not query termcap/terminfo for the `Co` capability -- that
/// would require additional FFI and is rarely needed in practice.
pub fn supports_color() -> bool {
    // $NO_COLOR takes precedence unconditionally.
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // Not a terminal -> no color.
    if !is_tty(1) {
        return false;
    }
    // $TERM=dumb means the terminal can't handle escapes.
    if let Ok(term) = std::env::var("TERM") {
        if term == "dumb" {
            return false;
        }
    }
    true
}

/// Query the terminal dimensions `(columns, rows)` for the given fd.
///
/// Returns `None` if the fd is not a terminal or the ioctl fails. For a
/// convenient fallback-included version, see
/// [`EditLine::terminal_size`](crate::EditLine::terminal_size).
pub fn size(fd: i32) -> Option<(usize, usize)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            Some((ws.ws_col as usize, ws.ws_row as usize))
        } else {
            None
        }
    }
}
