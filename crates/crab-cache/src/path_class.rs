use std::borrow::Cow;

use crab_xet::hash::MerkleHash;
use serde::{Deserialize, Serialize};

use crate::key::CacheKey;

/// Schema identifier for the cache-service route taxonomy.
pub const CACHE_ROUTE_CONTRACT_SCHEMA: &str = "crab-cache-service.routes.v1";

/// Whether a request path refers to an immutable (content-addressed) or
/// mutable (refs, HEAD, manifests, config, locks) object.
///
/// Shared between the cache service router and the client-side `CachingStore`
/// so both sides agree on what gets cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Immutable,
    Mutable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheObjectKind {
    Xorb,
    Shard,
    Pack,
    PackIndex,
    GeneratedPack,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObjectPath<'a> {
    pub repo_path: &'a str,
    pub kind: CacheObjectKind,
    pub identity: Cow<'a, str>,
}

/// Machine-readable path taxonomy shared by cache-service clients and servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRouteContract {
    pub schema: String,
    pub transport_prefix: String,
    pub immutable: Vec<CacheRoutePattern>,
    pub mutable: Vec<CacheRoutePattern>,
}

/// One route pattern in the cache-service path taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRoutePattern {
    pub kind: String,
    pub pattern: String,
}

/// Returns the current cache-service route taxonomy.
pub fn cache_route_contract() -> CacheRouteContract {
    CacheRouteContract {
        schema: CACHE_ROUTE_CONTRACT_SCHEMA.to_string(),
        transport_prefix: "/v1/".to_string(),
        immutable: route_patterns(&[
            ("xorb", ".crab/xorbs/{first-two-hex}/{hash}"),
            ("shard", ".crab/shards/{first-two-hex}/{hash}"),
            ("pack", "{repo}/packs/pack-{id}.pack"),
            ("pack_index", "{repo}/packs/pack-{id}.idx"),
            (
                "generated_pack",
                "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
            ),
            (
                "generated_pack",
                "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
            ),
            ("metadata", "{repo}/file_index_db/compacted/*.sst"),
            ("metadata", "{repo}/file_index_db/manifest/*.manifest"),
            ("metadata", "{repo}/file_index_db/wal/*.sst"),
            ("metadata", "{repo}/file_index_db/compactions/*.compactions"),
            ("metadata", ".crab/chunk_index_db/compacted/*.sst"),
            ("metadata", ".crab/chunk_index_db/manifest/*.manifest"),
            ("metadata", ".crab/chunk_index_db/wal/*.sst"),
            ("metadata", ".crab/chunk_index_db/compactions/*.compactions"),
        ]),
        mutable: route_patterns(&[
            ("control", "{repo}/refs/heads/*"),
            ("control", "{repo}/HEAD"),
            ("control", "{repo}/locks/*"),
            ("control", "{repo}/packs/pack-{id}.meta"),
            ("control", "{repo}/manifests/*"),
            ("control", "{repo}/pack-list"),
            ("control", "{repo}/shard-list"),
            ("control", ".crab/ref-registry/*"),
            ("metadata", "{repo}/file_index_db/manifest/current"),
            ("metadata", ".crab/chunk_index_db/manifest/current"),
        ]),
    }
}

/// Returns true when a remote route taxonomy matches this build exactly.
pub fn cache_route_contract_matches_current(contract: &CacheRouteContract) -> bool {
    contract == &cache_route_contract()
}

fn route_patterns(patterns: &[(&str, &str)]) -> Vec<CacheRoutePattern> {
    patterns
        .iter()
        .map(|(kind, pattern)| CacheRoutePattern {
            kind: (*kind).to_string(),
            pattern: (*pattern).to_string(),
        })
        .collect()
}

/// Classify a URL path as immutable (cacheable) or mutable (never cached).
///
/// Immutable paths are canonical object keys under the optional `/v1/`
/// cache-service route prefix: `.crab/xorbs/`, `.crab/shards/`, repo
/// `packs/`, generated response packs, or versioned SlateDB metadata objects.
/// Everything else —
/// refs, HEAD, config, locks, embedded noncanonical paths, and unversioned
/// metadata discovery — is mutable.
pub fn classify_path(path: &str) -> PathClass {
    if parse_cache_object_path(path).is_some() {
        PathClass::Immutable
    } else {
        PathClass::Mutable
    }
}

