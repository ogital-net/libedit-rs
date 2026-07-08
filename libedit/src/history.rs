//! Safe wrapper around libedit's `History` API.

use libedit_sys::*;
use std::ffi::CString;

use crate::error::{Error, Result};
use crate::shim;

/// The default maximum number of entries a [`History`] retains when created
/// with [`History::new`].
pub const DEFAULT_HISTORY_SIZE: usize = 100;

/// A command history buffer.
///
/// Wraps libedit's `History` object and manages its lifecycle. Attach it to
/// an editor with [`EditLine::set_history`](crate::EditLine::set_history) to
/// enable interactive recall (up/down arrows).
///
/// Not `Send` or `Sync` -- libedit uses global state internally.
pub struct History {
    inner: *mut libedit_sys::History,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("inner", &self.inner)
            .finish()
    }
}

impl History {
    /// Create a new history buffer with the [`DEFAULT_HISTORY_SIZE`] entry
    /// limit.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `history_init` call returns null, which
    /// indicates a severe resource exhaustion (memory or file descriptors).
    pub fn new() -> Self {
        Self::with_size(DEFAULT_HISTORY_SIZE)
    }

    /// Create a new history buffer that retains up to `size` entries.
    ///
    /// Once full, adding a new entry evicts the oldest one.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `history_init` call returns null, which
    /// indicates a severe resource exhaustion (memory or file descriptors).
    pub fn with_size(size: usize) -> Self {
        let inner = unsafe { history_init() };
        assert!(!inner.is_null(), "history_init returned null");
        // H_SETSIZE must be called before H_ENTER to allocate the buffer.
        unsafe { shim::history_setsize(inner, size as i32) };
        History { inner }
    }

    /// Set the maximum number of entries retained.
    ///
    /// Shrinking below the current entry count evicts the oldest entries.
    pub fn set_size(&mut self, size: usize) {
        unsafe { shim::history_setsize(self.inner, size as i32) }
    }

    /// Enable or disable duplicate suppression.
    ///
    /// When enabled, calling [`add`](Self::add) with a line identical to the
    /// most recent entry is a no-op, so consecutive duplicates are not stored
    /// (equivalent to readline's `history_ignore_dups`). Non-adjacent
    /// duplicates are still kept. Disabled by default.
    ///
    /// Set this before adding entries; it does not retroactively remove
    /// duplicates already in the buffer.
    pub fn set_unique(&mut self, unique: bool) {
        unsafe { shim::history_setunique(self.inner, unique) }
    }

    /// Add an entry to the history.
    ///
    /// Returns [`Error::Nul`] if `entry` contains an interior NUL byte, or
    /// [`Error::Operation`] if libedit rejects the entry.
    pub fn add(&mut self, entry: impl AsRef<str>) -> Result<()> {
        let c_entry = CString::new(entry.as_ref())?;
        let ok = unsafe { shim::history_enter(self.inner, &c_entry) };
        if ok {
            Ok(())
        } else {
            Err(Error::operation(0, -1))
        }
    }

    /// Return the most recently added entry.
    ///
    /// Returns [`Error::NotFound`] if the history is empty.
    pub fn first(&self) -> Result<String> {
        unsafe { shim::history_first(self.inner) }.ok_or(Error::NotFound)
    }

    /// Return the number of entries currently stored.
    pub fn len(&self) -> usize {
        unsafe { shim::history_len(self.inner) }
    }

    /// Returns `true` if the history contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all entries from the history.
    pub fn clear(&mut self) {
        unsafe { shim::history_clear_all(self.inner) }
    }

    /// Load history entries from a file, appending them to this buffer.
    ///
    /// The file format is libedit's own (one entry per line, as written by
    /// [`save`](Self::save)). Returns [`Error::Operation`] if the file cannot
    /// be read, or [`Error::Nul`] if `path` contains an interior NUL byte.
    ///
    /// Loading a non-existent file is reported as an error; callers that want
    /// "load if present" semantics should ignore the error or check for the
    /// file first.
    pub fn load(&mut self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let c_path = path_to_cstring(path.as_ref())?;
        let rc = unsafe { shim::history_load(self.inner, &c_path) };
        if rc < 0 {
            return Err(Error::operation(0, rc));
        }
        Ok(())
    }

    /// Write all history entries to a file, replacing its contents.
    ///
    /// The parent directory must already exist. Returns [`Error::Operation`]
    /// on write failure, or [`Error::Nul`] if `path` contains an interior NUL
    /// byte.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let c_path = path_to_cstring(path.as_ref())?;
        let rc = unsafe { shim::history_save(self.inner, &c_path) };
        if rc < 0 {
            return Err(Error::operation(0, rc));
        }
        Ok(())
    }

    /// Return the raw underlying `History` pointer for internal wiring.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut libedit_sys::History {
        self.inner
    }

    /// Return a raw pointer to the underlying `History`.
    ///
    /// # Safety
    /// The caller must not call `history_end` or otherwise free this pointer,
    /// and must not use it after this `History` is dropped.
    pub unsafe fn as_ptr(&self) -> *mut libedit_sys::History {
        self.inner
    }
}

/// Convert a filesystem path to a `CString` for libedit's C API.
///
/// On Unix this preserves the raw path bytes. Fails with [`Error::Nul`] if
/// the path contains an interior NUL byte.
pub(crate) fn path_to_cstring(path: &std::path::Path) -> Result<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(CString::new(path.as_os_str().as_bytes())?)
    }
    #[cfg(not(unix))]
    {
        let s = path.to_str().ok_or(Error::NotFound)?;
        Ok(CString::new(s)?)
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for History {
    fn drop(&mut self) {
        unsafe { history_end(self.inner) };
    }
}
