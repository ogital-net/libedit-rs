//! Wide-character string types for interfacing with libedit's wide (`wchar_t`)
//! API.
//!
//! libedit's wide API operates on NUL-terminated `wchar_t` arrays. On the
//! platforms this crate targets, `wchar_t` is a 32-bit value
//! ([`libedit_sys::wchar_t`] maps to [`libc::wchar_t`]), and libedit treats
//! each element as a Unicode scalar value. These types mirror the standard
//! library's
//! [`CStr`](std::ffi::CStr) / [`CString`](std::ffi::CString) pairing, but are
//! backed by `u32` code units instead of bytes:
//!
//! * [`WCString`] is an owned, NUL-terminated wide string (like `CString`).
//! * [`WCStr`] is a borrowed view of a NUL-terminated wide string (like
//!   `CStr`).
//!
//! Both are designed to make it easy to convert from a [`String`], a
//! [`&str`](str), or a slice of [`char`]s, and to hand a `*const wchar_t` to C
//! FFI that expects a wide string.
//!
//! # Encoding
//!
//! Each element holds one Unicode scalar value (a Rust [`char`]) as its `u32`
//! representation. Unlike UTF-16, there are no surrogate pairs; unlike UTF-8,
//! there is no multi-byte encoding. A single `char` always maps to a single
//! `wchar_t`. This matches libedit's wide API, which stores one scalar per
//! `wchar_t`.

// This module is introduced ahead of the call sites that will consume it as
// part of the migration to libedit's wide API; suppress dead-code warnings
// until those call sites land.
#![allow(dead_code)]

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

/// The wide code unit used by these types.
///
/// This is an alias for [`libc::wchar_t`]
pub(crate) type WChar = libc::wchar_t;

/// An error returned when constructing a [`WCString`] from input that contains
/// an interior NUL scalar (`U+0000`).
///
/// A `WCString` is NUL-terminated, so an interior NUL would be ambiguous and
/// is rejected, mirroring [`std::ffi::NulError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WNulError {
    /// The index (in scalars) at which the interior NUL was found.
    index: usize,
}

impl WNulError {
    /// The position of the interior NUL scalar, counted in scalars from the
    /// start of the input.
    pub(crate) fn nul_position(&self) -> usize {
        self.index
    }
}

impl fmt::Display for WNulError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "interior NUL scalar found in wide string at position {}",
            self.index
        )
    }
}

impl std::error::Error for WNulError {}

/// An owned, NUL-terminated wide-character string backed by `u32` code units.
///
/// This is the wide-string analogue of [`std::ffi::CString`]. The internal
/// buffer always ends with a trailing NUL (`0`) unit and never contains an
/// interior NUL, so a pointer to its data can be passed directly to libedit's
/// wide (`wchar_t`) FFI functions.
///
/// # Examples
///
/// ```ignore
/// let ws = WCString::from_str("héllo")?;
/// let ptr = ws.as_ptr(); // *const wchar_t, NUL-terminated
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct WCString {
    /// The wide units.
    ///
    /// A `WCString` produced by the checked constructors ([`from_str`],
    /// [`from_chars`]) upholds the strong invariant that `inner` ends in a
    /// single NUL terminator and contains no interior NUL. The builder API
    /// ([`new`], [`with_capacity`], [`push`], [`extend`]) relaxes this: the
    /// buffer may be un-terminated (or even hold interior NULs) *while it is
    /// being assembled*. Callers must call [`terminate`](Self::terminate) (or
    /// [`push_nul`](Self::push_nul)) before handing the buffer to libedit or
    /// borrowing it as a [`WCStr`]; the reading/FFI accessors assume a
    /// trailing NUL is present.
    ///
    /// [`from_str`]: Self::from_str
    /// [`from_chars`]: Self::from_chars
    /// [`new`]: Self::new
    /// [`with_capacity`]: Self::with_capacity
    /// [`push`]: Self::push
    /// [`extend`]: Self::extend
    inner: Vec<WChar>,
}

impl WCString {
    /// Create an empty, **un-terminated** wide string builder.
    ///
    /// The buffer starts with no NUL terminator; push scalars with
    /// [`push`](Self::push) / [`push_scalar`](Self::push_scalar) or the
    /// [`Extend`] impls, then call [`terminate`](Self::terminate) before use.
    pub(crate) fn new() -> Self {
        WCString { inner: Vec::new() }
    }

