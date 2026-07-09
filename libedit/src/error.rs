//! Unified error handling for the `libedit` crate.

use std::ffi::{CString, NulError};
use std::fmt;
use std::io;

use crate::wstr::WNulError;

/// The error type for all `libedit` operations.
///
/// A single error type is shared across the editor, history, and tokenizer
/// APIs so that callers can use one `?` and one `match` throughout a REPL.
///
/// This enum is marked `#[non_exhaustive]`: new variants may be added in
/// future releases without a breaking change, so downstream `match`
/// statements should include a wildcard arm.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O error occurred.
    Io(io::Error),

    /// A Rust string passed to libedit contained an interior NUL byte and
    /// could not be converted to a C string.
    Nul(NulError),

    /// libedit returned an unexpected null pointer (for example, from
    /// `el_init` or `history_init`).
    Null,

    /// A libedit operation reported failure.
    ///
    /// `op` is the libedit operation code involved (or `0` when not
    /// applicable), and `code` is the raw return value libedit produced.
    Operation {
        /// The libedit operation code (e.g. `EL_EDITMODE`), or `0`.
        op: i32,
        /// The raw return code libedit produced.
        code: i32,
    },

    /// The requested history entry does not exist (e.g. the history is
    /// empty).
    NotFound,

    /// The read was interrupted by a signal (e.g. the user pressed Ctrl-C).
    ///
    /// Only produced when signal handling is enabled via
    /// [`EditLine::set_signal_handling`](crate::EditLine::set_signal_handling).
    /// A typical REPL treats this as "abandon the current line and show a
    /// fresh prompt".
    Interrupted,

    /// End-of-file was received (e.g. the user pressed Ctrl-D on an empty
    /// line).
    Eof,
}

impl Error {
    /// Construct an [`Error::Operation`] from an operation code and return
    /// value.
    pub(crate) fn operation(op: i32, code: i32) -> Self {
        Error::Operation { op, code }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Nul(e) => write!(f, "interior NUL byte in string: {e}"),
            Error::Null => write!(f, "libedit returned an unexpected null pointer"),
            Error::Operation { op, code } => {
                write!(f, "libedit operation {op} failed with code {code}")
            }
            Error::NotFound => write!(f, "history entry not found"),
            Error::Interrupted => write!(f, "read interrupted by signal"),
            Error::Eof => write!(f, "end of file"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Nul(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<NulError> for Error {
    fn from(e: NulError) -> Self {
        Error::Nul(e)
    }
}

impl From<WNulError> for Error {
    fn from(e: WNulError) -> Self {
        // Convert a WNulError (wide-string interior NUL) to Error::Nul.
        // NulError has no public constructor, so we feed a Vec<u8> with an
        // interior NUL to CString::new and unwrap the error.
        let pos = e.nul_position();
        let mut bytes = vec![b'x'; pos + 2];
        bytes[pos] = 0; // interior NUL at position `pos`
        Error::Nul(CString::new(bytes).unwrap_err())
    }
}

/// A specialized [`Result`] type for `libedit` operations.
pub type Result<T> = std::result::Result<T, Error>;
