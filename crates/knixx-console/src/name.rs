//! The names of a board's files, such as its scripts': 1 to MAX_NAME
//! letters, digits, `-` and `_`, so that they are safe in a path.

/// The longest name of a file.
pub const MAX_NAME: usize = 24;

/// Reports whether name is a valid file name: 1 to MAX_NAME letters, digits,
/// `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A valid name, in a fixed buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    buf: [u8; MAX_NAME],
    len: u8,
}

impl Name {
    /// Returns name if it is valid.
    pub fn new(name: &str) -> Option<Name> {
        if !valid_name(name) {
            return None;
        }
        let mut buf = [0; MAX_NAME];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        Some(Name {
            buf,
            len: name.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // The bytes are checked ASCII.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for (name, ok) in [
            ("light", true),
            ("A-b_9", true),
            ("x".repeat(MAX_NAME).as_str(), true),
            ("", false),
            ("x".repeat(MAX_NAME + 1).as_str(), false),
            ("a.b", false),
            ("a/b", false),
            ("a b", false),
        ] {
            assert_eq!(valid_name(name), ok, "{name:?}");
            assert_eq!(Name::new(name).is_some(), ok, "{name:?}");
        }
        assert_eq!(Name::new("light").unwrap().as_str(), "light");
    }
}