    /// Create an empty, **un-terminated** wide string builder with room for at
    /// least `capacity` wide units before reallocating.
    ///
    /// Reserve one extra unit for the NUL terminator you will add later (see
    /// [`prompt_to_wide`](crate::editline) for a representative build pattern).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        WCString {
            inner: Vec::with_capacity(capacity),
        }
    }

    /// Build a [`WCString`] from anything that yields Unicode scalars,
    /// returning an error if any scalar is NUL (`U+0000`).
    fn from_scalars<I>(scalars: I) -> Result<Self, WNulError>
    where
        I: IntoIterator<Item = WChar>,
    {
        let mut inner: Vec<WChar> = Vec::new();
        for (index, scalar) in scalars.into_iter().enumerate() {
            if scalar == 0 {
                return Err(WNulError { index });
            }
            inner.push(scalar);
        }
        inner.push(0);
        Ok(WCString { inner })
    }

    /// Append a single [`char`] as one wide unit.
    ///
    /// No NUL check is performed: `'\0'` is pushed verbatim. This is part of
    /// the relaxed builder API; ensure the buffer is [`terminate`]d before use.
    ///
    /// [`terminate`]: Self::terminate
    pub(crate) fn push(&mut self, c: char) {
        self.inner.push(char_to_c(c));
    }

    /// Append a raw wide unit.
    ///
    /// No NUL or scalar-validity check is performed; the value is stored
    /// verbatim. Useful for splicing in control units (e.g. libedit's prompt
    /// escape delimiter). Ensure the buffer is [`terminate`]d before use.
    ///
    /// [`terminate`]: Self::terminate
    pub(crate) fn push_scalar(&mut self, unit: WChar) {
        self.inner.push(unit);
    }

    /// Append every [`char`] of a string slice as one wide unit each.
    ///
    /// Part of the relaxed builder API; no NUL check is performed.
    pub(crate) fn push_str(&mut self, s: &str) {
        self.inner.extend(s.chars().map(char_to_c));
    }

    /// Append a single NUL terminator unconditionally.
    ///
    /// Prefer [`terminate`](Self::terminate) unless you specifically intend to
    /// append a NUL even when one is already present.
    pub(crate) fn push_nul(&mut self) {
        self.inner.push(0);
    }

    /// Ensure the buffer ends in exactly one NUL terminator.
    ///
    /// A no-op if the last unit is already NUL; otherwise appends one. Call
    /// this after building with [`push`](Self::push) / [`extend`](Self::extend)
    /// and before [`as_ptr`](Self::as_ptr), [`as_wcstr`](Self::as_wcstr), or
    /// any reading accessor.
    pub(crate) fn terminate(&mut self) {
        if self.inner.last() != Some(&0) {
            self.inner.push(0);
        }
    }

    /// Convert a string slice into a [`WCString`].
    ///
    /// Each [`char`] of the input becomes one wide unit. Fails with
    /// [`WNulError`] if the string contains an interior NUL character.
    pub(crate) fn from_str(s: &str) -> Result<Self, WNulError> {
        Self::from_scalars(s.chars().map(char_to_c))
    }

    /// Convert a slice of [`char`]s into a [`WCString`].
    ///
    /// Fails with [`WNulError`] if the slice contains a NUL character.
    pub(crate) fn from_chars(chars: &[char]) -> Result<Self, WNulError> {
        Self::from_scalars(chars.iter().map(|&c| char_to_c(c)))
    }

    /// Borrow this owned string as a [`WCStr`].
    ///
    /// # Panics
    ///
    /// Panics if the buffer is not NUL-terminated. Strings from the checked
    /// constructors always are; if you built one with the relaxed builder API
    /// ([`new`](Self::new) / [`push`](Self::push) / [`extend`](Self::extend)),
    /// call [`terminate`](Self::terminate) first.
    pub(crate) fn as_wcstr(&self) -> &WCStr {
        assert_eq!(
            self.inner.last(),
            Some(&0),
            "WCString must be NUL-terminated before borrowing as WCStr; call terminate()"
        );
        // SAFETY: just verified a trailing NUL is present. `WCStr` tolerates
        // interior NULs for its slice accessors; only `from_ptr` scans.
        unsafe { WCStr::from_units_with_nul_unchecked(&self.inner) }
    }

    /// Return a pointer to the NUL-terminated wide buffer, suitable for
    /// passing to libedit's `wchar_t*` FFI functions.
    ///
    /// The pointer is valid as long as this `WCString` is alive and not
    /// mutated.
    pub(crate) fn as_ptr(&self) -> *const WChar {
        self.inner.as_ptr()
    }

    /// Consume the `WCString` and return the backing buffer, including the
    /// trailing NUL terminator.
    pub(crate) fn into_units_with_nul(self) -> Vec<WChar> {
        self.inner
    }

    /// Clears the internal vector, removing all values.
    pub(crate) fn clear(&mut self) {
        self.inner.clear();
    }

    /// Returns `true` if the buffer contains no characters.
    ///
    /// Unlike [`WCStr::is_empty`], this does not require the buffer to be
    /// NUL-terminated -- it is safe to call after [`clear`] (empty `Vec`).
    /// When the buffer IS NUL-terminated, the result matches `WCStr::is_empty`.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty() || self.inner == [0]
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }

    pub(crate) fn reserve_exact(&mut self, additional: usize) {
        self.inner.reserve_exact(additional);
    }

    /// Build a [`WCString`] by copying a NUL-terminated wide buffer from
    /// libedit. This is a single `memcpy`-like allocation -- no per-scalar
    /// conversion.
    pub(crate) fn from_wide_buf(ptr: *const WChar) -> Self {
        let wcstr = unsafe { WCStr::from_ptr(ptr) };
        Self {
            inner: wcstr.units_with_nul().to_vec(),
        }
    }

    /// Trim trailing whitespace (spaces and `\n`) by truncating the vec.
    pub(crate) fn trim_end(&mut self) {
        while let Some(&c) = self.inner.last() {
            if c == ' ' as WChar || c == '\n' as WChar || c == 0 {
                self.inner.pop();
            } else {
                break;
            }
        }
        self.inner.push(0);
    }

    /// Trim leading whitespace (spaces and `\n`) by shifting content forward
    /// and truncating.
    pub(crate) fn trim_start(&mut self) {
        let start = self
            .inner
            .iter()
            .position(|&c| c != ' ' as WChar && c != '\n' as WChar)
            .unwrap_or(self.inner.len());
        if start > 0 {
            self.inner.drain(..start);
        }
    }

    /// Trim leading and trailing whitespace (spaces and `\n`) in place.
    pub(crate) fn trim(&mut self) {
        self.trim_end();
        self.trim_start();
    }
}

