//! Tab-completion support for [`EditLine`](crate::EditLine).
//!
//! Register a completer with
//! [`EditLine::set_completer`](crate::EditLine::set_completer). When the user
//! presses the completion key (Tab by default), libedit calls back into your
//! completer with the current line and cursor position; you return the set of
//! candidate completions for the word under the cursor.
//!
//! The behavior mirrors readline/bash-style completion:
//! - If exactly one candidate matches, it is inserted.
//! - If several share a longer common prefix than what's typed, that common
//!   prefix is inserted.
//! - If the candidates are ambiguous with no further common prefix, they are
//!   listed for the user.
//!
//! # Example
//!
//! ```no_run
//! use libedit::{EditLine, completion::{Completer, Completion, LineContext}};
//!
//! struct Commands(Vec<&'static str>);
//!
//! impl Completer for Commands {
//!     fn complete(&mut self, ctx: &LineContext) -> Completion {
//!         let word = ctx.word();
//!         let matches = self
//!             .0
//!             .iter()
//!             .filter(|c| c.starts_with(word))
//!             .map(|c| c.to_string())
//!             .collect();
//!         Completion::new(matches)
//!     }
//! }
//!
//! let mut el = EditLine::new("cli").unwrap();
//! el.set_completer(Commands(vec!["show", "set", "save"])).unwrap();
//! ```

/// The state of the input line at the moment completion is requested.
///
/// Provides the full line, the byte offset of the cursor, and a convenience
/// accessor for the "word" immediately preceding the cursor (split on ASCII
/// whitespace), which is the token most completers want to complete.
#[derive(Debug, Clone)]
pub struct LineContext {
    line: String,
    cursor: usize,
}

impl LineContext {
    pub(crate) fn new(line: String, cursor: usize) -> Self {
        // Clamp cursor into range and onto a char boundary for safety.
        let cursor = cursor.min(line.len());
        let cursor = (0..=cursor)
            .rev()
            .find(|&i| line.is_char_boundary(i))
            .unwrap_or(0);
        LineContext { line, cursor }
    }

    /// The full contents of the input line.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The byte offset of the cursor within [`line`](Self::line).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The text from the start of the current word up to the cursor.
    ///
    /// The word starts after the most recent ASCII-whitespace character
    /// before the cursor. This is the token a typical command completer
    /// should match against.
    pub fn word(&self) -> &str {
        let before = &self.line[..self.cursor];
        let start = before
            .rfind(|c: char| c.is_ascii_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        &before[start..]
    }

    /// The byte offset at which the current word begins.
    pub fn word_start(&self) -> usize {
        self.cursor - self.word().len()
    }
}

/// The result of a completion request: the candidate strings for the word
/// under the cursor.
///
/// Each candidate is the *full replacement word*, not just the suffix. For
/// example, if the user typed `sh` and the command is `show`, the candidate
/// should be `"show"` (the wrapper computes what to insert).
///
/// For data structures that already know the insertion text (e.g., a trie's
/// `extension()`), use [`Completion::with_insertion`] to skip the LCP
/// computation.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub(crate) candidates: Vec<String>,
    /// Pre-computed text to insert at the cursor. When `Some`, the editor
    /// inserts this directly instead of computing the longest common prefix
    /// from `candidates`. The candidates are still used for the ambiguous
    /// listing if non-empty.
    pub(crate) insertion: Option<String>,
}

impl Completion {
    /// Create a completion result from a list of candidate words.
    ///
    /// The editor computes the longest common prefix of the candidates and
    /// inserts whatever extends beyond what the user has already typed.
    /// If there is exactly one candidate, a trailing space is appended.
    pub fn new(candidates: Vec<String>) -> Self {
        Completion {
            candidates,
            insertion: None,
        }
    }

    /// Create a completion with a pre-computed insertion string.
    ///
    /// Use this when your data structure already knows what text to splice
    /// into the buffer (e.g., a trie's `extension()`) and you want to avoid
    /// the O(n) longest-common-prefix scan over all candidates.
    ///
    /// `insertion` is inserted at the cursor verbatim. `candidates` are shown
    /// to the user if the completion is ambiguous (multiple matches); pass an
    /// empty vec to suppress the listing.
    ///
    /// # Example (with `command-trie`)
    ///
    /// ```ignore
    /// let sub = trie.subtrie(ctx.word()).unwrap();
    /// if sub.is_unique() {
    ///     Completion::with_insertion(
    ///         format!("{} ", sub.extension()),
    ///         vec![],
    ///     )
    /// } else {
    ///     let names: Vec<String> = sub.iter().map(|(k, _)| k).collect();
    ///     Completion::with_insertion(sub.extension().to_string(), names)
    /// }
    /// ```
    pub fn with_insertion(insertion: impl Into<String>, candidates: Vec<String>) -> Self {
        Completion {
            candidates,
            insertion: Some(insertion.into()),
        }
    }

