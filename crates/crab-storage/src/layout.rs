//! Repository object path routing.

use std::fmt::Display;

use crab_types::storage::StorageScope;
use object_store::path::Path as ObjectPath;

/// The default bucket-level prefix for all content-addressed objects.
pub const GLOBAL_PREFIX: &str = ".crab";
/// Number of leading hash characters used to partition global content.
pub const GLOBAL_CONTENT_FANOUT_WIDTH: usize = 2;

/// Build the directory containing one global content kind.
#[must_use]
pub fn global_content_prefix(global_prefix: &str, kind: &str) -> ObjectPath {
    ObjectPath::from(format!("{global_prefix}/{kind}"))
}

/// Build one populated hash-partition directory below a global content kind.
#[must_use]
pub fn global_content_partition_prefix(
    global_prefix: &str,
    kind: &str,
    partition: &str,
) -> ObjectPath {
    ObjectPath::from(format!("{global_prefix}/{kind}/{partition}"))
}

/// Build a two-hex-fan-out path for one global content-addressed object.
///
/// `hash` must be a lowercase 64-character hexadecimal content hash.
#[must_use]
pub fn global_content_path(global_prefix: &str, kind: &str, hash: &str) -> ObjectPath {
    let partition = hash.get(..GLOBAL_CONTENT_FANOUT_WIDTH).unwrap_or(hash);
    global_content_partition_prefix(global_prefix, kind, partition).join(hash)
}

/// Build a global content path under Crab's canonical bucket prefix.
///
/// `hash` must be a lowercase 64-character hexadecimal content hash.
#[must_use]
pub fn canonical_global_content_path(kind: &str, hash: &str) -> ObjectPath {
    global_content_path(GLOBAL_PREFIX, kind, hash)
}

/// Extract and validate the hash from a canonical fan-out content path.
#[must_use]
pub fn content_hash_from_path<'a>(path: &'a str, kind: &str) -> Option<&'a str> {
    let mut parts = path.rsplit('/');
    let hash = parts.next()?;
    let partition = parts.next()?;
    if parts.next()? != kind
        || hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || partition != hash.get(..GLOBAL_CONTENT_FANOUT_WIDTH)?
    {
        return None;
    }
    Some(hash)
}

/// Provides optional scoped prefixes for path-limited repository views.
pub trait StorageScopeProvider {
    fn storage_scope(&self) -> Option<&StorageScope>;
}

impl StorageScopeProvider for crate::store::Store {
    fn storage_scope(&self) -> Option<&StorageScope> {
        self.storage_scope()
    }
}

/// Routes content-addressed paths to a global prefix and mutable paths to `{repo}/`.
#[derive(Clone)]
pub struct StoreLayout<S> {
    /// Underlying object store or store facade.
    store: S,
    /// Per-repo prefix such as `org/models`.
    repo_prefix: String,
    /// Global prefix, normally `.crab` or a scoped view-local `.crab`.
    global_prefix: String,
}

/// Object type classification for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    /// Content-addressed, stored globally: xorbs and shards.
    Global,
    /// Per-repo mutable state: refs, manifests, packs, locks.
    RepoLocal,
}

impl<S: StorageScopeProvider> StoreLayout<S> {
    /// Creates a new layout routing content-addressed objects to `.crab/`
    /// and per-repo state to `repo_prefix`.
    #[must_use]
    pub fn new(store: S, repo_prefix: String) -> Self {
        let (repo_prefix, global_prefix) = match store.storage_scope() {
            Some(scope) => (scope.repo_prefix.clone(), scope.global_prefix.clone()),
            None => (repo_prefix, GLOBAL_PREFIX.to_owned()),
        };
        Self {
            store,
            repo_prefix,
            global_prefix,
        }
    }
}

impl<S> StoreLayout<S> {
    /// Creates a layout with an explicit global prefix.
    ///
    /// Used by service-owned ACL view builders that need to write a
    /// filtered repository's content-addressed objects under the view
    /// prefix before scoped client credentials exist.
    #[must_use]
    pub fn with_global_prefix(store: S, repo_prefix: String, global_prefix: String) -> Self {
        Self {
            store,
            repo_prefix,
            global_prefix,
        }
    }