impl fmt::Debug for WCString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_wcstr(), f)
    }
}

impl Deref for WCString {
    type Target = WCStr;

    fn deref(&self) -> &WCStr {
        self.as_wcstr()
    }
}

impl Borrow<WCStr> for WCString {
    fn borrow(&self) -> &WCStr {
        self.as_wcstr()
    }
}

impl AsRef<WCStr> for WCString {
    fn as_ref(&self) -> &WCStr {
        self.as_wcstr()
    }
}

impl TryFrom<&str> for WCString {
    type Error = WNulError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::from_str(s)
    }
}

impl TryFrom<String> for WCString {
    type Error = WNulError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::from_str(&s)
    }
}

impl TryFrom<&[char]> for WCString {
    type Error = WNulError;

    fn try_from(chars: &[char]) -> Result<Self, Self::Error> {
        Self::from_chars(chars)
    }
}

impl Extend<char> for WCString {
    /// Append each [`char`] as one wide unit. Part of the relaxed builder API:
    /// no NUL check, and the result is left un-terminated.
    fn extend<I: IntoIterator<Item = char>>(&mut self, iter: I) {
        self.inner.extend(iter.into_iter().map(char_to_c));
    }
}

impl<'a> Extend<&'a char> for WCString {
    fn extend<I: IntoIterator<Item = &'a char>>(&mut self, iter: I) {
        self.inner.extend(iter.into_iter().map(|&c| char_to_c(c)));
    }
}

impl Extend<WChar> for WCString {
    /// Append raw wide units verbatim (no scalar-validity or NUL check). Part
    /// of the relaxed builder API; the result is left un-terminated.
    fn extend<I: IntoIterator<Item = WChar>>(&mut self, iter: I) {
        self.inner.extend(iter);
    }
}

