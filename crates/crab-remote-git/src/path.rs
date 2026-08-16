use std::fmt;

use bytes::Bytes;

use crate::{Error, PathError, Result};

/// A validated repository path that preserves Git's raw name bytes.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitPath(Bytes);

impl GitPath {
    /// Construct the repository root.
    #[must_use]
    pub const fn root() -> Self {
        Self(Bytes::new())
    }

    /// Validate and own a slash-separated Git path.
    ///
    /// Empty bytes identify the repository root. Non-root paths must not
    /// contain NUL, leading/trailing slash, or adjacent slash bytes.
    pub fn new(bytes: impl Into<Bytes>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.contains(&0) {
            return Err(Error::InvalidPath {
                reason: PathError::Nul,
            });
        }
        if !bytes.is_empty() && bytes.split(|byte| *byte == b'/').any(<[u8]>::is_empty) {
            return Err(Error::InvalidPath {
                reason: PathError::EmptyComponent,
            });
        }
        Ok(Self(bytes))
    }

    /// Return the exact path bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Return whether this is the repository root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate over exact path components without filesystem normalization.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.0
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
    }

    /// Return the final raw component, or `None` for the repository root.
    #[must_use]
    pub fn file_name(&self) -> Option<&[u8]> {
        self.components().next_back()
    }

    /// Append one raw tree-entry name.
    pub fn join(&self, component: &[u8]) -> Result<Self> {
        if component.is_empty() {
            return Err(Error::InvalidPath {
                reason: PathError::EmptyComponent,
            });
        }
        if component.contains(&0) {
            return Err(Error::InvalidPath {
                reason: PathError::Nul,
            });
        }
        if component.contains(&b'/') {
            return Err(Error::InvalidPath {
                reason: PathError::SlashInComponent,
            });
        }

        let separator = usize::from(!self.0.is_empty());
        let capacity = self
            .0
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(component.len()))
            .ok_or(Error::LimitExceeded {
                limit: "path bytes",
                actual: u64::MAX,
                maximum: usize::MAX as u64,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|source| Error::Allocation {
                requested: capacity,
                source,
            })?;
        bytes.extend_from_slice(&self.0);
        if separator != 0 {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(component);
        Ok(Self(Bytes::from(bytes)))
    }
}

impl AsRef<[u8]> for GitPath {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            match byte {
                b'\\' => formatter.write_str("\\\\")?,
                0x20..=0x7e => write!(formatter, "{}", char::from(*byte))?,
                _ => write!(formatter, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_preserves_components_and_dot_like_names() {
        let path = GitPath::new(Bytes::from_static(b"src/../\xff.rs")).expect("valid path");
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            vec![b"src".as_slice(), b"..".as_slice(), b"\xff.rs".as_slice()]
        );
        assert_eq!(path.to_string(), "src/../\\xff.rs");
    }

    #[test]
    fn path_rejects_nul_and_empty_components() {
        for (value, reason) in [
            (b"a\0b".as_slice(), PathError::Nul),
            (b"/a".as_slice(), PathError::EmptyComponent),
            (b"a//b".as_slice(), PathError::EmptyComponent),
            (b"a/".as_slice(), PathError::EmptyComponent),
        ] {
            assert!(matches!(
                GitPath::new(Bytes::copy_from_slice(value)),
                Err(Error::InvalidPath { reason: actual }) if actual == reason
            ));
        }
    }

    #[test]
    fn path_order_and_hash_use_exact_bytes() {
        let lower = GitPath::new(Bytes::from_static(b"a")).expect("lower");
        let upper = GitPath::new(Bytes::from_static(b"\xff")).expect("upper");
        assert!(lower < upper);
        assert_ne!(lower, upper);
    }
}
