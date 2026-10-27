//! The C string functions littlefs uses that compiler-builtins lacks, for
//! the bare-metal build, which has no C library. The logic works on byte
//! slices so that the host tests check it against the C library; the
//! exported functions, built for the target only, wrap it.

/// The length of the span of s, up to its terminator, of bytes in set
/// (strspn) or not in set (strcspn).
pub(crate) fn span(s: &[u8], set: &[u8], accept: bool) -> usize {
    s.iter()
        .take_while(|&&b| b != 0 && set.contains(&b) == accept)
        .count()
}

/// The index of the first c in s, its terminator included, as strchr finds
/// it (strchr(s, 0) finds the terminator).
pub(crate) fn find(s: &[u8], c: u8) -> Option<usize> {
    for (i, &b) in s.iter().enumerate() {
        if b == c {
            return Some(i);
        }
        if b == 0 {
            return None;
        }
    }
    None
}

#[cfg(target_os = "none")]
mod exported {
    use core::ffi::{CStr, c_char, c_int};

    /// Returns the bytes of a C string with its terminator.
    ///
    /// # Safety
    /// s must point to a terminated C string.
    unsafe fn bytes<'a>(s: *const c_char) -> &'a [u8] {
        // Safety: as the caller promises.
        unsafe { CStr::from_ptr(s) }.to_bytes_with_nul()
    }

    /// # Safety
    /// s and accept must be terminated C strings.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn strspn(s: *const c_char, accept: *const c_char) -> usize {
        // Safety: as the caller promises.
        unsafe { super::span(bytes(s), CStr::from_ptr(accept).to_bytes(), true) }
    }

    /// # Safety
    /// s and reject must be terminated C strings.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn strcspn(s: *const c_char, reject: *const c_char) -> usize {
        // Safety: as the caller promises.
        unsafe { super::span(bytes(s), CStr::from_ptr(reject).to_bytes(), false) }
    }

    /// # Safety
    /// s must be a terminated C string.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn strchr(s: *const c_char, c: c_int) -> *mut c_char {
        // Safety: as the caller promises.
        match super::find(unsafe { bytes(s) }, c as u8) {
            // Safety: i is within s.
            Some(i) => unsafe { s.add(i) as *mut c_char },
            None => core::ptr::null_mut(),
        }
    }

    /// # Safety
    /// src must be a terminated C string and dst have room for it.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn strcpy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
        // Safety: as the caller promises.
        unsafe {
            let b = bytes(src);
            core::ptr::copy_nonoverlapping(b.as_ptr() as *const c_char, dst, b.len());
        }
        dst
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::{c_char, c_int};

    unsafe extern "C" {
        fn strspn(s: *const c_char, accept: *const c_char) -> usize;
        fn strcspn(s: *const c_char, reject: *const c_char) -> usize;
        fn strchr(s: *const c_char, c: c_int) -> *mut c_char;
    }

    const CASES: &[&[u8]] = &[
        b"\0",
        b"/\0",
        b"//knixx.ini\0",
        b"knixx.ini/\0",
        b"a/b/c\0",
        b"..\0",
    ];
    const SETS: &[&[u8]] = &[b"/\0", b"\0", b"ab\0", b".\0"];

    #[test]
    fn spans_match_libc() {
        for s in CASES {
            for set in SETS {
                let set_bytes = &set[..set.len() - 1];
                // Safety: both are terminated.
                let (a, r) = unsafe {
                    (
                        strspn(s.as_ptr() as _, set.as_ptr() as _),
                        strcspn(s.as_ptr() as _, set.as_ptr() as _),
                    )
                };
                assert_eq!(span(s, set_bytes, true), a, "strspn({s:?}, {set:?})");
                assert_eq!(span(s, set_bytes, false), r, "strcspn({s:?}, {set:?})");
            }
        }
    }

    #[test]
    fn find_matches_libc() {
        for s in CASES {
            for c in [b'/', b'.', 0, b'z'] {
                // Safety: s is terminated.
                let p = unsafe { strchr(s.as_ptr() as _, c as c_int) };
                let want = (!p.is_null()).then(|| p as usize - s.as_ptr() as usize);
                assert_eq!(find(s, c), want, "strchr({s:?}, {c})");
            }
        }
    }
}