/// A borrowed, NUL-terminated wide-character string backed by `u32` code
/// units.
///
/// This is the wide-string analogue of [`std::ffi::CStr`]. It is an unsized
/// type, always accessed behind a reference (`&WCStr`), and wraps a slice of
/// wide units whose final element is a NUL terminator.
///
/// A `&WCStr` is most commonly obtained by dereferencing a [`WCString`], or by
/// borrowing a NUL-terminated wide buffer received from libedit via
/// [`WCStr::from_ptr`].
#[repr(transparent)]
pub(crate) struct WCStr {
    /// The wide units, *including* the trailing NUL terminator.
    inner: [WChar],
}

impl WCStr {
    /// Wrap a slice whose final element is a NUL terminator.
    ///
    /// # Safety
    ///
    /// `units` must be non-empty and its last element must be `0`. Interior
    /// NULs are permitted for the slice/reading accessors, but note that
    /// [`from_ptr`](Self::from_ptr) and any C consumer will stop at the first
    /// NUL.
    unsafe fn from_units_with_nul_unchecked(units: &[WChar]) -> &WCStr {
        // SAFETY: `WCStr` is `repr(transparent)` over `[WChar]`, so the
        // reference cast is layout-compatible; the caller upholds the
        // NUL-termination invariant.
        &*(units as *const [WChar] as *const WCStr)
    }

    /// Borrow a NUL-terminated wide string from a raw `wchar_t` pointer.
    ///
    /// This scans forward from `ptr` until it finds a NUL unit, then returns a
    /// [`WCStr`] covering the units up to and including that terminator. This
    /// is the wide analogue of [`CStr::from_ptr`](std::ffi::CStr::from_ptr).
    ///
    /// # Safety
    ///
    /// * `ptr` must point to a valid, NUL-terminated array of `wchar_t`.
    /// * The memory must remain valid and unmutated for the lifetime `'a`.
    /// * The referenced buffer must not be larger than `isize::MAX` units.
    pub(crate) unsafe fn from_ptr<'a>(ptr: *const WChar) -> &'a WCStr {
        // SAFETY: caller guarantees a NUL terminator exists within the buffer.
        let len = unsafe { libc::wcslen(ptr) };
        // Include the terminator in the slice.
        let units = unsafe { std::slice::from_raw_parts(ptr, len + 1) };
        unsafe { WCStr::from_units_with_nul_unchecked(units) }
    }

    /// Mutable variant of [`from_ptr`](Self::from_ptr). The caller must have
    /// exclusive mutable access to the buffer.
    pub(crate) unsafe fn from_ptr_mut<'a>(ptr: *mut WChar) -> &'a mut WCStr {
        let len = unsafe { libc::wcslen(ptr) };
        let units = unsafe { std::slice::from_raw_parts_mut(ptr, len + 1) };
        // SAFETY: `WCStr` is `repr(transparent)` over `[WChar]`, so the
        // layout is identical.
        unsafe { &mut *(units as *mut [WChar] as *mut WCStr) }
    }

    /// Trim trailing whitespace from the end of the string, replacing any
    /// trailing spaces or `\n` with NUL. Mirrors [`str::trim_end`].
    pub(crate) fn trim_end(&mut self) {
        let len = self.inner.len();
        // Walk backwards from just before the NUL terminator.
        let mut i = len.wrapping_sub(2);
        while i < len {
            match self.inner[i] {
                c if c == ' ' as WChar || c == '\n' as WChar => {
                    self.inner[i] = 0;
                    i = i.wrapping_sub(1);
                }
                _ => break,
            }
        }
    }

    /// Return a sub-slice with leading whitespace (spaces and `\n`)
    /// removed. Because the backing buffer shares the trailing NUL, the
    /// returned `&WCStr` is valid without any mutation.
    pub(crate) fn trim_start(&self) -> &WCStr {
        let n = self.inner.len().wrapping_sub(1); // index of NUL terminator
        let mut start = 0;
        while start < n {
            let c = self.inner[start];
            if c != ' ' as WChar && c != '\n' as WChar {
                break;
            }
            start += 1;
        }
        // SAFETY: self.inner[start..] still ends with the NUL terminator,
        // satisfying the WCStr invariant.
        unsafe { WCStr::from_units_with_nul_unchecked(&self.inner[start..]) }
    }

    /// Trim trailing whitespace in place, then return a sub-slice with
    /// leading whitespace removed. Mirrors [`str::trim`].
    pub(crate) fn trim(&mut self) -> &WCStr {
        self.trim_end();
        self.trim_start()
    }

