//! Helpers for building path-shaped metadata keys.
//!
//! Holt's core API still accepts opaque byte keys. These helpers are
//! only for callers whose metadata keys are naturally slash-separated
//! paths, such as object names, directory entries, or artifact paths.

use std::fmt;

const DELIMITER: u8 = b'/';
const NUL: u8 = b'\0';

/// Error returned when a path-shaped key segment is not valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPathError {
    /// Empty segments would create ambiguous doubled delimiters.
    EmptySegment,
    /// Segments are slash-free; callers must push each component
    /// separately.
    ContainsDelimiter,
    /// NUL is rejected so helper-built keys stay path-like and
    /// FFI-friendly.
    ContainsNul,
    /// `.` and `..` are rejected so filesystem-style callers do not
    /// accidentally mix canonical and non-canonical paths.
    DotSegment,
}

impl fmt::Display for KeyPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySegment => write!(f, "key path segment must not be empty"),
            Self::ContainsDelimiter => write!(f, "key path segment must not contain '/'"),
            Self::ContainsNul => write!(f, "key path segment must not contain NUL"),
            Self::DotSegment => write!(f, "key path segment must not be '.' or '..'"),
        }
    }
}

impl std::error::Error for KeyPathError {}

/// Owned builder for slash-separated byte keys.
///
/// This type does not add filesystem semantics to Holt. It only
/// helps callers construct canonical byte keys without hand-written
/// `format!` strings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPathBuf {
    bytes: Vec<u8>,
}

impl KeyPathBuf {
    /// Create an empty key builder.
    #[must_use]
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Create a key under a namespace segment.
    ///
    /// `KeyPathBuf::with_namespace(b"o")` starts the key as `o/`.
    pub fn with_namespace(ns: impl AsRef<[u8]>) -> Result<Self, KeyPathError> {
        validate_segment(ns.as_ref())?;
        let mut bytes = Vec::with_capacity(ns.as_ref().len() + 1);
        bytes.extend_from_slice(ns.as_ref());
        bytes.push(DELIMITER);
        Ok(Self { bytes })
    }

    /// Append one slash-free path segment.
    pub fn push(&mut self, segment: impl AsRef<[u8]>) -> Result<(), KeyPathError> {
        validate_segment(segment.as_ref())?;
        if !self.bytes.is_empty() && !self.bytes.ends_with(&[DELIMITER]) {
            self.bytes.push(DELIMITER);
        }
        self.bytes.extend_from_slice(segment.as_ref());
        Ok(())
    }

    /// Borrow the underlying byte key.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the builder and return the byte key.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Convert this key into a prefix suitable for `scan` /
    /// `scan_keys`.
    ///
    /// Non-empty prefixes are guaranteed to end in `/`, so scanning
    /// `foo/` does not also match `foobar`.
    #[must_use]
    pub fn into_prefix(mut self) -> KeyPrefixBuf {
        if !self.bytes.is_empty() && !self.bytes.ends_with(&[DELIMITER]) {
            self.bytes.push(DELIMITER);
        }
        KeyPrefixBuf { bytes: self.bytes }
    }

    /// Number of bytes in the key.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when no namespace or segment has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl AsRef<[u8]> for KeyPathBuf {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Owned slash-terminated scan prefix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPrefixBuf {
    bytes: Vec<u8>,
}

impl KeyPrefixBuf {
    /// Borrow the underlying prefix bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the prefix and return the bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Number of bytes in the prefix.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when this prefix scans the whole keyspace.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl AsRef<[u8]> for KeyPrefixBuf {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

fn validate_segment(segment: &[u8]) -> Result<(), KeyPathError> {
    if segment.is_empty() {
        return Err(KeyPathError::EmptySegment);
    }
    if segment.contains(&DELIMITER) {
        return Err(KeyPathError::ContainsDelimiter);
    }
    if segment.contains(&NUL) {
        return Err(KeyPathError::ContainsNul);
    }
    if segment == b"." || segment == b".." {
        return Err(KeyPathError::DotSegment);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{KeyPathBuf, KeyPathError};

    #[test]
    fn builds_namespaced_key_from_segments() {
        let mut key = KeyPathBuf::with_namespace(b"o").unwrap();
        key.push(b"bucket-a").unwrap();
        key.push(b"photos").unwrap();
        key.push(b"img.jpg").unwrap();
        assert_eq!(key.as_bytes(), b"o/bucket-a/photos/img.jpg");
    }

    #[test]
    fn prefix_is_slash_terminated() {
        let mut key = KeyPathBuf::with_namespace(b"o").unwrap();
        key.push(b"bucket-a").unwrap();
        key.push(b"photos").unwrap();
        let prefix = key.into_prefix();
        assert_eq!(prefix.as_bytes(), b"o/bucket-a/photos/");
    }

    #[test]
    fn namespace_only_prefix_keeps_single_trailing_slash() {
        let prefix = KeyPathBuf::with_namespace(b"o").unwrap().into_prefix();
        assert_eq!(prefix.as_bytes(), b"o/");
    }

    #[test]
    fn empty_prefix_scans_everything() {
        let prefix = KeyPathBuf::new().into_prefix();
        assert!(prefix.is_empty());
    }

    #[test]
    fn rejects_ambiguous_segments() {
        assert_eq!(
            KeyPathBuf::new().push(b"").unwrap_err(),
            KeyPathError::EmptySegment
        );
        assert_eq!(
            KeyPathBuf::new().push(b"a/b").unwrap_err(),
            KeyPathError::ContainsDelimiter
        );
        assert_eq!(
            KeyPathBuf::new().push(b"a\0b").unwrap_err(),
            KeyPathError::ContainsNul
        );
        assert_eq!(
            KeyPathBuf::new().push(b"..").unwrap_err(),
            KeyPathError::DotSegment
        );
    }
}