    /// An empty completion (no candidates).
    pub fn none() -> Self {
        Completion {
            candidates: Vec::new(),
            insertion: None,
        }
    }

    /// The candidate words.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    /// Returns `true` if there are no candidates and no insertion.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.insertion.is_none()
    }
}

/// A source of tab-completions for an [`EditLine`](crate::EditLine).
///
/// Implement this for your command set, then hand it to
/// [`EditLine::set_completer`](crate::EditLine::set_completer). The completer
/// is stored inside the editor and invoked on the same thread during
/// [`readline`](crate::EditLine::readline) -- it does not need to be `Send` or
/// `Sync`.
pub trait Completer {
    /// Return the candidate completions for the word under the cursor.
    fn complete(&mut self, ctx: &LineContext) -> Completion;
}

// Allow a plain closure to be used as a completer for convenience.
impl<F> Completer for F
where
    F: FnMut(&LineContext) -> Completion,
{
    fn complete(&mut self, ctx: &LineContext) -> Completion {
        (self)(ctx)
    }
}

/// Styles individual candidates for display when a Tab completion is
/// ambiguous.
///
/// Register one with
/// [`EditLine::set_candidate_styler`](crate::EditLine::set_candidate_styler).
/// It is called once per candidate and *appends* the display text for that
/// candidate into `out`. The wrapper applies no ANSI styling itself and writes
/// whatever was appended verbatim, so a consumer can add color escapes,
/// symbols, or padding as they see fit. The styling affects *display only* --
/// the raw candidate is what gets inserted into the line on a match.
///
/// Appending into a shared buffer (rather than returning an owned `String`)
/// lets the crate reuse one allocation across every candidate and every Tab
/// press. Implementations typically use [`write!`](std::write) with
/// [`std::fmt::Write`].
pub trait CandidateStyler {
    /// Append the display text for `candidate` into `out`.
    fn style(&mut self, candidate: &str, out: &mut String);
}

// Allow a plain closure to be used as a candidate styler for convenience.
impl<F> CandidateStyler for F
where
    F: FnMut(&str, &mut String),
{
    fn style(&mut self, candidate: &str, out: &mut String) {
        (self)(candidate, out)
    }
}

/// Compute the longest common prefix shared by all `candidates`.
///
/// Operates on chars so it never splits a UTF-8 sequence. Returns an empty
/// string if the slice is empty or the candidates share no common prefix.
///
/// This is a public utility so callers can use it in their own completion
/// logic (e.g., computing LCP from a filtered candidate list before passing
/// it to [`Completion::with_insertion`]).
pub fn longest_common_prefix<S: AsRef<str>>(candidates: &[S]) -> String {
    let mut iter = candidates.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let first = first.as_ref();
    let mut prefix_len = first.chars().count();
    for c in iter {
        let common = first
            .chars()
            .zip(c.as_ref().chars())
            .take_while(|(a, b)| a == b)
            .count();
        prefix_len = prefix_len.min(common);
        if prefix_len == 0 {
            break;
        }
    }
    first.chars().take(prefix_len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcp_basic() {
        let v = vec!["show".to_string(), "shutdown".to_string()];
        assert_eq!(longest_common_prefix(&v), "sh");
    }

    #[test]
    fn lcp_single() {
        let v = vec!["show".to_string()];
        assert_eq!(longest_common_prefix(&v), "show");
    }

    #[test]
    fn lcp_none() {
        let v = vec!["show".to_string(), "abort".to_string()];
        assert_eq!(longest_common_prefix(&v), "");
    }

    #[test]
    fn lcp_empty() {
        assert_eq!(longest_common_prefix::<String>(&[]), "");
    }

    #[test]
    fn lcp_unicode() {
        let v = vec!["café".to_string(), "cafétière".to_string()];
        assert_eq!(longest_common_prefix(&v), "café");
    }

    #[test]
    fn word_extraction() {
        let ctx = LineContext::new("show ip ro".to_string(), 10);
        assert_eq!(ctx.word(), "ro");
        assert_eq!(ctx.line(), "show ip ro");
    }

    #[test]
    fn word_at_start() {
        let ctx = LineContext::new("sh".to_string(), 2);
        assert_eq!(ctx.word(), "sh");
        assert_eq!(ctx.word_start(), 0);
    }

    #[test]
    fn word_empty_after_space() {
        let ctx = LineContext::new("show ".to_string(), 5);
        assert_eq!(ctx.word(), "");
        assert_eq!(ctx.word_start(), 5);
    }
}
