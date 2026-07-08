//! Safe wrapper around libedit's `Tokenizer` API.
//!
//! This performs Bourne-shell-style (`sh(1)`) tokenization: it honors single
//! quotes (`'...'`), double quotes (`"..."`), and backslash escapes, so a word
//! containing spaces can be grouped (`deploy "my server"` yields two tokens,
//! the second being `my server`). It is the right tool when parsing a command
//! line whose arguments may be quoted.
//!
//! # When *not* to use this
//!
//! If your input has fixed, simple structure and does not use shell quoting,
//! prefer the standard library: [`str::split_whitespace`] (or
//! [`str::split`]) is simpler, allocation-free (it yields borrowed slices of
//! the input), and pulls in no libedit machinery. Reach for [`Tokenizer`]
//! only when you actually need the quoting/escaping semantics above.

use libedit_sys::*;
use std::ffi::{CStr, CString};
use std::ptr;

use crate::error::{Error, Result};

/// A tokenizer that splits a line into words using `sh(1)`-style quoting.
///
/// This corresponds to libedit's `tok_init` / `tok_str` / `tok_end` API.
/// Not `Send` or `Sync` -- libedit uses global state internally.
pub struct Tokenizer {
    inner: *mut libedit_sys::Tokenizer,
}

impl Tokenizer {
    /// Create a new tokenizer.
    ///
    /// If `separators` is `None`, the default field separators (space, tab,
    /// newline) are used.
    ///
    /// # Separators must be ASCII
    ///
    /// libedit's narrow tokenizer matches separators byte-by-byte. A
    /// non-ASCII separator byte could fall inside a multi-byte UTF-8 sequence
    /// in the input and split it, producing a token that is not valid UTF-8.
    /// To keep every token valid UTF-8 (which [`tokenize`](Self::tokenize)
    /// relies on), `separators` must be ASCII.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `separators` contains an interior NUL byte,
    /// or [`Error::Operation`] if `separators` contains a non-ASCII byte or
    /// libedit fails to initialize the tokenizer.
    pub fn new(separators: Option<&str>) -> Result<Self> {
        // Reject non-ASCII separators: they would let libedit's byte-wise
        // separator matching split a multi-byte character, breaking the
        // "every token is valid UTF-8" invariant that `tokenize` depends on.
        if let Some(s) = separators {
            if !s.is_ascii() {
                return Err(Error::operation(0, -1));
            }
        }
        let sep = match separators {
            Some(s) => Some(CString::new(s)?),
            None => None,
        };
        let inner = unsafe {
            match &sep {
                Some(s) => tok_init(s.as_ptr()),
                None => tok_init(ptr::null()),
            }
        };
        if inner.is_null() {
            return Err(Error::operation(0, -1));
        }
        Ok(Tokenizer { inner })
    }

    /// Tokenize a line, returning the words as slices borrowed from the
    /// tokenizer's internal buffer.
    ///
    /// The returned `&str`s point into storage owned by this `Tokenizer` and
    /// remain valid until the next call that mutates it
    /// ([`tokenize`](Self::tokenize) or [`reset`](Self::reset)); the borrow
    /// checker enforces this because the slices borrow `&mut self`. Call
    /// [`ToOwned::to_owned`] on a token if you need to retain it past then.
    ///
    /// Accepting `impl Into<String>` lets an owned `String` (e.g. the result
    /// of [`readline`](crate::EditLine::readline)) be forwarded without a
    /// fresh copy: it is moved into the `CString` handed to libedit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `input` contains an interior NUL byte, or
    /// [`Error::Operation`] if libedit's tokenizer fails (for example, an
    /// unterminated single or double quote).
    ///
    /// # Panics
    ///
    /// Panics if libedit produces a token that is not valid UTF-8. This
    /// cannot happen for any well-formed use: the input is a `String` (valid
    /// UTF-8), [`new`](Self::new) guarantees ASCII separators, and the
    /// tokenizer only ever removes ASCII quote/backslash bytes or splits on
    /// ASCII separators -- none of which can bisect a multi-byte UTF-8
    /// sequence. A panic here therefore indicates a bug in this crate (or
    /// libedit), not bad user input.
    pub fn tokenize(&mut self, input: impl Into<String>) -> Result<Vec<&str>> {
        // `CString::new` takes `Into<Vec<u8>>`; a `String` moves its buffer in
        // via `into_bytes` and the NUL terminator is appended in place (a
        // realloc only if the buffer had no spare capacity), so an owned
        // caller-supplied `String` avoids a content copy.
        let c_input = CString::new(input.into())?;
        let mut argc: i32 = 0;
        let mut argv: *mut *const std::os::raw::c_char = ptr::null_mut();

        let ret = unsafe { tok_str(self.inner, c_input.as_ptr(), &mut argc, &mut argv) };
        if ret != 0 {
            return Err(Error::operation(0, ret));
        }

        let mut words = Vec::with_capacity(argc as usize);
        for i in 0..argc {
            // SAFETY: `tok_str` returned success, so `argv` points to `argc`
            // valid C strings living in the tokenizer's `wspace` buffer, which
            // outlives the `&mut self` borrow the returned slices are tied to.
            let word = unsafe { CStr::from_ptr(*argv.offset(i as isize)) };
            // INVARIANT (see `# Panics`): input was a `String` and separators
            // are ASCII, so no token can be split mid-character; every token
            // is valid UTF-8. `expect` documents this so a future regression
            // (e.g. a wide-tokenizer path) fails loudly at the source.
            words.push(
                word.to_str().expect(
                    "tokenizer produced non-UTF-8 despite String input and ASCII separators",
                ),
            );
        }

        Ok(words)
    }

    /// Reset the tokenizer for reuse with a new line.
    pub fn reset(&mut self) {
        unsafe { tok_reset(self.inner) };
    }
}

impl Drop for Tokenizer {
    fn drop(&mut self) {
        unsafe { tok_end(self.inner) };
    }
}