    /// Return a pointer to the NUL-terminated wide buffer, suitable for
    /// passing to libedit's `wchar_t*` FFI functions.
    pub(crate) fn as_ptr(&self) -> *const WChar {
        self.inner.as_ptr()
    }

    /// The wide units of this string, **not** including the trailing NUL
    /// terminator.
    pub(crate) fn units(&self) -> &[WChar] {
        &self.inner[..self.inner.len() - 1]
    }

    /// The wide units of this string, **including** the trailing NUL
    /// terminator.
    pub(crate) fn units_with_nul(&self) -> &[WChar] {
        &self.inner
    }

    /// The number of scalars in the string, not counting the NUL terminator.
    ///
    /// Checks the first unit to short-circuit empty strings, otherwise uses
    /// `wcslen` to scan for the first NUL. Correct after
    /// [`trim_end`](Self::trim_end) has written NULs into the buffer.
    pub(crate) fn len(&self) -> usize {
        if self.inner[0] == 0 {
            return 0;
        }
        unsafe { libc::wcslen(self.inner.as_ptr()) }
    }

    /// Returns `true` if the string is logically empty.
    ///
    /// Correct after [`trim_end`](Self::trim_end) has written NULs into the
    /// buffer without shrinking the slice.
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate over the scalars of this string as [`char`]s.
    ///
    /// Any unit that is not a valid Unicode scalar value is replaced with the
    /// Unicode replacement character (`U+FFFD`).
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.units().iter().map(|&u| char_from_c(u))
    }

    /// Decode the string into an owned [`String`].
    ///
    /// Any unit that is not a valid Unicode scalar value is replaced with the
    /// Unicode replacement character (`U+FFFD`), so this is lossy but never
    /// fails. This mirrors [`String::from_utf8_lossy`].
    ///
    /// Uses `wcslen` (via [`len`](Self::len)) to determine the logical length,
    /// so it correctly handles buffers that have been trimmed in-place via
    /// [`trim_end`](Self::trim_end).
    pub(crate) fn to_string_lossy(&self) -> String {
        let n = self.len();
        self.inner[..n].iter().map(|&u| char_from_c(u)).collect()
    }
}

impl fmt::Debug for WCStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render like a normal string literal, escaping as needed.
        write!(f, "\"")?;
        for c in self.chars() {
            for esc in c.escape_debug() {
                write!(f, "{esc}")?;
            }
        }
        write!(f, "\"")
    }
}

