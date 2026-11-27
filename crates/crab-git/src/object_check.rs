//! Structural Git object rules shared by receive validation and streamed imports.

use std::collections::HashSet;

use bstr::ByteSlice;
use gix_hash::ObjectId;
use gix_object::Kind;

use crate::pointer_detect::PointerKind;

/// One rule violation in an otherwise identity-verified object.
#[derive(Debug)]
pub(crate) enum Violation {
    Parse(gix_object::decode::Error),
    Invalid(&'static str),
    Path(gix_validate::path::component::Error),
    TagName(gix_validate::tag::name::Error),
}

/// One tree entry borrowed from the inspected object bytes.
pub(crate) struct TreeEntryRef<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) oid: ObjectId,
    pub(crate) mode: u16,
}

/// Graph-relevant content of one valid object.
pub(crate) struct Structure<'a> {
    /// Outgoing closure links with the kind each target must have.
    pub(crate) links: Vec<(ObjectId, Kind)>,
    /// Tree entries, collected only when requested.
    pub(crate) entries: Vec<TreeEntryRef<'a>>,
    /// Number of tree entries, including uncollected ones.
    pub(crate) entry_count: usize,
    /// Commit message bytes for commit observers.
    pub(crate) message: Option<&'a [u8]>,
    pub(crate) pointer: PointerKind,
}

/// Parse one object whose identity was already verified and apply Crab's rules.
///
/// Gitlinks are validated but not linked: they name another repository's commit.
pub(crate) fn inspect(
    kind: Kind,
    data: &[u8],
    collect_entries: bool,
) -> Result<Structure<'_>, Violation> {
    let mut structure = Structure {
        links: Vec::new(),
        entries: Vec::new(),
        entry_count: 0,
        message: None,
        pointer: PointerKind::NotAPointer,
    };
    match kind {
        Kind::Commit => {
            if data.contains(&0) {
                return Err(Violation::Invalid("NUL in commit"));
            }
            let commit = gix_object::CommitRef::from_bytes(data, gix_hash::Kind::Sha1)
                .map_err(Violation::Parse)?;
            commit.author().map_err(Violation::Parse)?;
            commit.committer().map_err(Violation::Parse)?;
            if commit.extra_headers.iter().any(|(name, _)| {
                matches!(
                    name.as_bytes(),
                    b"tree" | b"parent" | b"author" | b"committer"
                )
            }) {
                return Err(Violation::Invalid("misplaced core commit header"));
            }
            structure.links.push((commit.tree(), Kind::Tree));
            structure
                .links
                .extend(commit.parents().map(|parent| (parent, Kind::Commit)));
            structure.message = Some(commit.message.as_bytes());
        }
        Kind::Tree => {
            let mut previous = None;
            let mut names = HashSet::new();
            for entry in gix_object::TreeRefIter::from_bytes(data, gix_hash::Kind::Sha1) {
                let entry = entry.map_err(Violation::Parse)?;
                if previous.as_ref().is_some_and(|previous| previous >= &entry)
                    || !names.insert(entry.filename)
                {
                    return Err(Violation::Invalid("unsorted or duplicate tree entries"));
                }
                let linked = entry_kind(entry.mode.value())?;
                let mode = entry
                    .mode
                    .is_link()
                    .then_some(gix_validate::path::component::Mode::Symlink);
                gix_validate::path::component(
                    entry.filename,
                    mode,
                    gix_validate::path::component::Options {
                        protect_windows: false,
                        protect_hfs: true,
                        protect_ntfs: true,
                    },
                )
                .map_err(Violation::Path)?;
                let target = entry.oid.to_owned();
                if target.is_null() {
                    return Err(Violation::Invalid("null tree entry"));
                }
                if let Some(kind) = linked {
                    structure.links.push((target, kind));
                }
                structure.entry_count += 1;
                if collect_entries {
                    structure.entries.push(TreeEntryRef {
                        name: entry.filename.as_bytes(),
                        oid: target,
                        mode: entry.mode.value(),
                    });
                }
                previous = Some(entry);
            }
        }
        Kind::Tag => {
            let tag = gix_object::TagRef::from_bytes(data, gix_hash::Kind::Sha1)
                .map_err(Violation::Parse)?;
            tag.tagger().map_err(Violation::Parse)?;
            gix_validate::tag::name(tag.name).map_err(Violation::TagName)?;
            structure.links.push((tag.target(), tag.target_kind));
        }
        Kind::Blob => structure.pointer = crate::classify(data),
    }
    if structure.links.iter().any(|(oid, _)| oid.is_null()) {
        return Err(Violation::Invalid("null object link"));
    }
    Ok(structure)
}

/// Return the kind a tree entry links to, or `None` for a gitlink.
fn entry_kind(mode: u16) -> Result<Option<Kind>, Violation> {
    match mode {
        0o040000 => Ok(Some(Kind::Tree)),
        // Git accepts group-writable 100664 entries outside `fsck --strict`;
        // early public histories, including Git's own, still contain them.
        0o100644 | 0o100664 | 0o100755 | 0o120000 => Ok(Some(Kind::Blob)),
        0o160000 => Ok(None),
        _ => Err(Violation::Invalid("unsupported tree mode")),
    }
}
