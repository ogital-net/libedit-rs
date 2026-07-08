//! Right-hand status / help text for [`EditLine`](crate::EditLine).
//!
//! A [`Hinter`] produces a short piece of text shown at the **right edge** of
//! the input line as the user types -- for example the description of the
//! command being entered, a mode indicator, or a brief help string.
//!
//! # How hints are rendered (and how they differ from suggestions)
//!
//! This crate renders hints via libedit's **right-hand prompt**
//! (`EL_RPROMPT`), which libedit redraws live on each keystroke. The hint
//! therefore appears anchored at the right margin of the terminal line, *not*
//! immediately after the cursor. That makes it well suited to a persistent
//! status or help annotation, but it is deliberately not fish-style ghost
//! text.
//!
//! For an inline suggestion that continues the line right after the cursor
//! (like fish shell or `zsh-autosuggestions`), use the
//! [`suggestion`](crate::suggestion) module and
//! [`EditLine::set_suggester`](crate::EditLine::set_suggester) instead. Hints
//! and suggestions are independent and may both be active at once -- a hint on
//! the right, a suggestion at the cursor.
//!
//! Register a hinter with
//! [`EditLine::set_hinter`](crate::EditLine::set_hinter).
//!
//! # Example
//!
//! ```no_run
//! use libedit::{EditLine, LineContext};
//! use libedit::hint::Hint;
//!
//! let mut el = EditLine::new("cli").unwrap();
//! el.set_hinter(|ctx: &LineContext| {
//!     match ctx.line() {
//!         "sh" => Some(Hint::new("ow  -- display state")),
//!         _ => None,
//!     }
//! });
//! ```

use crate::completion::LineContext;

/// A hint to display to the right of the input line.
#[derive(Debug, Clone)]
pub struct Hint {
    text: String,
}

impl Hint {
    /// Create a hint from display text.
    ///
    /// Any interior NUL bytes are stripped, since the text is handed to
    /// libedit as a C string.
    pub fn new(text: impl Into<String>) -> Self {
        let mut text = text.into();
        text.retain(|c| c != '\0');
        Hint { text }
    }

    /// The hint's display text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl From<&str> for Hint {
    fn from(s: &str) -> Self {
        Hint::new(s)
    }
}

impl From<String> for Hint {
    fn from(s: String) -> Self {
        Hint::new(s)
    }
}

/// A source of inline hints for an [`EditLine`](crate::EditLine).
///
/// Implement this (or pass a closure) and register it with
/// [`EditLine::set_hinter`](crate::EditLine::set_hinter). The hinter is
/// invoked on every keystroke during [`readline`](crate::EditLine::readline);
/// keep it fast and non-blocking. Returning `None` shows no hint.
pub trait Hinter {
    /// Return the hint for the current line state, or `None` for no hint.
    fn hint(&mut self, ctx: &LineContext) -> Option<Hint>;
}

// Allow a plain closure to be used as a hinter for convenience.
impl<F> Hinter for F
where
    F: FnMut(&LineContext) -> Option<Hint>,
{
    fn hint(&mut self, ctx: &LineContext) -> Option<Hint> {
        (self)(ctx)
    }
}