impl fmt::Display for WCStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.chars() {
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl PartialEq for WCStr {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for WCStr {}

impl ToOwned for WCStr {
    type Owned = WCString;

    fn to_owned(&self) -> WCString {
        WCString {
            inner: self.inner.to_vec(),
        }
    }
}

#[inline]
pub(crate) fn char_from_c(c: WChar) -> char {
    #[allow(clippy::unnecessary_cast)] // possibly signed wchar_t
    char::from_u32(c as u32).unwrap_or('\u{FFFD}')
}

#[inline]
pub(crate) fn char_to_c(c: char) -> WChar {
    c as WChar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_roundtrips() {
        let ws = WCString::from_str("hello").unwrap();
        assert_eq!(ws.len(), 5);
        assert_eq!(ws.to_string_lossy(), "hello");
    }

    #[test]
    fn non_ascii_is_one_unit_per_char() {
        let ws = WCString::from_str("héllo😀").unwrap();
        // h é l l o 😀 == 6 scalars, one wide unit each.
        assert_eq!(ws.len(), 6);
        assert_eq!(ws.to_string_lossy(), "héllo😀");
        // The emoji is a single u32 unit, not a surrogate pair.
        assert_eq!(*ws.units().last().unwrap(), '😀' as WChar);
    }

    #[test]
    fn empty_string() {
        let ws = WCString::from_str("").unwrap();
        assert!(ws.is_empty());
        assert_eq!(ws.len(), 0);
        // Just the terminator.
        assert_eq!(ws.units_with_nul(), &[0]);
    }

    #[test]
    fn always_nul_terminated() {
        let ws = WCString::from_str("abc").unwrap();
        assert_eq!(*ws.units_with_nul().last().unwrap(), 0);
    }

    #[test]
    fn interior_nul_is_rejected() {
        let err = WCString::from_str("ab\0cd").unwrap_err();
        assert_eq!(err.nul_position(), 2);
    }

    #[test]
    fn from_chars_works() {
        let chars = ['w', 'i', 'd', 'e'];
        let ws = WCString::from_chars(&chars).unwrap();
        assert_eq!(ws.to_string_lossy(), "wide");
    }

    #[test]
    fn try_from_impls() {
        let a = WCString::try_from("x").unwrap();
        let b = WCString::try_from(String::from("x")).unwrap();
        let chars: &[char] = &['x'];
        let c = WCString::try_from(chars).unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn from_ptr_borrows_until_nul() {
        let ws = WCString::from_str("ptr").unwrap();
        // SAFETY: `ws` is alive and NUL-terminated for the whole borrow.
        let borrowed = unsafe { WCStr::from_ptr(ws.as_ptr()) };
        assert_eq!(borrowed.len(), 3);
        assert_eq!(borrowed.to_string_lossy(), "ptr");
        assert_eq!(borrowed, ws.as_wcstr());
    }

    #[test]
    fn deref_to_wcstr() {
        let ws = WCString::from_str("deref").unwrap();
        // Exercise Deref: call a WCStr method through the WCString.
        assert_eq!(ws.len(), 5);
        assert_eq!(ws.to_string_lossy(), "deref");
    }

    #[test]
    fn to_owned_roundtrips() {
        let ws = WCString::from_str("own").unwrap();
        let borrowed: &WCStr = ws.as_wcstr();
        let owned = borrowed.to_owned();
        assert_eq!(owned, ws);
    }

    #[test]
    fn debug_escapes() {
        let ws = WCString::from_str("a\tb").unwrap();
        assert_eq!(format!("{ws:?}"), r#""a\tb""#);
    }

    #[test]
    fn builder_new_push_and_terminate() {
        let mut ws = WCString::new();
        ws.push('h');
        ws.push('i');
        ws.push_str("!!");
        ws.terminate();
        assert_eq!(ws.to_string_lossy(), "hi!!");
        assert_eq!(*ws.units_with_nul().last().unwrap(), 0);
    }

    #[test]
    fn builder_with_capacity_reserves() {
        let ws = WCString::with_capacity(16);
        // Empty and un-terminated until we add content.
        assert!(ws.inner.is_empty());
    }

    #[test]
    fn terminate_is_idempotent() {
        let mut ws = WCString::new();
        ws.push('x');
        ws.terminate();
        let len_after_first = ws.units_with_nul().len();
        ws.terminate();
        assert_eq!(ws.units_with_nul().len(), len_after_first);
    }

    #[test]
    fn extend_with_chars_and_units() {
        let mut ws = WCString::new();
        ws.extend("ab".chars());
        ws.extend([0x63u32 as WChar, 0x64 as WChar]); // c, d
        ws.terminate();
        assert_eq!(ws.to_string_lossy(), "abcd");
    }

    #[test]
    fn push_scalar_allows_control_units() {
        // Mirror prompt_to_wide's delimiter-splicing pattern.
        let mut ws = WCString::with_capacity(4);
        ws.push_scalar(0x01); // SOH delimiter
        ws.push('>');
        ws.push_scalar(0x01);
        ws.terminate();
        assert_eq!(ws.len(), 3);
        assert_eq!(ws.units()[0], 0x01);
        assert_eq!(ws.units()[2], 0x01);
    }

    #[test]
    #[should_panic(expected = "NUL-terminated")]
    fn as_wcstr_panics_when_unterminated() {
        let mut ws = WCString::new();
        ws.push('a');
        // No terminate() call -> borrowing must panic rather than read OOB.
        let _ = ws.as_wcstr();
    }

    // ---- WCString trim tests ----

    #[test]
    fn wcstring_trim_end_removes_trailing_whitespace() {
        let mut ws = WCString::from_str("hello  \n").unwrap();
        ws.trim_end();
        assert_eq!(ws.to_string_lossy(), "hello");
        assert_eq!(*ws.inner.last().unwrap(), 0);
    }

    #[test]
    fn wcstring_trim_end_all_whitespace() {
        let mut ws = WCString::from_str("  \n\n").unwrap();
        ws.trim_end();
        assert_eq!(ws.to_string_lossy(), "");
        assert_eq!(ws.inner, vec![0]);
    }

    #[test]
    fn wcstring_trim_end_no_trailing_whitespace() {
        let mut ws = WCString::from_str("abc").unwrap();
        ws.trim_end();
        assert_eq!(ws.to_string_lossy(), "abc");
    }

    #[test]
    fn wcstring_trim_start_removes_leading_whitespace() {
        let mut ws = WCString::from_str("  \nhello").unwrap();
        ws.trim_start();
        assert_eq!(ws.to_string_lossy(), "hello");
    }

    #[test]
    fn wcstring_trim_start_all_whitespace() {
        let mut ws = WCString::from_str("   ").unwrap();
        ws.trim_start();
        // Everything drained; only the NUL remains.
        assert_eq!(ws.inner, vec![0]);
    }

    #[test]
    fn wcstring_trim_start_no_leading_whitespace() {
        let mut ws = WCString::from_str("abc").unwrap();
        ws.trim_start();
        assert_eq!(ws.to_string_lossy(), "abc");
    }

    #[test]
    fn wcstring_trim_both_sides() {
        let mut ws = WCString::from_str(" \n hi \n ").unwrap();
        ws.trim();
        assert_eq!(ws.to_string_lossy(), "hi");
    }

    // ---- WCStr trim tests (borrowed/DST) ----

    #[test]
    fn wcstr_trim_end_writes_nuls() {
        let mut ws = WCString::from_str("foo \n").unwrap();
        {
            let wcstr: &mut WCStr =
                unsafe { &mut *(ws.inner.as_mut_slice() as *mut [WChar] as *mut WCStr) };
            wcstr.trim_end();
        }
        // The DST slice still has original length, but to_string_lossy uses
        // wcslen and stops at the first NUL.
        let wcstr = ws.as_wcstr();
        assert_eq!(wcstr.to_string_lossy(), "foo");
        assert_eq!(wcstr.len(), 3);
        assert!(!wcstr.is_empty());
    }

    #[test]
    fn wcstr_trim_end_all_whitespace_becomes_empty() {
        let mut ws = WCString::from_str(" \n").unwrap();
        {
            let wcstr: &mut WCStr =
                unsafe { &mut *(ws.inner.as_mut_slice() as *mut [WChar] as *mut WCStr) };
            wcstr.trim_end();
        }
        let wcstr = ws.as_wcstr();
        assert!(wcstr.is_empty());
        assert_eq!(wcstr.len(), 0);
        assert_eq!(wcstr.to_string_lossy(), "");
    }

    #[test]
    fn wcstr_trim_start_shifts_content() {
        let ws = WCString::from_str("  hi").unwrap();
        let trimmed = ws.as_wcstr().trim_start();
        assert_eq!(trimmed.to_string_lossy(), "hi");
    }

    #[test]
    fn wcstr_trim_start_all_whitespace() {
        let ws = WCString::from_str("   ").unwrap();
        let trimmed = ws.as_wcstr().trim_start();
        assert!(trimmed.is_empty());
    }

    #[test]
    fn wcstr_trim_both() {
        let mut ws = WCString::from_str(" x ").unwrap();
        let trimmed = {
            let wcstr: &mut WCStr =
                unsafe { &mut *(ws.inner.as_mut_slice() as *mut [WChar] as *mut WCStr) };
            wcstr.trim()
        };
        assert_eq!(trimmed.to_string_lossy(), "x");
    }

    // ---- from_ptr_mut ----

    #[test]
    fn from_ptr_mut_and_trim() {
        let mut buf: Vec<WChar> = "hello\n".chars().map(char_to_c).collect();
        buf.push(0);
        let wcstr = unsafe { WCStr::from_ptr_mut(buf.as_mut_ptr()) };
        wcstr.trim_end();
        assert_eq!(wcstr.to_string_lossy(), "hello");
    }

    // ---- Display ----

    #[test]
    fn display_impl() {
        let ws = WCString::from_str("hi!").unwrap();
        assert_eq!(format!("{}", ws.as_wcstr()), "hi!");
    }

    // ---- WNulError Display ----

    #[test]
    fn wnul_error_display() {
        let err = WCString::from_str("a\0b").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("position 1"));
    }

    // ---- from_wide_buf ----

    #[test]
    fn from_wide_buf_copies() {
        let src = WCString::from_str("copy").unwrap();
        let copy = WCString::from_wide_buf(src.as_ptr());
        assert_eq!(copy.to_string_lossy(), "copy");
    }
}
