//! Inline autosuggestions (fish/`zsh-autosuggestions`-style ghost text) for
//! [`EditLine`](crate::EditLine).
//!
//! A [`Suggester`] proposes a completion of the *current line* -- for example
//! the most recent matching history entry, or the single command that extends
//! what's been typed. The suggested suffix is shown, dimmed, immediately after
//! the cursor as the user types, and can be accepted with a key (by default
//! Ctrl-F or Right-arrow at end of line).
//!
//! # How this differs from a [`Hinter`](crate::hint::Hinter)
//!
//! A [`Hinter`](crate::hint::Hinter) renders through libedit's right-hand prompt
//! (`EL_RPROMPT`), so its text sits at the right edge of the line -- good for
//! a persistent status or help string, but visually detached from the cursor.
//! A [`Suggester`] instead renders **ghost text right after the cursor**, like
//! fish shell, by printing dimmed output past the caret on each keystroke and
//! moving the cursor back. Crucially the suggestion is *never* part of the
//! edit buffer, so pressing Enter, End, or a kill-line binding won't capture
//! it -- only the explicit accept key commits it.
//!
//! This mirrors the design used by LLDB's libedit integration.
//!
//! Register one with
//! [`EditLine::set_suggester`](crate::EditLine::set_suggester).
//!
//! # Example
//!
//! ```no_run
//! use libedit::{EditLine, LineContext};
//! use libedit::suggestion::Suggestion;
//!
//! let mut el = EditLine::new("cli").unwrap();
//! let history = ["show interfaces", "show version"];
//! el.set_suggester(move |ctx: &LineContext| {
//!     let line = ctx.line();
//!     if line.is_empty() {
//!         return None;
//!     }
//!     // Suggest the remainder of the first history entry that starts with
//!     // the current line.
//!     history
//!         .iter()
//!         .find(|h| h.starts_with(line) && h.len() > line.len())
//!         .map(|h| Suggestion::new(&h[line.len()..]))
//! }).unwrap();
//! ```

use crate::completion::LineContext;

/// A proposed inline completion of the current line.
///
/// The text is the **suffix** to display after the cursor (the part the user
/// has not yet typed), not the whole line.
#[derive(Debug, Clone)]
pub struct Suggestion {
    text: String,
}

impl Suggestion {
    /// Create a suggestion from the suffix text to show after the cursor.
    ///
    /// Interior NUL bytes are stripped, since the text is handed to libedit as
    /// a C string. Newlines are also stripped, as a suggestion must render on
    /// the current line.
    pub fn new(text: impl Into<String>) -> Self {
        let mut text = text.into();
        text.retain(|c| c != '\0' && c != '\n' && c != '\r');
        Suggestion { text }
    }

    /// The suffix text to display after the cursor.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns `true` if the suggestion is empty (nothing to show).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

impl From<&str> for Suggestion {
    fn from(s: &str) -> Self {
        Suggestion::new(s)
    }
}

impl From<String> for Suggestion {
    fn from(s: String) -> Self {
        Suggestion::new(s)
    }
}

/// A source of inline autosuggestions for an [`EditLine`](crate::EditLine).
///
/// Implement this (or pass a closure) and register it with
/// [`EditLine::set_suggester`](crate::EditLine::set_suggester). It is invoked
/// on every keystroke during [`readline`](crate::EditLine::readline); keep it
/// fast and non-blocking. Returning `None` shows no suggestion.
pub trait Suggester {
    /// Return the suggested completion of the current line, or `None`.
    fn suggest(&mut self, ctx: &LineContext) -> Option<Suggestion>;
}

// Allow a plain closure to be used as a suggester for convenience.
impl<F> Suggester for F
where
    F: FnMut(&LineContext) -> Option<Suggestion>,
{
    fn suggest(&mut self, ctx: &LineContext) -> Option<Suggestion> {
        (self)(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::Suggestion;

    #[test]
    fn strips_control_chars() {
        let s = Suggestion::new("ab\0c\nd\re");
        assert_eq!(s.text(), "abcde");
    }

    #[test]
    fn empty_is_reported() {
        assert!(Suggestion::new("").is_empty());
        assert!(!Suggestion::new("x").is_empty());
    }

    #[test]
    fn from_conversions() {
        let a: Suggestion = "hi".into();
        let b: Suggestion = String::from("hi").into();
        assert_eq!(a.text(), "hi");
        assert_eq!(b.text(), "hi");
    }
}