    /// Classify an object kind for routing.
    ///
    /// Content-addressed kinds (`"xorbs"`, `"shards"`) route to the
    /// global prefix. Everything else is per-repo.
    #[must_use]
    pub fn classify(kind: &str) -> ObjectType {
        match kind {
            "xorbs" | "shards" => ObjectType::Global,
            _ => ObjectType::RepoLocal,
        }
    }

    /// Build the full object-store path for a content-addressed object.
    ///
    /// Returns `.crab/{kind}/{first-two-hex}/{hash}`.
    #[must_use]
    pub fn global_path(&self, kind: &str, hash: &str) -> ObjectPath {
        global_content_path(&self.global_prefix, kind, hash)
    }

    /// Build the full object-store path for a per-repo object.
    ///
    /// Returns `{repo_prefix}/{relative_path}`.
    #[must_use]
    pub fn repo_path(&self, relative_path: &str) -> ObjectPath {
        ObjectPath::from(format!("{}/{relative_path}", self.repo_prefix))
    }

    /// Convenience: xorb path at `.crab/xorbs/{first-two-hex}/{hash}`.
    #[must_use]
    pub fn xorb_path(&self, hash: &(impl Display + ?Sized)) -> ObjectPath {
        self.global_path("xorbs", &hash.to_string())
    }

    /// Convenience: shard path at `.crab/shards/{first-two-hex}/{hash}`.
    #[must_use]
    pub fn shard_path(&self, hash: &(impl Display + ?Sized)) -> ObjectPath {
        self.global_path("shards", &hash.to_string())
    }

    /// Path to the compacted repository snapshot: `{repo}/manifest`.
    #[must_use]
    pub fn manifest_path(&self) -> ObjectPath {
        self.repo_path("manifest")
    }

    /// Prefix containing immutable historical manifest roots.
    #[must_use]
    pub fn manifest_history_prefix(&self) -> ObjectPath {
        self.repo_path("manifests/history")
    }

    /// Path to one immutable historical manifest root.
    #[must_use]
    pub fn manifest_history_path(&self, generation: u64, digest: &str) -> ObjectPath {
        self.repo_path(&format!("manifests/history/{generation:020}-{digest}.json"))
    }

    /// Prefix containing independently mutable ref-journal heads.
    #[must_use]
    pub fn ref_journal_heads_prefix(&self) -> ObjectPath {
        self.repo_path("refs/journal/heads")
    }

    /// Path to one independently mutable ref-journal head.
    #[must_use]
    pub fn ref_journal_head_path(&self, ref_name_hash: &str) -> ObjectPath {
        self.repo_path(&format!("refs/journal/heads/{ref_name_hash}.json"))
    }

    /// Path to one immutable ref-journal transaction body.
    #[must_use]
    pub fn ref_journal_transaction_path(&self, transaction_id: &str) -> ObjectPath {
        self.repo_path(&format!("refs/journal/transactions/{transaction_id}.json"))
    }

    /// Prefix containing committed transactions not yet cleaned by compaction.
    #[must_use]
    pub fn ref_journal_active_prefix(&self) -> ObjectPath {
        self.repo_path("refs/journal/active")
    }

    /// Path to one atomic ref-journal visibility marker.
    #[must_use]
    pub fn ref_journal_active_path(&self, transaction_id: &str) -> ObjectPath {
        self.repo_path(&format!("refs/journal/active/{transaction_id}.json"))
    }

    /// Path to the immutable compaction frontier for one manifest Git state.
    #[must_use]
    pub fn ref_journal_frontier_path(&self, git_validation_digest: &str) -> ObjectPath {
        self.repo_path(&format!(
            "refs/journal/frontiers/{git_validation_digest}.json"
        ))
    }

    /// Path to a bulk manifest object: `{repo}/manifests/{prefix}-{hash}`.
    ///
    /// Bulk objects are immutable and content-addressed. `prefix` is one of
    /// `"shard-list"`, `"pack-list"`, etc. `hash` is the blake3 hex digest
    /// of the serialized content.
    #[must_use]
    pub fn bulk_manifest_path(&self, prefix: &str, hash: &str) -> ObjectPath {
        self.repo_path(&format!("manifests/{prefix}-{hash}"))
    }

