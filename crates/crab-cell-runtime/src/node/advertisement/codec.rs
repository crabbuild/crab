//! Strict JSON wire payloads for node advertisements and tombstones.

use super::*;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawUnsignedIdentity {
    pub(in crate::node) node: String,
    pub(in crate::node) session: String,
    pub(in crate::node) endpoint: String,
    pub(in crate::node) fleet: String,
    pub(in crate::node) certificate: String,
    pub(in crate::node) image: String,
    pub(in crate::node) release: String,
    pub(in crate::node) public_key: String,
    pub(in crate::node) module_digests: Vec<String>,
    pub(in crate::node) peer_versions: Vec<u32>,
    pub(in crate::node) failure_domain: RawFailureDomain,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawNodeSigningPayload {
    pub(in crate::node) identity: RawUnsignedIdentity,
    pub(in crate::node) capacity: RawCapacity,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawFailureDomain {
    pub(in crate::node) zone: Option<String>,
    pub(in crate::node) host: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawNodeTombstoneEnvelope {
    pub(in crate::node) tombstone: RawNodeTombstone,
}

impl From<&NodeTombstone> for RawNodeTombstoneEnvelope {
    fn from(value: &NodeTombstone) -> Self {
        Self {
            tombstone: RawNodeTombstone {
                version: 1,
                session: encode_hex(value.session.as_bytes()),
                node: encode_hex(value.node.as_bytes()),
                expires_at_ms: value.expires_at_ms.to_string(),
                retired_at_ms: value.retired_at_ms.to_string(),
                claimant: value
                    .claimant
                    .map(|claimant| encode_hex(claimant.as_bytes())),
                claim_generation: value.claim_generation.to_string(),
                claim_expires_at_ms: value.claim_expires_at_ms.map(|value| value.to_string()),
                log: value.log.as_ref().map(encode_log),
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawNodeTombstone {
    pub(in crate::node) version: u8,
    pub(in crate::node) session: String,
    pub(in crate::node) node: String,
    pub(in crate::node) expires_at_ms: String,
    pub(in crate::node) retired_at_ms: String,
    pub(in crate::node) claimant: Option<String>,
    pub(in crate::node) claim_generation: String,
    pub(in crate::node) claim_expires_at_ms: Option<String>,
    pub(in crate::node) log: Option<RawNodeLog>,
}

impl From<&NodeAdvertisement> for RawUnsignedIdentity {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            node: encode_hex(value.node.as_bytes()),
            session: encode_hex(value.session.as_bytes()),
            endpoint: value.endpoint.clone(),
            fleet: encode_hex(value.fleet.as_bytes()),
            certificate: encode_hex(value.certificate.as_bytes()),
            image: encode_hex(value.image.as_bytes()),
            release: encode_hex(value.release.as_bytes()),
            public_key: encode_hex(&value.public_key),
            module_digests: value
                .module_digests
                .iter()
                .map(|digest| encode_hex(digest.as_bytes()))
                .collect(),
            peer_versions: value.peer_versions.clone(),
            failure_domain: RawFailureDomain {
                zone: value.failure_domain.zone.clone(),
                host: value.failure_domain.host.clone(),
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawAdvertisement {
    pub(in crate::node) version: u8,
    pub(in crate::node) identity: RawIdentity,
    pub(in crate::node) lease: RawLease,
    pub(in crate::node) log: Option<RawNodeLog>,
    pub(in crate::node) capacity: RawCapacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) placement: Option<RawPlacementCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) placement_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) placement_signature: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawIdentity {
    #[serde(flatten)]
    pub(in crate::node) unsigned: RawUnsignedIdentity,
    pub(in crate::node) signature: String,
}

impl From<&NodeAdvertisement> for RawAdvertisement {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            version: 1,
            identity: RawIdentity {
                unsigned: RawUnsignedIdentity::from(value),
                signature: encode_hex(&value.signature),
            },
            lease: RawLease {
                generation: value.generation.to_string(),
                progress: value.progress.to_string(),
                issued_at_ms: value.issued_at_ms.to_string(),
                expires_at_ms: value.expires_at_ms.to_string(),
            },
            log: value.log.as_ref().map(encode_log),
            capacity: RawCapacity::from(value.capacity),
            placement: value.placement.map(|placement| RawPlacementCapacity {
                memory_capacity_bytes: placement.memory_capacity_bytes.to_string(),
                disk_capacity_bytes: placement.disk_capacity_bytes.to_string(),
                active_cells: placement.active_cells,
                max_active_cells: placement.max_active_cells,
                running_jobs: placement.running_jobs,
                job_capacity: placement.job_capacity,
                publication_backlog: (value.placement_version >= PLACEMENT_SCHEMA_VERSION)
                    .then_some(placement.publication_backlog),
                hydration_backlog: (value.placement_version >= PLACEMENT_SCHEMA_VERSION)
                    .then_some(placement.hydration_backlog),
                primitive_backlog: (value.placement_version >= PLACEMENT_SCHEMA_VERSION)
                    .then_some(placement.primitive_backlog),
            }),
            placement_version: (value.placement_version != 0).then_some(value.placement_version),
            placement_signature: value
                .placement_signature
                .iter()
                .any(|byte| *byte != 0)
                .then(|| encode_hex(&value.placement_signature)),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawLease {
    pub(in crate::node) generation: String,
    pub(in crate::node) progress: String,
    pub(in crate::node) issued_at_ms: String,
    pub(in crate::node) expires_at_ms: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawCapacity {
    pub(in crate::node) free_memory_bytes: String,
    pub(in crate::node) free_disk_bytes: String,
    pub(in crate::node) follower_free_bytes: String,
    pub(in crate::node) follower_retained_bytes: String,
    pub(in crate::node) job_credits: u32,
    pub(in crate::node) log_protocol: u32,
}

impl From<NodeCapacity> for RawCapacity {
    fn from(value: NodeCapacity) -> Self {
        Self {
            free_memory_bytes: value.free_memory_bytes.to_string(),
            free_disk_bytes: value.free_disk_bytes.to_string(),
            follower_free_bytes: value.follower_free_bytes.to_string(),
            follower_retained_bytes: value.follower_retained_bytes.to_string(),
            job_credits: value.job_credits,
            log_protocol: value.log_protocol,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawPlacementCapacity {
    pub(in crate::node) memory_capacity_bytes: String,
    pub(in crate::node) disk_capacity_bytes: String,
    pub(in crate::node) active_cells: u32,
    pub(in crate::node) max_active_cells: u32,
    pub(in crate::node) running_jobs: u32,
    pub(in crate::node) job_capacity: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) publication_backlog: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) hydration_backlog: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::node) primitive_backlog: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawNodeLog {
    pub(in crate::node) state: RawNodeLogPhase,
    pub(in crate::node) epoch: String,
    pub(in crate::node) members: Vec<String>,
    pub(in crate::node) active: bool,
    pub(in crate::node) tiered_through: String,
    pub(in crate::node) recovery: Option<RawNodeRecoveryClaim>,
    pub(in crate::node) recovery_manifest: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::node) enum RawNodeLogPhase {
    Open,
    Recovering,
    Sealed,
    Retired,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::node) struct RawNodeRecoveryClaim {
    pub(in crate::node) claimant: String,
    pub(in crate::node) generation: String,
    pub(in crate::node) expires_at_ms: String,
}

impl TryFrom<RawAdvertisement> for NodeAdvertisement {
    type Error = Error;

    fn try_from(value: RawAdvertisement) -> Result<Self> {
        if value.version != 1 {
            return Err(Error::Node("unsupported advertisement version"));
        }
        let raw = value.identity.unsigned;
        let session = SessionId::from_bytes(decode_hex(&raw.session)?);
        let node = NodeId::from_bytes(decode_hex(&raw.node)?);
        Ok(Self {
            node,
            session,
            endpoint: raw.endpoint,
            fleet: Digest::from_bytes(decode_hex(&raw.fleet)?),
            certificate: Digest::from_bytes(decode_hex(&raw.certificate)?),
            image: Digest::from_bytes(decode_hex(&raw.image)?),
            release: Digest::from_bytes(decode_hex(&raw.release)?),
            public_key: decode_hex(&raw.public_key)?,
            generation: canonical_u64(&value.lease.generation)?,
            progress: canonical_u64(&value.lease.progress)?,
            issued_at_ms: canonical_i64(&value.lease.issued_at_ms)?,
            expires_at_ms: canonical_i64(&value.lease.expires_at_ms)?,
            module_digests: raw
                .module_digests
                .iter()
                .map(|value| decode_hex(value).map(Digest::from_bytes))
                .collect::<Result<Vec<_>>>()?,
            peer_versions: raw.peer_versions,
            failure_domain: NodeFailureDomain::new(
                raw.failure_domain.zone,
                raw.failure_domain.host,
            )?,
            capacity: NodeCapacity {
                free_memory_bytes: canonical_u64(&value.capacity.free_memory_bytes)?,
                free_disk_bytes: canonical_u64(&value.capacity.free_disk_bytes)?,
                follower_free_bytes: canonical_u64(&value.capacity.follower_free_bytes)?,
                follower_retained_bytes: canonical_u64(&value.capacity.follower_retained_bytes)?,
                job_credits: value.capacity.job_credits,
                log_protocol: value.capacity.log_protocol,
            },
            placement: value
                .placement
                .map(|placement| {
                    NodePlacementCapacity {
                        memory_capacity_bytes: canonical_u64(&placement.memory_capacity_bytes)?,
                        disk_capacity_bytes: canonical_u64(&placement.disk_capacity_bytes)?,
                        active_cells: placement.active_cells,
                        max_active_cells: placement.max_active_cells,
                        running_jobs: placement.running_jobs,
                        job_capacity: placement.job_capacity,
                        publication_backlog: placement.publication_backlog.unwrap_or(0),
                        hydration_backlog: placement.hydration_backlog.unwrap_or(0),
                        primitive_backlog: placement.primitive_backlog.unwrap_or(0),
                    }
                    .validated()
                })
                .transpose()?,
            log: value.log.map(|log| decode_log(node, log)).transpose()?,
            signature: decode_hex(&value.identity.signature)?,
            placement_version: value.placement_version.unwrap_or(0),
            placement_signature: value
                .placement_signature
                .map(|signature| decode_hex(&signature))
                .transpose()?
                .unwrap_or([0; 64]),
        })
    }
}
