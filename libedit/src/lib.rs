//! Safe Rust bindings to the [libedit](https://www.thrysoee.dk/editline/)
//! line-editing library.
//!
//! This crate provides ergonomic, safe wrappers around the raw FFI bindings
//! in [`libedit_sys`]. It handles memory management, provides Rust-idiomatic
//! types and error handling, and deals with libedit's C variadic functions
//! properly.
//!
//! # Example
//!
//! ```no_run
//! use libedit::EditLine;
//!
//! let mut el = EditLine::new("example").unwrap();
//! if let Some(line) = el.readline("prompt> ").unwrap() {
//!     println!("got: {line}");
//! }
//! ```
//!
//! # Error handling
//!
//! All fallible operations return [`Result<T>`], which aliases
//! `std::result::Result<T, `[`Error`]`>`. A single [`Error`] type is shared
//! across the editor, history, and tokenizer so that a `?` works uniformly
//! throughout an application.

#![deny(missing_docs)]

pub mod completion;
pub mod editline;
pub mod error;
pub mod hint;
pub mod history;
pub mod suggestion;
pub mod term;
pub mod tokenizer;

mod shim;
mod trace;

pub use completion::{longest_common_prefix, CandidateStyler, Completer, Completion, LineContext};
pub use editline::{Action, ActionContext, EditLine, Editor, Writer};
pub use error::{Error, Result};
pub use hint::{Hint, Hinter};
pub use history::{History, DEFAULT_HISTORY_SIZE};
pub use suggestion::{Suggester, Suggestion};
pub use tokenizer::Tokenizer;

/// Re-exported libedit `EL_*` / `H_*` operation constants for use with the
/// escape-hatch methods [`EditLine::set_int`] and [`EditLine::get_int`].
///
/// Prefer the typed helper methods where available; these constants are
/// provided so callers do not need to depend on `libedit-sys` directly.
pub mod consts {
    pub use libedit_sys::{EL_EDITMODE, EL_HIST, EL_PROMPT, EL_RPROMPT, EL_SIGNAL, EL_UNBUFFERED};
}