pub fn parse_cache_object_path(path: &str) -> Option<CacheObjectPath<'_>> {
    let path = normalize_transport_path(path);
    parse_global_crab_object(path)
        .or_else(|| parse_pack_object(path))
        .or_else(|| parse_generated_pack_object(path))
        .or_else(|| parse_versioned_metadb_object(path))
}

/// Map a cache-service object path to the local disk cache key, when supported.
///
/// Pack and metadata routes are immutable cache-service objects, but the local
/// disk cache currently stores only xorb and shard object bodies by hash.
#[must_use]
pub fn cache_key_for_path(path: &str) -> Option<CacheKey> {
    let parsed = parse_cache_object_path(path)?;
    let hash = MerkleHash::from_hex(parsed.identity.as_ref()).ok()?;
    match parsed.kind {
        CacheObjectKind::Xorb => Some(CacheKey::Xorb(hash)),
        CacheObjectKind::Shard => Some(CacheKey::Shard(hash)),
        CacheObjectKind::Pack
        | CacheObjectKind::PackIndex
        | CacheObjectKind::GeneratedPack
        | CacheObjectKind::Metadata => None,
    }
}

pub fn parse_mutable_repo_path(path: &str) -> Option<&str> {
    let path = path.trim();
    let path = normalize_transport_path(path).trim_matches('/');
    if parse_cache_object_path(path).is_some() {
        return None;
    }
    if path == ".crab/ref-registry" || path.starts_with(".crab/ref-registry/") {
        return Some(".crab");
    }

    for marker in [
        "/locks/",
        "/refs/",
        "/packs/",
        "/generated-packs/",
        "/manifests/",
        "/file_index_db/",
        "/chunk_index_db/",
    ] {
        if let Some(index) = path.find(marker) {
            return Some(&path[..index]);
        }
    }

    for suffix in [
        "/HEAD",
        "/manifest",
        "/pack-list",
        "/shard-list",
        "/ref-registry",
    ] {
        if let Some(repo_path) = path.strip_suffix(suffix) {
            return Some(repo_path);
        }
    }

    None
}

fn parse_generated_pack_object(path: &str) -> Option<CacheObjectPath<'_>> {
    let marker = "/generated-packs/v1/";
    let marker_position = path.find(marker)?;
    let repo_path = &path[..marker_position];
    let suffix = &path[marker_position + marker.len()..];
    let (kind, extension) = if let Some(suffix) = suffix.strip_prefix("artifacts/") {
        (suffix, ".pack")
    } else {
        (suffix.strip_prefix("requests/")?, ".json")
    };
    let mut parts = kind.split('/');
    let partition = parts.next()?;
    let filename = parts.next()?;
    if repo_path.is_empty() || parts.next().is_some() {
        return None;
    }
    let hash = filename.strip_suffix(extension)?;
    if !is_hash_hex(hash) || partition != hash.get(..2)? {
        return None;
    }
    Some(CacheObjectPath {
        repo_path,
        kind: CacheObjectKind::GeneratedPack,
        identity: Cow::Owned(blake3::hash(path.as_bytes()).to_hex().to_string()),
    })
}

pub(crate) fn normalize_transport_path(path: &str) -> &str {
    let Some(path) = path.strip_prefix('/') else {
        return path;
    };
    path.strip_prefix("v1/").unwrap_or(path)
}

fn parse_global_crab_object(path: &str) -> Option<CacheObjectPath<'_>> {
    let after_global = path.strip_prefix(".crab/")?;
    let mut parts = after_global.split('/');
    let type_str = parts.next()?;
    let partition = parts.next()?;
    let hash = parts.next()?;
    if parts.next().is_some()
        || !is_hash_hex(hash)
        || partition != hash.get(..2)?
        || !partition
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let kind = match type_str {
        "xorbs" => CacheObjectKind::Xorb,
        "shards" => CacheObjectKind::Shard,
        _ => return None,
    };
    Some(CacheObjectPath {
        repo_path: ".crab",
        kind,
        identity: Cow::Borrowed(hash),
    })
}

fn parse_pack_object(path: &str) -> Option<CacheObjectPath<'_>> {
    let packs_pos = path.find("/packs/")?;
    let repo_path = &path[..packs_pos];
    let filename = &path[packs_pos + 7..];
    if repo_path.is_empty() || filename.is_empty() || filename.contains('/') {
        return None;
    }

    let (kind, identity) = if let Some(sha) = filename.strip_suffix(".pack") {
        (CacheObjectKind::Pack, sha)
    } else {
        let sha = filename.strip_suffix(".idx")?;
        (CacheObjectKind::PackIndex, sha)
    };
    if identity.is_empty() {
        return None;
    }

    Some(CacheObjectPath {
        repo_path,
        kind,
        identity: Cow::Borrowed(identity),
    })
}

