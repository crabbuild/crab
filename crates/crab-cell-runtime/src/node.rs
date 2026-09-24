//! Node advertisements, capacity, the node directory, leases, and the durability log.
pub mod durability;
pub mod lease;
pub mod log;
pub mod log_recovery;
pub mod log_shipper;
pub mod log_state;
pub mod log_transport;

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError, map_object_store_error};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::fleet::placement::{PlacementObservation, PlacementPlanner, PlacementScore};
use crate::identity::NodeId;
use crate::identity::{Digest, SessionId};
use crate::node::log::NodeLogRotationBarrier;
use crate::node::log_state::{NodeLogPhase, NodeLogStatus, NodeRecoveryClaim};
use crate::{Error, Result};

const MAX_NODE_BYTES: u64 = 64 * 1024;
const MAX_ENDPOINT_BYTES: usize = 512;
const MAX_FAILURE_DOMAIN_BYTES: usize = 253;
const MAX_MODULES: usize = 128;
const MAX_PEER_VERSIONS: usize = 16;
const MAX_ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const STALE_ADVERTISEMENT_RETENTION_MS: i64 = MAX_CLOCK_SKEW_MS + MAX_ADVERTISEMENT_LIFETIME_MS;
const MAX_STALE_COLLECTION_ITEMS: usize = 1_024;
const MAX_LIVE_NODE_RECORDS: usize = 10_000;
const NODE_DIRECTORY_READ_CONCURRENCY: usize = 32;
const RECOVERY_SCAN_CACHE_TTL_MS: i64 = 1_000;
const SIGNING_DOMAIN: &[u8] = b"crab.node.v1\0";
const PLACEMENT_SIGNING_DOMAIN: &[u8] = b"crab.node-placement.v1\0";
const NODE_LOG_SELECTION_DOMAIN: &[u8] = b"crab.node-log.member.v1\0";
const RECOVERY_CANDIDATE_ROTATION_DOMAIN: &[u8] = b"crab.node-recovery-candidate.v1\0";
const PLACEMENT_SCHEMA_VERSION: u32 = 2;

/// Current private follower-log wire and persistence protocol.
pub const NODE_LOG_PROTOCOL_VERSION: u32 = 1;
pub use advertisement::{
    FencedNodeSession, NodeAdvertisement, NodeTakeoverProof, SealedNodeLog,
    VersionedNodeAdvertisement,
};
pub use capacity::{NodeCapacity, NodeFailureDomain, NodePlacementCapacity};
pub use directory::NodeDirectory;
mod advertisement;
mod capacity;
mod directory;

#[cfg(test)]
mod tests;
