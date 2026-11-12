use serde::Serialize;

use super::Identity;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Crab,
    Lfs,
    Git,
}

#[derive(Serialize)]
pub(crate) struct FileVersion {
    kind: Kind,
    hash: String,
    size: u64,
    mode: Option<String>,
}

impl FileVersion {
    pub(super) fn pointer(identity: Identity, size: u64) -> Self {
        let (kind, hash) = match identity {
            Identity::Blake3(hash) => (Kind::Crab, hash),
            Identity::Sha256(hash) => (Kind::Lfs, hash),
        };
        Self {
            kind,
            hash: crab_git::lfs_pointer::hex_encode(&hash),
            size,
            mode: None,
        }
    }

    pub(super) fn git(hash: &str, size: u64, mode: &str) -> Self {
        Self {
            kind: Kind::Git,
            hash: hash.into(),
            size,
            mode: Some(mode.into()),
        }
    }
}
