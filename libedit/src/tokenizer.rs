//! Safe wrapper around libedit's `Tokenizer` API.

use libedit_sys::*;
use std::ffi::{CStr, CString};
use std::ptr;

use crate::error::{Error, Result};

/// A tokenizer that splits a line into words.
///
/// This corresponds to libedit's `tok_init` / `tok_str` / `tok_end` API.
/// Not `Send` or `Sync` -- libedit uses global state internally.
pub struct Tokenizer {
    inner: *mut libedit_sys::Tokenizer,
}

impl Tokenizer {
    /// Create a new tokenizer.
    ///
    /// If `separators` is `None`, whitespace is used as the separator.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `separators` contains an interior NUL byte,
    /// or [`Error::Operation`] if libedit fails to initialize the tokenizer.
    pub fn new(separators: Option<&str>) -> Result<Self> {
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

    /// Tokenize a string into words.
    ///
    /// Words that are not valid UTF-8 are converted lossily (invalid
    /// sequences become the U+FFFD replacement character).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Nul`] if `input` contains an interior NUL byte, or
    /// [`Error::Operation`] if libedit's tokenizer fails.
    pub fn tokenize(&mut self, input: impl AsRef<str>) -> Result<Vec<String>> {
        let c_input = CString::new(input.as_ref())?;
        let mut argc: i32 = 0;
        let mut argv: *mut *const std::os::raw::c_char = ptr::null_mut();

        let ret = unsafe { tok_str(self.inner, c_input.as_ptr(), &mut argc, &mut argv) };
        if ret != 0 {
            return Err(Error::operation(0, ret));
        }

        let mut words = Vec::with_capacity(argc as usize);
        for i in 0..argc {
            let word = unsafe { CStr::from_ptr(*argv.offset(i as isize)) };
            words.push(word.to_string_lossy().into_owned());
        }

        Ok(words)
    }

    /// Reset the tokenizer for reuse with a new string.
    pub fn reset(&mut self) {
        unsafe { tok_reset(self.inner) };
    }
}

impl Drop for Tokenizer {
    fn drop(&mut self) {
        unsafe { tok_end(self.inner) };
    }
}
