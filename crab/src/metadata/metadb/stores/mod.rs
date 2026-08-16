//! Typed owning stores over the two SlateDB instances held by a
//! [`MetaDb`] session.
//!
//! Each store wraps the crab-native [`Db`] handle plus any local
//! cache tiers the store needs, and exposes a narrow point-operation
//! API. Stores are cheap-cloneable (`Arc`-backed) and carry no
//! lifetime parameters, so callers can stash them in long-lived
//! structs without borrowing the owning session.
//!
//! [`MetaDb`]: super::MetaDb
//! [`Db`]: super::db::Db

pub mod chunk_index;
pub mod file_index;

use crate::core::error::{CrabError, MetaDbError};
use crab_metadata::error::MetadataError;

pub use chunk_index::ChunkIndexStore;
pub use file_index::FileIndexStore;

// `XorbRef` is owned by `crab_xet::xorb::format`. Re-export it through
// this module so callers who import the stores get the entity type in
// the same namespace without reaching into the storage layer.
pub use crab_xet::xorb::format::XorbRef;

pub(crate) fn map_value_codec_error(
    error: MetadataError,
    db: &'static str,
    key: &[u8],
) -> CrabError {
    match error {
        MetadataError::CorruptObject { reason, .. } => MetaDbError::CorruptValue {
            db: String::from(db),
            key: hex_encode(key),
            reason,
        }
        .into(),
        other => other.into(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}
