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
//! match el.readline("prompt> ") {
//!     Ok(line) => println!("got: {line}"),
//!     Err(e) => eprintln!("error: {e}"),
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

// libedit is a POSIX library. This crate would need significant rework
// (Windows console APIs, etc.) to support anything else.
#[cfg(not(unix))]
compile_error!("libedit only supports Unix targets");

pub mod editline;
pub mod error;
pub mod history;
pub mod term;
pub mod tokenizer;

mod shim;
pub(crate) mod wstr;

pub use editline::{Action, EditLine, Editor, EventHandler, LineContext, OutWriter};
pub use error::{Error, Result};
pub use history::{History, DEFAULT_HISTORY_SIZE};
pub use tokenizer::Tokenizer;

/// Re-exported libedit `EL_*` / `H_*` operation constants for use with the
/// escape-hatch methods [`EditLine::set_int`] and [`EditLine::get_int`].
///
/// Prefer the typed helper methods where available; these constants are
/// provided so callers do not need to depend on `libedit-sys` directly.
pub mod consts {
    pub use libedit_sys::{EL_EDITMODE, EL_HIST, EL_PROMPT, EL_RPROMPT, EL_SIGNAL, EL_UNBUFFERED};
}
