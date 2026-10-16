//! Exact object admission and authenticated per-member counts for one run.
use super::{contract_error, corrupt};
use crate::error::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub use reader::CapsuleRunAdmissionReader;

const ADMISSION_MAGIC: &[u8; 8] = b"CRBADM01";
const ADMISSION_VERSION: u32 = 2;
const ADMISSION_TRAILER_BYTES: usize = 4;
const MAX_RUN_ADMISSION_BYTES: usize = 128 * 1024 * 1024;
const MAX_RUN_ADMISSION_ENTRIES: usize = 8_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmissionPages {
    member_count: u32,
    body_bytes: u64,
    body_hash: [u8; 32],
    objects: crate::pages::PageRef,
    counts: crate::pages::PageRef,
}

impl AdmissionPages {
    fn validate(&self) -> Result<()> {
        use crate::pages::Kind;
        if self.member_count == 0
            || self.body_bytes == 0
            || self.body_bytes > MAX_RUN_ADMISSION_BYTES as u64
            || self.objects.count as usize > MAX_RUN_ADMISSION_ENTRIES
        {
            return Err(corrupt("capsule run admission has an invalid count"));
        }
        self.objects.validate(Kind::RunAdmission, self.body_bytes)?;
        self.counts
            .validate(Kind::RunAdmissionCounts, self.body_bytes)?;
        if self.counts.count != self.member_count
            || self.counts.first != 0u32.to_be_bytes()
            || self.counts.last != (self.member_count - 1).to_be_bytes()
        {
            return Err(corrupt(
                "capsule run admission member counts have incomplete coverage",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_region(&self, length: u64, member_count: usize) -> Result<()> {
        self.validate()?;
        let encoded = self
            .body_bytes
            .checked_add(self.header().len() as u64)
            .and_then(|bytes| bytes.checked_add(ADMISSION_TRAILER_BYTES as u64));
        if self.member_count as usize != member_count || encoded != Some(length) {
            return Err(corrupt(
                "capsule run admission descriptor does not match its range or inventory",
            ));
        }
        Ok(())
    }

    fn header(&self) -> Vec<u8> {
        let mut bytes = ADMISSION_MAGIC.to_vec();
        bytes.extend_from_slice(&ADMISSION_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.member_count.to_be_bytes());
        bytes.extend_from_slice(&self.body_hash);
        crate::pages::encode_ref(&mut bytes, &self.objects);
        crate::pages::encode_ref(&mut bytes, &self.counts);
        bytes
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        use crate::pages::{Input, Kind};
        if bytes.len() < ADMISSION_TRAILER_BYTES || bytes.len() > MAX_RUN_ADMISSION_BYTES {
            return Err(corrupt("capsule run admission has an invalid size"));
        }
        let end = bytes.len() - ADMISSION_TRAILER_BYTES;
        let size = u32::from_be_bytes(
            bytes[end..]
                .try_into()
                .map_err(|_| corrupt("capsule run admission trailer is invalid"))?,
        ) as usize;
        let start = end
            .checked_sub(size)
            .ok_or_else(|| corrupt("capsule run admission header starts outside its body"))?;
        let mut header = Input::new(&bytes[start..end], "capsule run admission header");
        if header.take(ADMISSION_MAGIC.len())? != ADMISSION_MAGIC
            || u32::from_be_bytes(header.array()?) != ADMISSION_VERSION
        {
            return Err(corrupt(
                "capsule run admission magic or version is unsupported",
            ));
        }
        let member_count = u32::from_be_bytes(header.array()?);
        let body_hash = header.array()?;
        let objects = crate::pages::decode_ref(&mut header, Kind::RunAdmission)?;
        let counts = crate::pages::decode_ref(&mut header, Kind::RunAdmissionCounts)?;
        header.finish()?;
        let result = Self {
            member_count,
            body_bytes: start as u64,
            body_hash,
            objects,
            counts,
        };
        result.validate()?;
        Ok(result)
    }
}

/// Exact object-to-member admission for one immutable capsule run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRunAdmission {
    pub(super) members: BTreeMap<[u8; 20], Vec<u32>>,
}

impl CapsuleRunAdmission {
    pub(super) fn from_member_oids(
        member_oids: &[Vec<[u8; 20]>],
        member_count: usize,
    ) -> Result<Self> {
        if member_oids.len() != member_count {
            return Err(contract_error(
                "capsule run admission member count does not match the pack directory",
            ));
        }
        let mut members = BTreeMap::new();
        for (member_index, oids) in member_oids.iter().enumerate() {
            let member_index = u32::try_from(member_index)
                .map_err(|_| contract_error("capsule run admission member index overflowed"))?;
            let mut oids = oids.clone();
            oids.sort_unstable();
            oids.dedup();
            for oid in oids {
                members
                    .entry(oid)
                    .or_insert_with(Vec::new)
                    .push(member_index);
            }
        }
        Ok(Self { members })
    }

    pub(super) fn append(&mut self, newer: Self, member_offset: usize) -> Result<()> {
        let member_offset = u32::try_from(member_offset)
            .map_err(|_| contract_error("capsule run admission member offset overflowed"))?;
        for (oid, newer_members) in newer.members {
            let entry = self.members.entry(oid).or_default();
            for member in newer_members {
                entry.push(member.checked_add(member_offset).ok_or_else(|| {
                    contract_error("capsule run admission member index overflowed")
                })?);
            }
        }
        Ok(())
    }

    pub(super) fn encode(&self, member_count: usize) -> Result<Bytes> {
        use crate::pages::{Kind, Writer};
        if self.members.is_empty()
            || self.members.len() > MAX_RUN_ADMISSION_ENTRIES
            || member_count == 0
        {
            return Err(contract_error("capsule run admission has an invalid count"));
        }
        let member_count = u32::try_from(member_count)
            .map_err(|_| contract_error("capsule run admission member count overflowed"))?;
        let minimum_bytes = self
            .members
            .len()
            .checked_mul(28)
            .and_then(|bytes| {
                self.members.values().try_fold(bytes, |total, members| {
                    total.checked_add(members.len().checked_mul(4)?)
                })
            })
            .and_then(|bytes| bytes.checked_add((member_count as usize).checked_mul(16)?));
        if minimum_bytes.is_none_or(|bytes| bytes > MAX_RUN_ADMISSION_BYTES) {
            return Err(contract_error(
                "capsule run admission exceeds its byte bound",
            ));
        }
        let mut counts = vec![0u64; member_count as usize];
        let mut rows = Vec::with_capacity(self.members.len());
        for (oid, members) in &self.members {
            if members.is_empty() || members.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(contract_error(
                    "capsule run admission members are not canonical",
                ));
            }
            let mut value = Vec::new();
            crate::pages::put_u32(&mut value, members.len())?;
            for member in members {
                let count = counts.get_mut(*member as usize).ok_or_else(|| {
                    contract_error("capsule run admission references an absent member")
                })?;
                *count += 1;
                value.extend_from_slice(&member.to_le_bytes());
            }
            rows.push((oid.to_vec(), value));
        }
        let mut writer = Writer::default();
        let objects = writer.tree(Kind::RunAdmission, rows)?;
        let counts = writer.tree(
            Kind::RunAdmissionCounts,
            counts
                .into_iter()
                .enumerate()
                .map(|(index, count)| {
                    (
                        (index as u32).to_be_bytes().to_vec(),
                        count.to_le_bytes().to_vec(),
                    )
                })
                .collect(),
        )?;
        let pages = AdmissionPages {
            member_count,
            body_bytes: writer.bytes.len() as u64,
            body_hash: *blake3::hash(&writer.bytes).as_bytes(),
            objects,
            counts,
        };
        pages.validate()?;
        let header = pages.header();
        let header_len = u32::try_from(header.len())
            .map_err(|_| contract_error("capsule run admission header size overflows"))?;
        writer.bytes.extend_from_slice(&header);
        writer.bytes.extend_from_slice(&header_len.to_be_bytes());
        if writer.bytes.len() > MAX_RUN_ADMISSION_BYTES {
            return Err(contract_error(
                "capsule run admission exceeds its byte bound",
            ));
        }
        Ok(writer.bytes.into())
    }

    pub(super) fn decode(bytes: &[u8], expected_member_count: usize) -> Result<Self> {
        use crate::pages::Kind;
        let pages = AdmissionPages::decode(bytes)?;
        if pages.member_count as usize != expected_member_count {
            return Err(corrupt(
                "capsule run admission member count does not match the pack directory",
            ));
        }
        let body = &bytes[..pages.body_bytes as usize];
        if blake3::hash(body).as_bytes() != &pages.body_hash {
            return Err(corrupt(
                "capsule run admission page body hash does not match",
            ));
        }
        let mut audit = BTreeMap::new();
        let rows = crate::pages::scan(
            body,
            &pages.objects,
            Kind::RunAdmission,
            "capsule run admission",
            &mut audit,
        )?;
        let mut members = BTreeMap::new();
        for (oid, value) in rows {
            let list = decode_members(&value, expected_member_count)?;
            let oid = oid
                .try_into()
                .map_err(|_| corrupt("capsule run admission OID has an invalid length"))?;
            members.insert(oid, list);
        }
        crate::pages::scan(
            body,
            &pages.counts,
            Kind::RunAdmissionCounts,
            "capsule run admission",
            &mut audit,
        )?;
        let result = Self { members };
        if result.encode(expected_member_count)?.as_ref() != bytes {
            return Err(corrupt(
                "capsule run admission pages are not canonical or counts disagree",
            ));
        }
        Ok(result)
    }

    /// Return the member ordinals that contain one Git object.
    #[must_use]
    pub fn object_members(&self, oid: &[u8; 20]) -> Option<&[u32]> {
        self.members.get(oid).map(Vec::as_slice)
    }

    /// Return every admitted object and the run members that contain it.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8; 20], &[u32])> {
        self.members
            .iter()
            .map(|(oid, members)| (oid, members.as_slice()))
    }
}

fn decode_members(value: &[u8], member_count: usize) -> Result<Vec<u32>> {
    let mut input = crate::pages::Input::new(value, "capsule run admission members");
    let count = input.u32()? as usize;
    if count == 0 || count > member_count || input.remaining() / 4 != count {
        return Err(corrupt("capsule run admission has an invalid member list"));
    }
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        let member = input.u32()?;
        if member as usize >= member_count
            || list.last().is_some_and(|previous| *previous >= member)
        {
            return Err(corrupt("capsule run admission members are invalid"));
        }
        list.push(member);
    }
    input.finish()?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{Kind, Writer};