    /// Path to a repo-local Git pack object: `{repo}/packs/pack-{id}.pack`.
    #[must_use]
    pub fn pack_path(&self, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
        repo_pack_path(&self.repo_prefix, pack_id)
    }

    /// Path to a repo-local Git pack index object: `{repo}/packs/pack-{id}.idx`.
    #[must_use]
    pub fn pack_index_path(&self, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
        repo_pack_index_path(&self.repo_prefix, pack_id)
    }

    /// Path to a repo-local Git reverse index: `{repo}/packs/pack-{id}.rev`.
    #[must_use]
    pub fn pack_reverse_index_path(&self, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
        repo_pack_reverse_index_path(&self.repo_prefix, pack_id)
    }

    /// Path to a repo-local Git pack metadata object: `{repo}/packs/pack-{id}.meta`.
    #[must_use]
    pub fn pack_metadata_path(&self, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
        repo_pack_metadata_path(&self.repo_prefix, pack_id)
    }

    /// Path to the version-bound integrity receipt for a Git pack body.
    #[must_use]
    pub fn pack_origin_receipt_path(&self, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
        self.repo_path(&format!("metadata/pack-origin/{pack_id}.json"))
    }

    /// Path to one immutable Git object visibility proof.
    #[must_use]
    pub fn git_visibility_path(&self, git_validation_digest: &str) -> ObjectPath {
        self.repo_path(&format!(
            "metadata/git-visibility/v2/{git_validation_digest}.json"
        ))
    }

    /// Path to a v1 visibility proof shipped by Crab 1.0.15.
    #[must_use]
    pub fn git_visibility_v1_path(&self, generation: u64, pack_index_hash: &str) -> ObjectPath {
        self.repo_path(&format!(
            "metadata/git-visibility/{generation:020}-{pack_index_hash}.json"
        ))
    }

    /// Path to immutable ref-update visibility evidence.
    #[must_use]
    pub fn git_visibility_edit_path(&self, evidence_hash: &str) -> ObjectPath {
        self.repo_path(&format!(
            "metadata/git-visibility-edits/{evidence_hash}.json"
        ))
    }

    /// Convenience: bucket-level ref-registry path.
    #[must_use]
    pub fn ref_registry_path(&self) -> ObjectPath {
        ObjectPath::from(format!("{}/ref-registry", self.global_prefix))
    }

    /// Access the underlying store.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The repo prefix.
    #[must_use]
    pub fn repo_prefix(&self) -> &str {
        &self.repo_prefix
    }

    /// The global content-addressed prefix.
    #[must_use]
    pub fn global_prefix(&self) -> &str {
        &self.global_prefix
    }
}

/// Build a repo-local Git pack object path for callers that have only a prefix.
#[must_use]
pub fn repo_pack_path(repo_prefix: &str, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
    ObjectPath::from(format!("{repo_prefix}/{}", pack_relative_path(pack_id)))
}

/// Build a repo-local Git pack index path for callers that have only a prefix.
#[must_use]
pub fn repo_pack_index_path(repo_prefix: &str, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
    ObjectPath::from(format!(
        "{repo_prefix}/{}",
        pack_index_relative_path(pack_id)
    ))
}

/// Build a repo-local Git reverse-index path for callers that have only a prefix.
#[must_use]
pub fn repo_pack_reverse_index_path(
    repo_prefix: &str,
    pack_id: &(impl Display + ?Sized),
) -> ObjectPath {
    ObjectPath::from(format!(
        "{repo_prefix}/{}",
        pack_reverse_index_relative_path(pack_id)
    ))
}

/// Build a repo-local Git pack metadata path for callers that have only a prefix.
#[must_use]
pub fn repo_pack_metadata_path(repo_prefix: &str, pack_id: &(impl Display + ?Sized)) -> ObjectPath {
    ObjectPath::from(format!(
        "{repo_prefix}/{}",
        pack_metadata_relative_path(pack_id)
    ))
}

fn pack_relative_path(pack_id: &(impl Display + ?Sized)) -> String {
    format!("packs/pack-{pack_id}.pack")
}

fn pack_index_relative_path(pack_id: &(impl Display + ?Sized)) -> String {
    format!("packs/pack-{pack_id}.idx")
}

