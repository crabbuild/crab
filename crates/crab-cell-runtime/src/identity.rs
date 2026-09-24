//! Cell, tenant, application, namespace, session, node, and request identities.
use std::fmt;

use crate::{Error, Result};

macro_rules! fixed_id {
    ($name:ident, $len:literal) => {
        #[doc = concat!("Opaque fixed-width ", stringify!($name), " identity bytes.")]
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            /// Wraps the fixed-width identity bytes without validation.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Returns the raw identity bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl TryFrom<&[u8]> for $name {
            type Error = Error;

            fn try_from(value: &[u8]) -> Result<Self> {
                let bytes = value
                    .try_into()
                    .map_err(|_| Error::Identity(concat!(stringify!($name), " length")))?;
                Ok(Self(bytes))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(stringify!($name))?;
                formatter.write_str("(")?;
                write_hex(formatter, &self.0)?;
                formatter.write_str(")")
            }
        }
    };
}

fixed_id!(TenantId, 16);
fixed_id!(ApplicationId, 16);
fixed_id!(NamespaceId, 16);
fixed_id!(NodeId, 16);
fixed_id!(SessionId, 16);
fixed_id!(IncarnationId, 16);
fixed_id!(RequestId, 16);
fixed_id!(Digest, 32);
fixed_id!(CellId, 32);

/// A resolved, authorized Cell location before its content hash is derived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellTarget {
    tenant: TenantId,
    application: ApplicationId,
    namespace: NamespaceId,
    partition: Vec<u8>,
}

impl CellTarget {
    /// Constructs a target from stable IDs and a bounded partition key.
    pub fn new(
        tenant: TenantId,
        application: ApplicationId,
        namespace: NamespaceId,
        partition: &[u8],
    ) -> Result<Self> {
        if partition.len() > 1024 {
            return Err(Error::Identity("partition exceeds 1024 bytes"));
        }
        Ok(Self {
            tenant,
            application,
            namespace,
            partition: partition.to_vec(),
        })
    }

    /// Derives the Cell ID from the target's tenant, application, namespace,
    /// and partition.
    #[must_use]
    pub fn cell_id(&self) -> CellId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab.cell.v1\0");
        hasher.update(self.tenant.as_bytes());
        hasher.update(self.application.as_bytes());
        hasher.update(self.namespace.as_bytes());
        hasher.update(&(self.partition.len() as u32).to_be_bytes());
        hasher.update(&self.partition);
        CellId::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Returns the tenant this target belongs to.
    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Returns the application this target belongs to.
    #[must_use]
    pub const fn application(&self) -> ApplicationId {
        self.application
    }

    /// Returns the namespace that owns this target's Cell.
    #[must_use]
    pub const fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    /// Returns the partition key that selected the Cell within the namespace.
    #[must_use]
    pub fn partition(&self) -> &[u8] {
        &self.partition
    }
}

/// Maps one bounded scope to a fixed power-of-two shard count.
pub fn shard_for_scope(namespace: NamespaceId, scope: &[u8], shard_count: u32) -> Result<u32> {
    if scope.len() > 1024 {
        return Err(Error::Identity("scope exceeds 1024 bytes"));
    }
    if !(1..=4096).contains(&shard_count) || !shard_count.is_power_of_two() {
        return Err(Error::Identity(
            "shard count must be a power of two in 1..=4096",
        ));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.shard.v1\0");
    hasher.update(namespace.as_bytes());
    hasher.update(&(scope.len() as u32).to_be_bytes());
    hasher.update(scope);
    let digest = hasher.finalize();
    let prefix: [u8; 8] = digest.as_bytes()[..8]
        .try_into()
        .map_err(|_| Error::Identity("BLAKE3 prefix"))?;
    Ok((u64::from_be_bytes(prefix) % u64::from(shard_count)) as u32)
}

/// Encodes a primitive shard as the canonical Cell partition bytes.
#[must_use]
pub const fn partition_for_shard(shard: u32) -> [u8; 4] {
    shard.to_be_bytes()
}

pub(crate) fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(crate) fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(Error::Control("hex field length"));
    }
    let mut decoded = [0; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = nibble(pair[0]).ok_or(Error::Control("hex field must be lowercase"))?;
        let low = nibble(pair[1]).ok_or(Error::Control("hex field must be lowercase"))?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_identity_binds_every_field_and_partition_length() {
        let target = CellTarget::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
            NamespaceId::from_bytes([3; 16]),
            b"repository-42",
        )
        .unwrap();
        assert_eq!(
            encode_hex(target.cell_id().as_bytes()),
            "62da35085973dc3c2a46bba884a9099899c2039418e901c5836005e66c5fb089"
        );
        let other = CellTarget::new(
            target.tenant(),
            target.application(),
            target.namespace(),
            b"\0\0\0\x0drepository-42",
        )
        .unwrap();
        assert_ne!(target.cell_id(), other.cell_id());
    }

    #[test]
    fn shard_mapping_rejects_unbounded_or_mutable_topology() {
        let namespace = NamespaceId::from_bytes([4; 16]);
        assert_eq!(shard_for_scope(namespace, b"scope", 64).unwrap(), 61);
        assert!(shard_for_scope(namespace, b"scope", 3).is_err());
        assert!(shard_for_scope(namespace, &[0; 1025], 64).is_err());
        assert_eq!(partition_for_shard(54), [0, 0, 0, 54]);
    }
}
