//! Safe wrapper around libedit's `History` API.

use libedit_sys::*;
use std::ffi::CString;
use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::shim;
use crate::wstr::{WCStr, WCString};

/// The default maximum number of entries a [`History`] retains when created
/// with [`History::new`].
pub const DEFAULT_HISTORY_SIZE: usize = 100;

/// A single entry retrieved from a [`History`] buffer.
///
/// Wraps the data from libedit's `HistEventW`: the event number and the
/// entry's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// libedit's internal event ID (a monotonically increasing counter).
    pub num: i32,
    /// The history entry text.
    pub value: String,
}

impl HistoryEvent {
    /// Construct a `HistoryEvent` from a raw `HistEventW`.
    fn from_hist_event(ev: HistEventW) -> Self {
        let wc = unsafe { WCStr::from_ptr(ev.str_) };
        HistoryEvent {
            num: ev.num,
            value: wc.to_string_lossy(),
        }
    }
}

/// A command history buffer.
///
/// Wraps libedit's `HistoryW` object and manages its lifecycle. Attach it to
/// an editor with [`EditLine::set_history`](crate::EditLine::set_history) to
/// enable interactive recall (up/down arrows).
///
/// Not `Send` or `Sync` -- libedit uses global state internally.
pub struct History {
    inner: NonNull<libedit_sys::HistoryW>,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("inner", &self.inner.as_ptr())
            .finish()
    }
}

impl History {
    /// Create a new history buffer with the [`DEFAULT_HISTORY_SIZE`] entry
    /// limit.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `history_winit` call returns null, which
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
    /// Panics if the underlying `history_winit` call returns null, which
    /// indicates a severe resource exhaustion (memory or file descriptors).
    pub fn with_size(size: usize) -> Self {
        let inner = unsafe { history_winit() };
        assert!(!inner.is_null(), "history_winit returned null");
        let inner = unsafe { NonNull::new_unchecked(inner) };
        // H_SETSIZE must be called before H_ENTER to allocate the buffer.
        unsafe { shim::history_w_setsize(inner.as_ptr(), size as i32) };
        History { inner }
    }

    /// Set the maximum number of entries retained.
    ///
    /// Shrinking below the current entry count evicts the oldest entries.
    pub fn set_size(&mut self, size: usize) {
        unsafe { shim::history_w_setsize(self.inner.as_ptr(), size as i32) }
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
        unsafe { shim::history_w_setunique(self.inner.as_ptr(), unique) }
    }

    /// Add an entry to the history.
    ///
    /// Returns `Ok(true)` if the entry was inserted, `Ok(false)` if it was
    /// suppressed by duplicate detection (see [`set_unique`](Self::set_unique)),
    /// or [`Error::Nul`] if `entry` contains an interior NUL byte, or
    /// [`Error::Operation`] on allocation failure.
    pub fn add(&mut self, entry: impl AsRef<str>) -> Result<bool> {
        let wc = WCString::from_str(entry.as_ref())?;
        let rc = unsafe { shim::history_w_enter(self.inner.as_ptr(), wc.as_ptr()) };
        if rc < 0 {
            Err(Error::operation(0, rc))
        } else {
            Ok(rc == 1)
        }
    }

    /// Add an already-wide string to the history, avoiding a `String` ->
    /// `WCString` conversion.
    #[doc(hidden)]
    pub(crate) fn add_wide(&mut self, entry: &WCStr) {
        // Allocation failure (-1) is effectively unrecoverable and equivalent
        // to what libedit itself does internally (it aborts). Silently ignore
        // the return here for parity with the C API's behavior in the
        // history-hot-path. Dedup (0) is also fine to ignore.
        let _ = unsafe { shim::history_w_enter(self.inner.as_ptr(), entry.as_ptr()) };
    }

