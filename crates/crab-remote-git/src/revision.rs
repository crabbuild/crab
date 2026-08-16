use gix_hash::ObjectId;

use crate::{AnnotatedTag, Error, Result, RevisionError};

/// A revision accepted by repository operations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Revision {
    /// A complete or unqualified reference name resolved against current refs.
    Reference(String),
    /// A complete SHA-1 commit ID whose current-ref reachability must be proven.
    Commit(ObjectId),
}

impl Revision {
    /// Parse a complete SHA-1 ID or retain a reference name for repository resolution.
    ///
    /// Forty lowercase or uppercase hexadecimal digits become a commit request.
    /// Other values remain reference requests so repositories may legitimately
    /// contain hexadecimal branch names; unresolved abbreviated hex is rejected
    /// during repository resolution rather than scanned by prefix.
    pub fn parse(value: &str) -> Result<Self> {
        if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Self::from_oid_hex(value);
        }
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::UnsupportedObjectFormat);
        }
        if value.is_empty() || value.as_bytes().contains(&0) {
            return Err(Error::Revision {
                reason: RevisionError::InvalidReference,
            });
        }
        Ok(Self::Reference(value.to_owned()))
    }

    /// Parse exactly one complete SHA-1 commit ID.
    pub fn from_oid_hex(value: &str) -> Result<Self> {
        if value.len() < 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Revision {
                reason: RevisionError::AbbreviatedObjectId,
            });
        }
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::UnsupportedObjectFormat);
        }
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Revision {
                reason: RevisionError::MalformedObjectId,
            });
        }
        let oid = ObjectId::from_hex(value.as_bytes()).map_err(|_| Error::Revision {
            reason: RevisionError::MalformedObjectId,
        })?;
        Ok(Self::Commit(oid))
    }
}

/// A revision resolved once against one immutable repository generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    /// Original typed request.
    pub requested: Revision,
    /// Complete reference name when resolution began from a reference.
    pub reference: Option<String>,
    /// Immutable commit selected for all snapshot work.
    pub commit: ObjectId,
    /// Annotated tags traversed while peeling the selected reference.
    pub tags: Vec<AnnotatedTag>,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parser_distinguishes_refs_sha1_and_unsupported_hashes() {
        assert!(matches!(
            Revision::parse("refs/heads/main"),
            Ok(Revision::Reference(name)) if name == "refs/heads/main"
        ));
        assert!(matches!(
            Revision::parse("1111111111111111111111111111111111111111"),
            Ok(Revision::Commit(_))
        ));
        assert!(matches!(
            Revision::parse(&"1".repeat(64)),
            Err(Error::UnsupportedObjectFormat)
        ));
    }

    proptest! {
        #[test]
        fn every_complete_sha1_hex_parses(bytes in proptest::array::uniform20(any::<u8>())) {
            let value = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            let revision = Revision::from_oid_hex(&value).expect("complete SHA-1");
            prop_assert!(matches!(revision, Revision::Commit(oid) if oid.as_bytes() == bytes));
        }

        #[test]
        fn every_abbreviated_hex_oid_is_rejected(
            value in proptest::string::string_regex("[0-9a-fA-F]{1,39}").expect("regex")
        ) {
            let rejected = matches!(
                Revision::from_oid_hex(&value),
                Err(Error::Revision { reason: RevisionError::AbbreviatedObjectId })
            );
            prop_assert!(rejected);
        }
    }
}