fn parse_versioned_metadb_object(path: &str) -> Option<CacheObjectPath<'_>> {
    for db_name in ["file_index_db", "chunk_index_db"] {
        let db = format!("/{db_name}/");
        let Some(db_start) = path.find(&db) else {
            continue;
        };
        let repo_path = &path[..db_start];
        let suffix = &path[db_start + db.len()..];
        if repo_path.is_empty() || !is_versioned_metadb_suffix(suffix) {
            return None;
        }
        return Some(CacheObjectPath {
            repo_path,
            kind: CacheObjectKind::Metadata,
            identity: Cow::Owned(blake3::hash(path.as_bytes()).to_hex().to_string()),
        });
    }
    None
}

fn is_versioned_metadb_suffix(suffix: &str) -> bool {
    (suffix.starts_with("wal/") && has_extension(suffix, "sst"))
        || (suffix.starts_with("compacted/") && has_extension(suffix, "sst"))
        || (suffix.starts_with("manifest/") && has_extension(suffix, "manifest"))
        || (suffix.starts_with("compactions/") && has_extension(suffix, "compactions"))
}

fn has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(extension))
}

fn is_hash_hex(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCHITECTURE_DOC: &str =
        include_str!("../../../packages/web/content/docs/cli/cache-service/architecture.mdx");

    struct RouteContractCase {
        name: &'static str,
        path: String,
        class: PathClass,
        parsed: Option<ParsedRouteContract>,
        docs_token: &'static str,
    }

    struct ParsedRouteContract {
        repo_path: &'static str,
        kind: CacheObjectKind,
        identity: String,
    }

    fn hex_hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn route_contract_cases() -> Vec<RouteContractCase> {
        let xorb_hash = hex_hash('a');
        let shard_hash = hex_hash('b');
        let generated_hash = hex_hash('c');
        vec![
            RouteContractCase {
                name: "global xorb",
                path: format!("/v1/.crab/xorbs/aa/{xorb_hash}"),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: ".crab",
                    kind: CacheObjectKind::Xorb,
                    identity: xorb_hash,
                }),
                docs_token: ".crab/xorbs/{first-two-hex}/{hash}",
            },
            RouteContractCase {
                name: "global shard",
                path: format!("/v1/.crab/shards/bb/{shard_hash}"),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: ".crab",
                    kind: CacheObjectKind::Shard,
                    identity: shard_hash,
                }),
                docs_token: ".crab/shards/{first-two-hex}/{hash}",
            },
            RouteContractCase {
                name: "git pack",
                path: "/v1/org/repo/packs/pack-abc.pack".to_string(),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: "org/repo",
                    kind: CacheObjectKind::Pack,
                    identity: "pack-abc".to_string(),
                }),
                docs_token: "{repo}/packs/pack-{id}.pack",
            },
            RouteContractCase {
                name: "git pack index",
                path: "/v1/org/repo/packs/pack-abc.idx".to_string(),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: "org/repo",
                    kind: CacheObjectKind::PackIndex,
                    identity: "pack-abc".to_string(),
                }),
                docs_token: "{repo}/packs/pack-{id}.idx",
            },
            RouteContractCase {
                name: "generated pack artifact",
                path: format!("/v1/org/repo/generated-packs/v1/artifacts/cc/{generated_hash}.pack"),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: "org/repo",
                    kind: CacheObjectKind::GeneratedPack,
                    identity: blake3::hash(
                        format!("org/repo/generated-packs/v1/artifacts/cc/{generated_hash}.pack")
                            .as_bytes(),
                    )
                    .to_hex()
                    .to_string(),
                }),
                docs_token: "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
            },
            RouteContractCase {
                name: "generated pack request",
                path: format!("/v1/org/repo/generated-packs/v1/requests/cc/{generated_hash}.json"),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: "org/repo",
                    kind: CacheObjectKind::GeneratedPack,
                    identity: blake3::hash(
                        format!("org/repo/generated-packs/v1/requests/cc/{generated_hash}.json")
                            .as_bytes(),
                    )
                    .to_hex()
                    .to_string(),
                }),
                docs_token: "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
            },
            RouteContractCase {
                name: "file index db sst",
                path: "/v1/org/repo/file_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst"
                    .to_string(),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: "org/repo",
                    kind: CacheObjectKind::Metadata,
                    identity: blake3::hash(
                        b"org/repo/file_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst",
                    )
                    .to_hex()
                    .to_string(),
                }),
                docs_token: "{repo}/file_index_db/compacted/*.sst",
            },
            RouteContractCase {
                name: "chunk index db manifest",
                path: "/v1/.crab/chunk_index_db/manifest/00000000000000000008.manifest".to_string(),
                class: PathClass::Immutable,
                parsed: Some(ParsedRouteContract {
                    repo_path: ".crab",
                    kind: CacheObjectKind::Metadata,
                    identity: blake3::hash(
                        b".crab/chunk_index_db/manifest/00000000000000000008.manifest",
                    )
                    .to_hex()
                    .to_string(),
                }),
                docs_token: ".crab/chunk_index_db/manifest/*.manifest",
            },
            RouteContractCase {
                name: "git ref",
                path: "/v1/org/repo/refs/heads/main".to_string(),
                class: PathClass::Mutable,
                parsed: None,
                docs_token: "{repo}/refs/heads/*",
            },
            RouteContractCase {
                name: "head",
                path: "/v1/org/repo/HEAD".to_string(),
                class: PathClass::Mutable,
                parsed: None,
                docs_token: "{repo}/HEAD",
            },
            RouteContractCase {
                name: "lock",
                path: "/v1/org/repo/locks/push-main".to_string(),
                class: PathClass::Mutable,
                parsed: None,
                docs_token: "{repo}/locks/*",
            },
            RouteContractCase {
                name: "pack metadata",
                path: "/v1/org/repo/packs/pack-abc.meta".to_string(),
                class: PathClass::Mutable,
                parsed: None,
                docs_token: "{repo}/packs/pack-{id}.meta",
            },
            RouteContractCase {
                name: "metadata current pointer",
                path: "/v1/.crab/chunk_index_db/manifest/current".to_string(),
                class: PathClass::Mutable,
                parsed: None,
                docs_token: ".crab/chunk_index_db/manifest/current",
            },
        ]
    }

    #[test]
    fn route_contract_matrix_matches_classifier_and_parser() {
        for case in route_contract_cases() {
            assert_eq!(classify_path(&case.path), case.class, "{}", case.name);
            match case.parsed {
                Some(expected) => {
                    let parsed = parse_cache_object_path(&case.path)
                        .unwrap_or_else(|| panic!("{} should parse as cache object", case.name));
                    assert_eq!(parsed.repo_path, expected.repo_path, "{}", case.name);
                    assert_eq!(parsed.kind, expected.kind, "{}", case.name);
                    assert_eq!(parsed.identity.as_ref(), expected.identity, "{}", case.name);
                }
                None => {
                    assert!(
                        parse_cache_object_path(&case.path).is_none(),
                        "{} should not parse as cache object",
                        case.name
                    );
                }
            }
        }
    }

    #[test]
    fn route_contract_matrix_is_documented() {
        for case in route_contract_cases() {
            assert!(
                ARCHITECTURE_DOC.contains(case.docs_token),
                "architecture docs should mention {} token `{}`",
                case.name,
                case.docs_token
            );
        }
    }

    #[test]
    fn route_contract_advertises_current_taxonomy() {
        let contract = cache_route_contract();

        assert_eq!(contract.schema, CACHE_ROUTE_CONTRACT_SCHEMA);
        assert_eq!(contract.transport_prefix, "/v1/");
        for pattern in [
            ".crab/xorbs/{first-two-hex}/{hash}",
            ".crab/shards/{first-two-hex}/{hash}",
            "{repo}/packs/pack-{id}.pack",
            "{repo}/packs/pack-{id}.idx",
            "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
            "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
            "{repo}/file_index_db/manifest/*.manifest",
            ".crab/chunk_index_db/manifest/*.manifest",
        ] {
            assert!(
                contract
                    .immutable
                    .iter()
                    .any(|route| route.pattern == pattern),
                "immutable route contract should contain {pattern}"
            );
        }
        for pattern in [
            "{repo}/refs/heads/*",
            "{repo}/HEAD",
            ".crab/chunk_index_db/manifest/current",
        ] {
            assert!(
                contract
                    .mutable
                    .iter()
                    .any(|route| route.pattern == pattern),
                "mutable route contract should contain {pattern}"
            );
        }
        assert!(
            contract.mutable.iter().all(|route| route.kind != "retired"),
            "route contract should not advertise retired routes"
        );
        assert!(cache_route_contract_matches_current(&contract));
    }

    #[test]
    fn route_contract_matching_is_strict() {
        let mut contract = cache_route_contract();
        contract.immutable.pop();

        assert!(!cache_route_contract_matches_current(&contract));
    }

    #[test]
    fn local_cache_key_mapping_covers_hash_addressed_objects_only() {
        let xorb_hash = MerkleHash::from([9u64, 10, 11, 12]);
        let shard_hash = MerkleHash::from([1u64, 2, 3, 4]);

        let xorb_key = cache_key_for_path(&format!(
            ".crab/xorbs/{}/{}",
            &xorb_hash.hex()[..2],
            xorb_hash.hex()
        ))
        .expect("xorb path should map to local cache key");
        assert!(matches!(xorb_key, CacheKey::Xorb(hash) if hash == xorb_hash));

        let shard_key = cache_key_for_path(&format!(
            "/v1/.crab/shards/{}/{}",
            &shard_hash.hex()[..2],
            shard_hash.hex()
        ))
        .expect("shard path should map to local cache key");
        assert!(matches!(shard_key, CacheKey::Shard(hash) if hash == shard_hash));

        assert!(cache_key_for_path("org/repo/packs/pack-abc.pack").is_none());
        assert!(
            cache_key_for_path("org/repo/file_index_db/manifest/00000000000000000009.manifest")
                .is_none()
        );
        assert!(cache_key_for_path("org/repo/refs/heads/main").is_none());
        assert!(cache_key_for_path(".crab/shards/not-a-valid-hex").is_none());
        assert!(cache_key_for_path("repo/xet/xorbs/aaaaaaaa").is_none());
    }

    #[test]
    fn mutable_repo_paths_cover_control_plane_shapes() {
        for (path, expected) in [
            ("/v1/org/team/repo/refs/heads/main", "org/team/repo"),
            ("org/team/repo/locks/refs/heads/main/lock", "org/team/repo"),
            ("org/team/repo/locks/internal/repack/lock", "org/team/repo"),
            ("org/team/repo/packs/pack-abc.meta", "org/team/repo"),
            (
                "org/team/repo/manifests/pack-list-cafebabe",
                "org/team/repo",
            ),
            (
                "org/team/repo/file_index_db/manifest/current",
                "org/team/repo",
            ),
            (".crab/chunk_index_db/manifest/current", ".crab"),
            ("org/team/repo/HEAD", "org/team/repo"),
            ("org/team/repo/manifest", "org/team/repo"),
            ("org/team/repo/pack-list", "org/team/repo"),
            ("org/team/repo/shard-list", "org/team/repo"),
            (".crab/ref-registry", ".crab"),
            (".crab/ref-registry/records/ab/repo.json", ".crab"),
            (".crab/ref-registry/shard-roots/repo/0123.json", ".crab"),
        ] {
            assert_eq!(parse_mutable_repo_path(path), Some(expected), "{path}");
        }
    }

    #[test]
    fn mutable_repo_paths_reject_unscoped_and_immutable_objects() {
        let hash = hex_hash('9');
        assert_eq!(parse_mutable_repo_path("opaque-control-object"), None);
        assert_eq!(
            parse_mutable_repo_path(&format!("org/repo/packs/{hash}.pack")),
            None,
        );
        assert_eq!(
            parse_mutable_repo_path(&format!(".crab/xorbs/{}/{hash}", &hash[..2])),
            None,
        );
    }

    #[test]
    fn pack_path_is_immutable() {
        assert_eq!(
            classify_path("/v1/bucket/repo/packs/deadbeef.pack"),
            PathClass::Immutable,
        );
    }

    #[test]
    fn ref_path_is_mutable() {
        assert_eq!(
            classify_path("/v1/bucket/repo/refs/heads/main"),
            PathClass::Mutable,
        );
    }

    #[test]
    fn head_is_mutable() {
        assert_eq!(classify_path("/v1/bucket/repo/HEAD"), PathClass::Mutable,);
    }

    #[test]
    fn manifest_path_is_mutable() {
        assert_eq!(
            classify_path("/v1/bucket/repo/pack-list"),
            PathClass::Mutable,
        );
    }

    #[test]
    fn lock_path_is_mutable() {
        assert_eq!(
            classify_path("/v1/bucket/repo/locks/my-lock"),
            PathClass::Mutable,
        );
    }

    #[test]
    fn config_path_is_mutable() {
        assert_eq!(classify_path("/v1/bucket/repo/config"), PathClass::Mutable,);
    }

    #[test]
    fn canonical_crab_xorb_path_is_immutable() {
        let hash = hex_hash('a');
        assert_eq!(
            classify_path(&format!("/v1/.crab/xorbs/{}/{hash}", &hash[..2])),
            PathClass::Immutable,
        );
    }

    #[test]
    fn canonical_crab_shard_path_is_immutable() {
        let hash = hex_hash('b');
        assert_eq!(
            classify_path(&format!("/v1/.crab/shards/{}/{hash}", &hash[..2])),
            PathClass::Immutable,
        );
    }

    #[test]
    fn embedded_crab_content_paths_are_mutable() {
        let xorb_hash = hex_hash('c');
        let shard_hash = hex_hash('d');
        assert_eq!(
            classify_path(&format!(
                "/v1/bucket/.crab/xorbs/{}/{xorb_hash}",
                &xorb_hash[..2]
            )),
            PathClass::Mutable,
        );
        assert_eq!(
            classify_path(&format!(
                "/v1/bucket/.crab/shards/{}/{shard_hash}",
                &shard_hash[..2]
            )),
            PathClass::Mutable,
        );
    }

    #[test]
    fn malformed_crab_content_paths_are_mutable() {
        assert_eq!(
            classify_path("/v1/.crab/xorbs/not-a-valid-hash"),
            PathClass::Mutable,
        );
        assert_eq!(classify_path("/v1/.crab/shards/abc123"), PathClass::Mutable,);
        let hash = hex_hash('a');
        assert_eq!(
            classify_path(&format!("/v1/.crab/xorbs/{hash}")),
            PathClass::Mutable,
        );
        assert_eq!(
            classify_path(&format!("/v1/.crab/xorbs/ff/{hash}")),
            PathClass::Mutable,
        );
    }

    #[test]
    fn slatedb_file_index_sst_is_immutable() {
        assert_eq!(
            classify_path("/v1/org/repo/file_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst"),
            PathClass::Immutable,
        );
    }

    #[test]
    fn slatedb_file_index_manifest_version_is_immutable() {
        assert_eq!(
            classify_path("/v1/org/repo/file_index_db/manifest/00000000000000000008.manifest"),
            PathClass::Immutable,
        );
    }

    #[test]
    fn slatedb_file_index_wal_sst_is_immutable() {
        assert_eq!(
            classify_path("/v1/org/repo/file_index_db/wal/00000000000000000001.sst"),
            PathClass::Immutable,
        );
    }

    #[test]
    fn slatedb_file_index_compactions_version_is_immutable() {
        assert_eq!(
            classify_path(
                "/v1/org/repo/file_index_db/compactions/00000000000000000001.compactions"
            ),
            PathClass::Immutable,
        );
    }

    #[test]
    fn slatedb_chunk_index_objects_are_immutable() {
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst"),
            PathClass::Immutable,
        );
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/wal/00000000000000000001.sst"),
            PathClass::Immutable,
        );
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/manifest/00000000000000000008.manifest"),
            PathClass::Immutable,
        );
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/compactions/00000000000000000001.compactions"),
            PathClass::Immutable,
        );
    }

    #[test]
    fn slatedb_metadata_directories_are_mutable() {
        assert_eq!(
            classify_path("/v1/org/repo/file_index_db/manifest/"),
            PathClass::Mutable,
        );
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/wal/"),
            PathClass::Mutable,
        );
        assert_eq!(
            classify_path("/v1/.crab/chunk_index_db/manifest/current"),
            PathClass::Mutable,
        );
    }

    #[test]
    fn bare_crab_paths_are_immutable() {
        let xorb_hash = hex_hash('e');
        let shard_hash = hex_hash('f');
        assert_eq!(
            classify_path(&format!(".crab/xorbs/{}/{xorb_hash}", &xorb_hash[..2])),
            PathClass::Immutable,
        );
        assert_eq!(
            classify_path(&format!(".crab/shards/{}/{shard_hash}", &shard_hash[..2])),
            PathClass::Immutable,
        );
    }

    #[test]
    fn bare_v1_prefix_is_not_a_cache_service_route() {
        let hash = hex_hash('1');
        assert_eq!(
            classify_path(&format!("v1/.crab/xorbs/{hash}")),
            PathClass::Mutable,
        );
    }

    #[test]
    fn crab_ref_registry_is_mutable() {
        assert_eq!(
            classify_path("/v1/bucket/.crab/ref-registry"),
            PathClass::Mutable,
        );
    }
}
