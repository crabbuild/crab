//! Git tag discovery and peeling helpers.
//!
//! This Module owns local annotated-tag detection. Push orchestration decides
//! whether a peeled tag should become a manifest hint or a follow-tags spec.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gix_hash::ObjectId;
use gix_object::Find;
use tracing::debug;

/// Result alias for Git tag peeling.
pub type Result<T> = std::result::Result<T, TagPeelError>;

/// Errors returned while opening local Git tag/ref/object data.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum TagPeelError {
    /// Opening the Git object database failed.
    #[error("failed to open git ODB at {path}: {source}")]
    OpenOdb {
        /// Object database path.
        path: PathBuf,
        /// Source error from Gitoxide.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Opening or iterating the local ref store failed.
    #[error("failed to iterate git refs: {source}")]
    RefIter {
        /// Source error from Gitoxide.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Reading an individual local ref failed.
    #[error("failed to read git ref: {source}")]
    ReadRef {
        /// Source error from Gitoxide.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// An object required to peel an annotated tag is missing.
    #[error("annotated tag {tag_oid} references missing object {object_oid}")]
    MissingObject {
        /// Root annotated tag object id.
        tag_oid: String,
        /// Missing object id in the tag chain.
        object_oid: String,
    },

    /// Reading an object required to peel an annotated tag failed.
    #[error("failed to read object {object_oid} for annotated tag {tag_oid}: {source}")]
    ReadObject {
        /// Root annotated tag object id.
        tag_oid: String,
        /// Object id that could not be read.
        object_oid: String,
        /// Source error from Gitoxide.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Decoding an annotated tag object failed.
    #[error("failed to decode object {object_oid} for annotated tag {tag_oid}: {source}")]
    DecodeTag {
        /// Root annotated tag object id.
        tag_oid: String,
        /// Malformed tag object id.
        object_oid: String,
        /// Source error from Gitoxide.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// An annotated tag chain exceeded the supported recursion limit.
    #[error("annotated tag {tag_oid} exceeds the tag-chain recursion limit")]
    RecursionLimit {
        /// Root annotated tag object id.
        tag_oid: String,
    },
}

/// A local annotated tag ref and the commit it peels to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedTagRef {
    /// Full tag ref name, such as `refs/tags/v1.0.0`.
    pub name: String,
    /// The tag object's SHA-1.
    pub tag_sha: String,
    /// The commit SHA-1 reached by peeling the tag object.
    pub peeled_commit: String,
}

/// Discover annotated tag refs under `refs/tags/` and peel each to a commit.
///
/// Lightweight tags, symbolic tags, unreadable refs, malformed object ids,
/// missing tag objects, and tags that do not peel to a commit are skipped.
/// Opening the ref store or object database returns [`TagPeelError`].
pub fn annotated_tag_refs_at(git_dir: &Path) -> Result<Vec<AnnotatedTagRef>> {
    annotated_tag_refs_with_policy(git_dir, TagReadPolicy::Lenient)
}

/// Discover annotated tag refs and fail if any candidate cannot be read.
///
/// Lightweight, symbolic, and non-commit tags are not candidates and remain
/// excluded. Missing, unreadable, malformed, or excessively nested annotated
/// tag objects return [`TagPeelError`].
pub fn annotated_tag_refs_strict_at(git_dir: &Path) -> Result<Vec<AnnotatedTagRef>> {
    annotated_tag_refs_with_policy(git_dir, TagReadPolicy::Strict)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagReadPolicy {
    Lenient,
    Strict,
}

fn annotated_tag_refs_with_policy(
    git_dir: &Path,
    policy: TagReadPolicy,
) -> Result<Vec<AnnotatedTagRef>> {
    let objects_dir = git_dir.join("objects");
    let odb = gix_odb::at(&objects_dir).map_err(|source| TagPeelError::OpenOdb {
        path: objects_dir,
        source: boxed_source(source),
    })?;

    let store = crate::ref_resolve::open_ref_store(git_dir);
    let platform = store.iter().map_err(|source| TagPeelError::RefIter {
        source: boxed_source(source),
    })?;
    let iter = platform.all().map_err(|source| TagPeelError::RefIter {
        source: boxed_source(source),
    })?;

    let mut tags = Vec::new();
    for reference in iter {
        let reference = match reference {
            Ok(reference) => reference,
            Err(source) if policy == TagReadPolicy::Lenient => {
                debug!(error = %source, "tag peel: skipping unreadable ref");
                continue;
            }
            Err(source) => {
                return Err(TagPeelError::ReadRef {
                    source: boxed_source(source),
                });
            }
        };

        let name = reference.name.to_string();
        if !name.starts_with("refs/tags/") {
            continue;
        }

        let tag_oid = match reference.target {
            gix_ref::Target::Object(oid) => oid,
            gix_ref::Target::Symbolic(_) => {
                debug!(tag = %name, "tag peel: skipping symbolic tag");
                continue;
            }
        };

        let Some(peeled_commit) = peel_annotated_tag_to_commit(&odb, tag_oid, policy)? else {
            continue;
        };

        tags.push(AnnotatedTagRef {
            name,
            tag_sha: tag_oid.to_hex().to_string(),
            peeled_commit: peeled_commit.to_hex().to_string(),
        });
    }

    Ok(tags)
}

/// Build a `ref_name -> peeled commit SHA` map for annotated tags in `refs`.
///
/// The input map should contain direct ref targets. Non-tag refs and
/// lightweight tags produce no entries.
pub fn peeled_tag_refs_at(
    git_dir: &Path,
    refs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !refs.keys().any(|name| name.starts_with("refs/tags/")) {
        return Ok(out);
    }

    let objects_dir = git_dir.join("objects");
    let odb = gix_odb::at(&objects_dir).map_err(|source| TagPeelError::OpenOdb {
        path: objects_dir,
        source: boxed_source(source),
    })?;

    for (ref_name, sha) in refs {
        if !ref_name.starts_with("refs/tags/") {
            continue;
        }

        let oid = match ObjectId::from_hex(sha.as_bytes()) {
            Ok(oid) => oid,
            Err(error) => {
                debug!(
                    ref_name = %ref_name,
                    sha = %sha,
                    error = %error,
                    "tag peel: skipping invalid tag object id"
                );
                continue;
            }
        };

        let Some(peeled_commit) = peel_annotated_tag_to_commit(&odb, oid, TagReadPolicy::Lenient)?
        else {
            continue;
        };

        out.insert(ref_name.clone(), peeled_commit.to_hex().to_string());
    }

    Ok(out)
}

fn peel_annotated_tag_to_commit(
    odb: &impl Find,
    tag_oid: ObjectId,
    policy: TagReadPolicy,
) -> Result<Option<ObjectId>> {
    let mut current = tag_oid;
    for depth in 0..10 {
        let mut buf = Vec::new();
        let data = match odb.try_find(&current, &mut buf) {
            Ok(Some(data)) => data,
            Ok(None) => {
                let error = TagPeelError::MissingObject {
                    tag_oid: tag_oid.to_hex().to_string(),
                    object_oid: current.to_hex().to_string(),
                };
                return handle_peel_error(policy, error);
            }
            Err(source) => {
                let error = TagPeelError::ReadObject {
                    tag_oid: tag_oid.to_hex().to_string(),
                    object_oid: current.to_hex().to_string(),
                    source,
                };
                return handle_peel_error(policy, error);
            }
        };

        if data.kind != gix_object::Kind::Tag {
            if depth == 0 {
                debug!(tag = %tag_oid, kind = ?data.kind, "tag peel: lightweight tag");
            }
            return Ok(None);
        }

        let tag = match gix_object::TagRef::from_bytes(data.data, data.hash_kind) {
            Ok(tag) => tag,
            Err(source) => {
                let error = TagPeelError::DecodeTag {
                    tag_oid: tag_oid.to_hex().to_string(),
                    object_oid: current.to_hex().to_string(),
                    source: boxed_source(source),
                };
                return handle_peel_error(policy, error);
            }
        };

        current = tag.target();
        match tag.target_kind {
            gix_object::Kind::Commit => return Ok(Some(current)),
            gix_object::Kind::Tag => continue,
            other => {
                debug!(tag = %tag_oid, target = %current, kind = ?other, "tag peel: tag target is not a commit");
                return Ok(None);
            }
        }
    }

    handle_peel_error(
        policy,
        TagPeelError::RecursionLimit {
            tag_oid: tag_oid.to_hex().to_string(),
        },
    )
}

fn handle_peel_error(policy: TagReadPolicy, error: TagPeelError) -> Result<Option<ObjectId>> {
    if policy == TagReadPolicy::Strict {
        return Err(error);
    }
    debug!(error = %error, "tag peel: skipping unreadable annotated tag");
    Ok(None)
}

fn boxed_source<E>(source: E) -> Box<dyn std::error::Error + Send + Sync>
where
    E: std::error::Error + Send + Sync + 'static,
{
    Box::new(source)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn annotated_tag_refs_at_skips_lightweight_tags() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");

        let tags = annotated_tag_refs_at(&git_dir).expect("tags");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "refs/tags/v1");
        assert_eq!(tags[0].tag_sha, git(repo.path(), &["rev-parse", "v1"]));
        assert_eq!(
            tags[0].peeled_commit,
            git(repo.path(), &["rev-parse", "v1^{}"])
        );
    }

    #[test]
    fn peeled_tag_refs_at_returns_only_annotated_tags() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        let mut refs = BTreeMap::new();
        refs.insert(
            "refs/tags/v1".to_owned(),
            git(repo.path(), &["rev-parse", "v1"]),
        );
        refs.insert(
            "refs/tags/light".to_owned(),
            git(repo.path(), &["rev-parse", "light"]),
        );

        let peeled = peeled_tag_refs_at(&git_dir, &refs).expect("peeled refs");
        assert_eq!(peeled.len(), 1);
        assert_eq!(
            peeled.get("refs/tags/v1").map(String::as_str),
            Some(git(repo.path(), &["rev-parse", "v1^{}"]).as_str())
        );
    }

    #[test]
    fn strict_tag_discovery_rejects_missing_tag_object() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        let tag_oid = git(repo.path(), &["rev-parse", "refs/tags/v1"]);
        let object_path = git_dir
            .join("objects")
            .join(&tag_oid[..2])
            .join(&tag_oid[2..]);
        std::fs::remove_file(object_path).expect("remove loose tag object");

        let error = annotated_tag_refs_strict_at(&git_dir).expect_err("missing tag must fail");

        assert!(matches!(error, TagPeelError::MissingObject { .. }));
    }

    #[test]
    fn strict_tag_discovery_rejects_unreadable_tag_ref() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        std::fs::write(git_dir.join("refs/tags/broken"), b"not-an-object-id\n")
            .expect("write malformed tag ref");

        let error = annotated_tag_refs_strict_at(&git_dir).expect_err("bad ref must fail");

        assert!(matches!(error, TagPeelError::ReadRef { .. }));
    }

    #[test]
    fn strict_tag_discovery_rejects_malformed_tag_object() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        std::fs::write(repo.path().join("malformed.tag"), b"not a tag object\n")
            .expect("write malformed tag body");
        let malformed_oid = git(
            repo.path(),
            &[
                "hash-object",
                "--literally",
                "-t",
                "tag",
                "-w",
                "malformed.tag",
            ],
        );
        std::fs::write(
            git_dir.join("refs/tags/broken-tag"),
            format!("{malformed_oid}\n"),
        )
        .expect("write ref to malformed tag object");

        let error = annotated_tag_refs_strict_at(&git_dir).expect_err("bad tag must fail");

        assert!(matches!(error, TagPeelError::DecodeTag { .. }));
    }

    #[test]
    fn strict_tag_discovery_rejects_excessive_tag_nesting() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        git(repo.path(), &["tag", "-a", "nested-0", "-m", "nested zero"]);
        for depth in 1..=10 {
            let name = format!("nested-{depth}");
            let target = format!("nested-{}", depth - 1);
            git(repo.path(), &["tag", "-a", &name, &target, "-m", &name]);
        }

        let error = annotated_tag_refs_strict_at(&git_dir).expect_err("deep tag must fail");

        assert!(matches!(error, TagPeelError::RecursionLimit { .. }));
    }

    #[test]
    fn strict_tag_discovery_excludes_legitimate_non_candidates() {
        let repo = git_repo_with_tags();
        let git_dir = repo.path().join(".git");
        let blob_oid = git(repo.path(), &["hash-object", "-w", "file.txt"]);
        git(
            repo.path(),
            &["tag", "-a", "blob-tag", &blob_oid, "-m", "blob tag"],
        );
        git(
            repo.path(),
            &["symbolic-ref", "refs/tags/symbolic", "refs/heads/master"],
        );

        let tags = annotated_tag_refs_strict_at(&git_dir).expect("strict discovery");

        assert_eq!(
            tags.iter().map(|tag| tag.name.as_str()).collect::<Vec<_>>(),
            vec!["refs/tags/v1"]
        );
    }

    fn git_repo_with_tags() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init"]);
        git(tmp.path(), &["config", "user.name", "Crab Test"]);
        git(tmp.path(), &["config", "user.email", "crab@example.com"]);
        std::fs::write(tmp.path().join("file.txt"), b"hello\n").expect("write file");
        git(tmp.path(), &["add", "file.txt"]);
        git(tmp.path(), &["commit", "-m", "initial"]);
        git(tmp.path(), &["tag", "-a", "v1", "-m", "v1"]);
        git(tmp.path(), &["tag", "light"]);
        tmp
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}