fn pack_reverse_index_relative_path(pack_id: &(impl Display + ?Sized)) -> String {
    format!("packs/pack-{pack_id}.rev")
}

fn pack_metadata_relative_path(pack_id: &(impl Display + ?Sized)) -> String {
    format!("packs/pack-{pack_id}.meta")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct TestStore {
        scope: Option<StorageScope>,
    }

    impl StorageScopeProvider for TestStore {
        fn storage_scope(&self) -> Option<&StorageScope> {
            self.scope.as_ref()
        }
    }

    fn test_layout() -> StoreLayout<TestStore> {
        StoreLayout::new(TestStore::default(), "org/models".to_string())
    }

    #[test]
    fn global_path_routes_to_crab_prefix() {
        let layout = test_layout();
        let hash = format!("ab{}", "1".repeat(62));
        let path = layout.global_path("xorbs", &hash);
        assert_eq!(path.as_ref(), format!(".crab/xorbs/ab/{hash}"));
    }

    #[test]
    fn repo_path_routes_to_repo_prefix() {
        let layout = test_layout();
        let path = layout.repo_path("refs/heads/main");
        assert_eq!(path.as_ref(), "org/models/refs/heads/main");
    }

    #[test]
    fn classify_content_addressed_kinds_as_global() {
        assert_eq!(
            StoreLayout::<TestStore>::classify("xorbs"),
            ObjectType::Global
        );
        assert_eq!(
            StoreLayout::<TestStore>::classify("shards"),
            ObjectType::Global
        );
    }

    #[test]
    fn classify_mutable_kinds_as_repo_local() {
        assert_eq!(
            StoreLayout::<TestStore>::classify("refs"),
            ObjectType::RepoLocal
        );
        assert_eq!(
            StoreLayout::<TestStore>::classify("manifests"),
            ObjectType::RepoLocal
        );
        assert_eq!(
            StoreLayout::<TestStore>::classify("packs"),
            ObjectType::RepoLocal
        );
        assert_eq!(
            StoreLayout::<TestStore>::classify("locks"),
            ObjectType::RepoLocal
        );
    }

    #[test]
    fn xorb_path_uses_hex_hash() {
        let layout = test_layout();
        let hash = "a".repeat(64);
        let path = layout.xorb_path(&hash);
        assert_eq!(path.as_ref(), format!(".crab/xorbs/aa/{hash}"));
    }

    #[test]
    fn shard_path_uses_hex_hash() {
        let layout = test_layout();
        let hash = "b".repeat(64);
        let path = layout.shard_path(&hash);
        assert_eq!(path.as_ref(), format!(".crab/shards/bb/{hash}"));
    }

    #[test]
    fn content_path_parser_requires_matching_lowercase_fanout() {
        let hash = format!("ab{}", "3".repeat(62));

        assert_eq!(
            content_hash_from_path(&format!(".crab/xorbs/ab/{hash}"), "xorbs"),
            Some(hash.as_str())
        );
        assert!(content_hash_from_path(&format!(".crab/xorbs/ac/{hash}"), "xorbs").is_none());
        assert!(content_hash_from_path(&format!(".crab/xorbs/AB/{hash}"), "xorbs").is_none());
        assert!(content_hash_from_path(&format!(".crab/xorbs/ab/{hash}"), "shards").is_none());
        assert!(content_hash_from_path(&format!(".crab/xorbs/{hash}"), "xorbs").is_none());
    }

    #[test]
    fn manifest_path_is_per_repo() {
        let layout = test_layout();
        assert_eq!(layout.manifest_path().as_ref(), "org/models/manifest");
    }

    #[test]
    fn manifest_history_paths_are_stable_and_sortable() {
        let layout = test_layout();
        let digest = "a".repeat(64);

        assert_eq!(
            layout.manifest_history_prefix().as_ref(),
            "org/models/manifests/history"
        );
        assert_eq!(
            layout.manifest_history_path(42, &digest).as_ref(),
            format!("org/models/manifests/history/00000000000000000042-{digest}.json")
        );
    }

    #[test]
    fn bulk_manifest_path_builds_correct_key() {
        let layout = test_layout();
        let path = layout.bulk_manifest_path("shard-list", "deadbeef1234");
        assert_eq!(
            path.as_ref(),
            "org/models/manifests/shard-list-deadbeef1234"
        );
    }

    #[test]
    fn bulk_manifest_path_works_for_pack_list() {
        let layout = test_layout();
        let path = layout.bulk_manifest_path("pack-list", "cafebabe5678");
        assert_eq!(path.as_ref(), "org/models/manifests/pack-list-cafebabe5678");
    }

    #[test]
    fn pack_paths_are_repo_local() {
        let layout = test_layout();
        let pack_id = "a".repeat(64);

        assert_eq!(
            layout.pack_path(&pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.pack")
        );
        assert_eq!(
            layout.pack_index_path(&pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.idx")
        );
        assert_eq!(
            layout.pack_reverse_index_path(&pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.rev")
        );
        assert_eq!(
            layout.pack_metadata_path(&pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.meta")
        );
        assert_eq!(
            layout.pack_origin_receipt_path(&pack_id).as_ref(),
            format!("org/models/metadata/pack-origin/{pack_id}.json")
        );
    }

    #[test]
    fn repo_pack_paths_work_without_layout_instance() {
        let pack_id = "b".repeat(64);

        assert_eq!(
            repo_pack_path("org/models", &pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.pack")
        );
        assert_eq!(
            repo_pack_index_path("org/models", &pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.idx")
        );
        assert_eq!(
            repo_pack_reverse_index_path("org/models", &pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.rev")
        );
        assert_eq!(
            repo_pack_metadata_path("org/models", &pack_id).as_ref(),
            format!("org/models/packs/pack-{pack_id}.meta")
        );
    }

    #[test]
    fn ref_registry_path_is_global() {
        let layout = test_layout();
        assert_eq!(layout.ref_registry_path().as_ref(), ".crab/ref-registry");
    }

    #[test]
    fn store_accessor_returns_inner_store() {
        let layout = test_layout();
        let _store = layout.store();
    }

    #[test]
    fn repo_prefix_accessor() {
        let layout = test_layout();
        assert_eq!(layout.repo_prefix(), "org/models");
    }

    #[test]
    fn scoped_store_routes_repo_and_global_objects_to_view_prefix() {
        let scope_hash = "a".repeat(64);
        let view_prefix = format!("org/models/acl-views/v1/{scope_hash}/7-deadbeef");
        let store = TestStore {
            scope: Some(StorageScope {
                repo_prefix: view_prefix.clone(),
                global_prefix: format!("{view_prefix}/.crab"),
                source_repo: "org/models".to_owned(),
                scope_hash,
            }),
        };
        let layout = StoreLayout::new(store, "org/models".to_owned());
        let hash = "c".repeat(64);

        assert_eq!(layout.repo_prefix(), view_prefix);
        assert_eq!(layout.global_prefix(), format!("{view_prefix}/.crab"));
        assert_eq!(
            layout.manifest_path().as_ref(),
            format!("{view_prefix}/manifest")
        );
        assert!(
            layout
                .xorb_path(&hash)
                .as_ref()
                .starts_with(&format!("{view_prefix}/.crab/xorbs/cc/"))
        );
        assert_eq!(
            layout.ref_registry_path().as_ref(),
            format!("{view_prefix}/.crab/ref-registry")
        );
    }

    #[test]
    fn explicit_global_prefix_routes_service_built_view_objects() {
        let view_prefix = "org/models/acl-views/v1/scope/7-deadbeef".to_owned();
        let layout = StoreLayout::with_global_prefix(
            TestStore::default(),
            view_prefix.clone(),
            format!("{view_prefix}/.crab"),
        );
        let hash = "d".repeat(64);

        assert_eq!(layout.repo_prefix(), view_prefix);
        assert_eq!(layout.global_prefix(), format!("{view_prefix}/.crab"));
        assert!(
            layout
                .xorb_path(&hash)
                .as_ref()
                .starts_with(&format!("{view_prefix}/.crab/xorbs/dd/"))
        );
        assert!(
            layout
                .shard_path(&hash)
                .as_ref()
                .starts_with(&format!("{view_prefix}/.crab/shards/dd/"))
        );
    }
}