    #[test]
    fn admission_pages_preserve_aliases_and_empty_member_counts() {
        let inputs = [vec![[1; 20], [2; 20], [1; 20]], Vec::new(), vec![[1; 20]]];
        let admission = CapsuleRunAdmission::from_member_oids(&inputs, 3).unwrap();
        let bytes = admission.encode(3).unwrap();
        let pages = AdmissionPages::decode(&bytes).unwrap();
        let counts = crate::pages::scan(
            &bytes[..pages.body_bytes as usize],
            &pages.counts,
            Kind::RunAdmissionCounts,
            "test",
            &mut BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            counts
                .into_iter()
                .map(|(_, value)| u64::from_le_bytes(value.try_into().unwrap()))
                .collect::<Vec<_>>(),
            [2, 0, 1]
        );
        assert_eq!(CapsuleRunAdmission::decode(&bytes, 3).unwrap(), admission);
    }

    #[test]
    fn resigned_member_counts_cannot_disagree_with_object_admission() {
        let admission = CapsuleRunAdmission::from_member_oids(&[vec![[1; 20]]], 1).unwrap();
        let bytes = admission.encode(1).unwrap();
        let pages = AdmissionPages::decode(&bytes).unwrap();
        let objects = crate::pages::scan(
            &bytes[..pages.body_bytes as usize],
            &pages.objects,
            Kind::RunAdmission,
            "test",
            &mut BTreeMap::new(),
        )
        .unwrap();
        let mut writer = Writer::default();
        let objects = writer.tree(Kind::RunAdmission, objects).unwrap();
        let counts = writer
            .tree(
                Kind::RunAdmissionCounts,
                vec![(0u32.to_be_bytes().to_vec(), 2u64.to_le_bytes().to_vec())],
            )
            .unwrap();
        let pages = AdmissionPages {
            member_count: 1,
            body_bytes: writer.bytes.len() as u64,
            body_hash: *blake3::hash(&writer.bytes).as_bytes(),
            objects,
            counts,
        };
        let header = pages.header();
        writer.bytes.extend_from_slice(&header);
        writer
            .bytes
            .extend_from_slice(&(header.len() as u32).to_be_bytes());
        assert!(CapsuleRunAdmission::decode(&writer.bytes, 1).is_err());
    }

    proptest::proptest! {
        #[test]
        fn admission_input_order_does_not_change_page_bytes(
            oids in proptest::collection::vec(proptest::array::uniform20(proptest::num::u8::ANY), 1..100),
        ) {
            let mut reversed = oids.clone(); reversed.reverse();
            let left = CapsuleRunAdmission::from_member_oids(&[oids], 1).unwrap().encode(1).unwrap();
            let right = CapsuleRunAdmission::from_member_oids(&[reversed], 1).unwrap().encode(1).unwrap();
            proptest::prop_assert_eq!(left, right);
        }
    }
}