    /// Return the first element in the history list, which is the **most
    /// recently added** entry (libedit's `H_FIRST`).
    ///
    /// Repositions the internal cursor to the first entry.
    /// Returns `None` if the history is empty.
    pub fn first(&mut self) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_first(self.inner.as_ptr()) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Return the last element in the history list, which is the **oldest
    /// retained** entry (libedit's `H_LAST`).
    ///
    /// Repositions the internal cursor to the last entry.
    /// Returns `None` if the history is empty.
    pub fn last(&mut self) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_last(self.inner.as_ptr()) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Advance the history cursor to the **next** (older) entry and return
    /// it (libedit's `H_NEXT`).
    ///
    /// Moving "next" walks toward older entries. Repeated calls iterate from
    /// newest to oldest. Returns `None` when the end of history is reached
    /// or the list is empty.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_next(self.inner.as_ptr()) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Move the history cursor to the **previous** (newer) entry and return
    /// it (libedit's `H_PREV`).
    ///
    /// Moving "previous" walks toward newer entries. Repeated calls iterate
    /// from oldest to newest. Returns `None` when the start of history is
    /// reached or the list is empty.
    pub fn prev(&mut self) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_prev(self.inner.as_ptr()) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Return the current cursor entry without moving (libedit's `H_CURR`).
    ///
    /// Unlike [`first`](Self::first), [`last`](Self::last),
    /// [`next`](Self::next), and [`prev`](Self::prev), this does **not**
    /// reposition the cursor.
    ///
    /// Returns `None` if the history is empty or the cursor is invalid
    /// (e.g. after clearing the history).
    pub fn curr(&self) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_curr(self.inner.as_ptr()) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Seek to the entry with the given event number, scanning toward
    /// **older** entries (libedit's `H_NEXT_EVENT`).
    ///
    /// Unlike [`next`](Self::next) which advances one step, this seeks to
    /// a specific event ID by walking toward older entries from the
    /// current cursor. Returns that entry, or `None` if not found.
    pub fn next_event(&mut self, num: i32) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_next_event(self.inner.as_ptr(), num) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Seek to the entry with the given event number, scanning toward
    /// **newer** entries (libedit's `H_PREV_EVENT`).
    ///
    /// Unlike [`prev`](Self::prev) which moves one step toward newer
    /// entries, this seeks to a specific event ID. Returns that entry,
    /// or `None` if not found.
    pub fn prev_event(&mut self, num: i32) -> Option<HistoryEvent> {
        let ev = unsafe { shim::history_w_prev_event(self.inner.as_ptr(), num) }?;
        Some(HistoryEvent::from_hist_event(ev))
    }

    /// Advance toward older entries `n` times.
    ///
    /// Returns the entry at the new cursor position, or `None` if the
    /// end of history is reached before completing `n` steps.
    ///
    /// This is a convenience wrapper around repeated [`next`](Self::next)
    /// calls.
    pub fn next_n(&mut self, n: usize) -> Option<HistoryEvent> {
        let mut last = None;
        for _ in 0..n {
            last = self.next();
            last.as_ref()?;
        }
        last
    }

    /// Move toward newer entries `n` times.
    ///
    /// Returns the entry at the new cursor position, or `None` if the
    /// start of history is reached before completing `n` steps.
    ///
    /// This is a convenience wrapper around repeated [`prev`](Self::prev)
    /// calls.
    pub fn prev_n(&mut self, n: usize) -> Option<HistoryEvent> {
        let mut last = None;
        for _ in 0..n {
            last = self.prev();
            last.as_ref()?;
        }
        last
    }

    // -- Semantic aliases --
    //
    // libedit's naming is the reverse of what most APIs expect
    //
    //   H_FIRST  = newest       H_LAST = oldest
    //   H_NEXT   = toward older H_PREV = toward newer
    //
    // These aliases use intuitive names so callers don't have to remember
    // the inversion.

    /// Alias for [`first`](Self::first) — return the **newest** entry and
    /// reposition the cursor there.
    #[inline]
    pub fn newest(&mut self) -> Option<HistoryEvent> {
        self.first()
    }

    /// Alias for [`last`](Self::last) — return the **oldest** retained entry
    /// and reposition the cursor there.
    #[inline]
    pub fn oldest(&mut self) -> Option<HistoryEvent> {
        self.last()
    }

    /// Alias for [`next`](Self::next) — move one step toward **older**
    /// entries.
    #[inline]
    pub fn older(&mut self) -> Option<HistoryEvent> {
        self.next()
    }

    /// Alias for [`prev`](Self::prev) — move one step toward **newer**
    /// entries.
    #[inline]
    pub fn newer(&mut self) -> Option<HistoryEvent> {
        self.prev()
    }

    /// Alias for [`next_n`](Self::next_n) — advance `n` steps toward
    /// **older** entries.
    #[inline]
    pub fn older_n(&mut self, n: usize) -> Option<HistoryEvent> {
        self.next_n(n)
    }

    /// Alias for [`prev_n`](Self::prev_n) — advance `n` steps toward
    /// **newer** entries.
    #[inline]
    pub fn newer_n(&mut self, n: usize) -> Option<HistoryEvent> {
        self.prev_n(n)
    }

    /// Return the number of entries currently stored.
    pub fn len(&self) -> usize {
        unsafe { shim::history_w_len(self.inner.as_ptr()) }
    }

    /// Returns `true` if the history contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all entries from the history.
    pub fn clear(&mut self) {
        unsafe { shim::history_w_clear_all(self.inner.as_ptr()) }
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
        let rc = unsafe { shim::history_w_load(self.inner.as_ptr(), &c_path) };
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
    pub fn save(&mut self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let c_path = path_to_cstring(path.as_ref())?;
        let rc = unsafe { shim::history_w_save(self.inner.as_ptr(), &c_path) };
        if rc < 0 {
            return Err(Error::operation(0, rc));
        }
        Ok(())
    }

    /// Return the raw underlying `HistoryW` pointer.
    pub(crate) fn as_ptr(&self) -> *mut libedit_sys::HistoryW {
        self.inner.as_ptr()
    }
}

// SAFETY: NonNull is !Send + !Sync, matching History's invariant.
// No additional impl needed.

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
        unsafe { history_wend(self.inner.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_is_newest() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();
        // H_FIRST returns the most recently added entry (newest).
        assert_eq!(h.first().unwrap().value, "charlie");
    }

    #[test]
    fn last_is_oldest() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();
        // H_LAST returns the oldest retained entry (first one added).
        assert_eq!(h.last().unwrap().value, "alpha");
    }

    #[test]
    fn next_walks_toward_older_entries() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();

        // first() → newest, then next() walks toward older.
        assert_eq!(h.first().unwrap().value, "charlie");
        assert_eq!(h.next().unwrap().value, "bravo");
        assert_eq!(h.next().unwrap().value, "alpha");
        assert!(h.next().is_none(), "end of history");
    }

