//! Splitting what is typed into writes for the board.

/// Ends the session: Ctrl-], as in telnet.
pub const ESCAPE: u8 = 0x1d;

/// The payload of an ATT write with the default MTU of 23.
pub const DEFAULT_CHUNK: usize = 20;

/// Splits p, read from the terminal, into writes of at most size bytes. It
/// stops at ESCAPE, which it reports; the bytes before it are still sent. A
/// size of 0 means DEFAULT_CHUNK.
pub fn chunks(p: &[u8], size: usize) -> (Vec<&[u8]>, bool) {
    let size = if size == 0 { DEFAULT_CHUNK } else { size };
    let (p, quit) = match p.iter().position(|&b| b == ESCAPE) {
        Some(i) => (&p[..i], true),
        None => (p, false),
    };
    (p.chunks(size).collect(), quit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split() {
        let long = [b'x'; 45];
        let l = |n: usize| &long[..n];
        for (name, input, size, want, quit) in [
            ("empty", &b""[..], 20, vec![], false),
            ("key", b"a", 20, vec![&b"a"[..]], false),
            ("line", b"version\r", 20, vec![&b"version\r"[..]], false),
            ("exact", l(20), 20, vec![l(20)], false),
            ("split", &long[..], 20, vec![l(20), l(20), l(5)], false),
            ("bigger MTU", &long[..], 244, vec![&long[..]], false),
            ("default size", &long[..], 0, vec![l(20), l(20), l(5)], false),
            ("escape alone", &[ESCAPE][..], 20, vec![], true),
            ("escape inside", b"ab\x1dcd", 20, vec![&b"ab"[..]], true),
        ] {
            assert_eq!(chunks(input, size), (want, quit), "{name}");
        }
    }
}