    #[test]
    fn prev_walks_toward_newer_entries() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();

        // last() → oldest, then prev() walks toward newer.
        assert_eq!(h.last().unwrap().value, "alpha");
        assert_eq!(h.prev().unwrap().value, "bravo");
        assert_eq!(h.prev().unwrap().value, "charlie");
        assert!(h.prev().is_none(), "start of history");
    }

    #[test]
    fn event_numbers_are_monotonic() {
        let mut h = History::new();
        h.add("one").unwrap();
        h.add("two").unwrap();
        h.add("three").unwrap();

        // Walk oldest→newest: event numbers should increase.
        let mut nums = Vec::new();
        let mut entry = h.last();
        while let Some(e) = entry {
            nums.push(e.num);
            entry = h.prev();
        }
        assert_eq!(nums.len(), 3);
        assert!(
            nums.windows(2).all(|w| w[0] < w[1]),
            "event numbers must be increasing"
        );
    }

    #[test]
    fn empty_history_returns_none() {
        let mut h = History::new();
        assert!(h.first().is_none());
        assert!(h.last().is_none());
        assert!(h.next().is_none());
        assert!(h.prev().is_none());
        assert!(h.curr().is_none());
    }

    // -- next_event / prev_event --

    #[test]
    fn next_event_seeks_by_event_number_toward_older() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();

        // first() → newest (charlie). Record its event num.
        let newest = h.first().unwrap();
        // next_event(newest.num) scans toward older from current cursor
        // and should find it immediately (it's already there).
        assert_eq!(h.next_event(newest.num).unwrap().value, "charlie");

        // Move one step older, then seek back to the event we passed.
        let _ = h.next(); // now at "bravo"
                          // next_event scans toward older — can't find newer entries.
        assert!(h.next_event(newest.num).is_none());
    }

    #[test]
    fn prev_event_seeks_by_event_number_toward_newer() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();
        h.add("charlie").unwrap();

        // last() → oldest (alpha).
        let oldest = h.last().unwrap();
        assert_eq!(h.prev_event(oldest.num).unwrap().value, "alpha");

        // Move one step newer, then seek back to the event we passed.
        let _ = h.prev(); // now at "bravo"
                          // prev_event scans toward newer — can't find older entries.
        assert!(h.prev_event(oldest.num).is_none());
    }

    #[test]
    fn event_seek_missing_number_returns_none() {
        let mut h = History::new();
        h.add("one").unwrap();
        h.add("two").unwrap();
        h.add("three").unwrap();

        let first = h.first().unwrap();
        // An event number that has never existed.
        assert!(h.next_event(first.num + 100).is_none());
        assert!(h.prev_event(first.num + 100).is_none());
    }

    // -- next_n / prev_n --

    #[test]
    fn next_n_moves_n_steps() {
        let mut h = History::new();
        h.add("a").unwrap();
        h.add("b").unwrap();
        h.add("c").unwrap();
        h.add("d").unwrap();
        h.add("e").unwrap(); // 5 entries: e(1) d(2) c(3) b(4) a(5) newest→oldest

        let _ = h.first(); // newest: "e"
                           // Advance 2 steps older: skip e → d → c.
        assert_eq!(h.next_n(2).unwrap().value, "c");
    }

    #[test]
    fn next_n_hits_end_returns_none() {
        let mut h = History::new();
        h.add("a").unwrap();
        h.add("b").unwrap();

        let _ = h.first(); // newest: "b"
                           // 2 steps would go past the end (only "a" remains).
        assert!(h.next_n(2).is_none());
    }

    #[test]
    fn prev_n_moves_n_steps() {
        let mut h = History::new();
        h.add("a").unwrap();
        h.add("b").unwrap();
        h.add("c").unwrap();
        h.add("d").unwrap();
        h.add("e").unwrap();

        let _ = h.last(); // oldest: "a"
                          // Advance 2 steps newer: skip a → b → c.
        assert_eq!(h.prev_n(2).unwrap().value, "c");
    }

    #[test]
    fn prev_n_hits_start_returns_none() {
        let mut h = History::new();
        h.add("a").unwrap();
        h.add("b").unwrap();

        let _ = h.last(); // oldest: "a"
                          // 2 steps would go past the start (only "b" remains).
        assert!(h.prev_n(2).is_none());
    }

    #[test]
    fn next_n_zero_returns_none() {
        let mut h = History::new();
        h.add("x").unwrap();

        let _ = h.first();
        // next_n(0) iterates zero times — no entry retrieved.
        assert!(h.next_n(0).is_none());
        assert!(h.prev_n(0).is_none());
    }

    // -- curr --

    #[test]
    fn curr_does_not_move_cursor() {
        let mut h = History::new();
        h.add("alpha").unwrap();
        h.add("bravo").unwrap();

        let _ = h.first(); // newest: "bravo"
        assert_eq!(h.curr().unwrap().value, "bravo");
        // Cursor unchanged — curr again still sees the same entry.
        assert_eq!(h.curr().unwrap().value, "bravo");
    }

    #[test]
    fn curr_after_clear_returns_none() {
        let mut h = History::new();
        h.add("temp").unwrap();
        let _ = h.first(); // position cursor
        assert!(h.curr().is_some());

        h.clear();
        assert!(h.curr().is_none());
    }
}
