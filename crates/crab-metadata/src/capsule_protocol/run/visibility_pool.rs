//! Co-located visibility proofs keep compacted control reads away from Git payloads.
use super::*;

pub(super) fn build(
    body: &mut Vec<u8>,
    level: u8,
    capsules: &[RunCapsuleLocation],
) -> Result<Option<RunSectionLocation>> {
    if level == 0 || capsules.iter().all(|capsule| capsule.visibility.is_none()) {
        return Ok(None);
    }
    let offset = body.len();
    for range in capsules
        .iter()
        .filter_map(|capsule| capsule.visibility.as_ref())
    {
        let start = usize::try_from(range.offset)
            .map_err(|_| contract_error("visibility pool offset cannot be represented"))?;
        let end = range
            .offset
            .checked_add(range.length)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|end| *end <= offset)
            .ok_or_else(|| contract_error("visibility pool source is outside its capsules"))?;
        body.extend_from_within(start..end);
    }
    Ok(Some(RunSectionLocation {
        offset: offset as u64,
        length: (body.len() - offset) as u64,
        hash: blake3::hash(&body[offset..]).to_hex().to_string(),
    }))
}

pub(super) fn validate(footer: &CapsuleRunFooter, offset: u64, control_offset: u64) -> Result<u64> {
    let required = footer.level > 0
        && footer
            .capsules
            .iter()
            .any(|capsule| capsule.visibility.is_some());
    if required != footer.visibility_pool.is_some() {
        return Err(corrupt(
            "visibility pool differs from its level and capsule sections",
        ));
    }
    let Some(pool) = &footer.visibility_pool else {
        return Ok(offset);
    };
    let end = footer
        .catalog
        .as_ref()
        .or(footer.admission.as_ref())
        .map_or(control_offset, |range| range.offset);
    validate_body_range(pool, offset, end)?;
    let length = footer
        .capsules
        .iter()
        .filter_map(|capsule| capsule.visibility.as_ref())
        .try_fold(0u64, |length, section| length.checked_add(section.length))
        .ok_or_else(|| corrupt("visibility pool length overflows"))?;
    if pool.length != length {
        return Err(corrupt(
            "visibility pool does not cover its capsule sections",
        ));
    }
    Ok(end)
}

pub(super) fn verify(bytes: &[u8], footer: &CapsuleRunFooter) -> Result<()> {
    let Some(pool) = &footer.visibility_pool else {
        return Ok(());
    };
    let body = bytes
        .get(pool.offset as usize..(pool.offset + pool.length) as usize)
        .ok_or_else(|| corrupt("visibility pool is outside its run"))?;
    if blake3::hash(body).to_hex().as_str() != pool.hash {
        return Err(corrupt("visibility pool hash differs"));
    }
    let mut offset = 0;
    for range in footer
        .capsules
        .iter()
        .filter_map(|capsule| capsule.visibility.as_ref())
    {
        let end = offset + range.length as usize;
        let section = body
            .get(offset..end)
            .ok_or_else(|| corrupt("pooled visibility section is truncated"))?;
        if blake3::hash(section).to_hex().as_str() != range.hash {
            return Err(corrupt(
                "pooled visibility differs from its original capsule",
            ));
        }
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsule_protocol::{CapsuleRefEdit, CapsuleSection};
    use crate::git_visibility::GitVisibilityEdit;

    fn compacted() -> CapsuleRun {
        let leaves = (1..=2)
            .map(|version| {
                let oid = format!("{version:040x}");
                let tx = CapsuleTransaction::new(
                    &"1".repeat(64),
                    vec![CapsuleRefEdit::new(
                        "refs/heads/main",
                        None,
                        Some(oid.clone()),
                        None,
                    )],
                )
                .unwrap();
                let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
                    "refs/heads/main".into(),
                    GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid]),
                )]))
                .unwrap();
                CapsuleRun::leaf(
                    Capsule::build(
                        &tx,
                        vec![],
                        vec![CapsuleSection::new(
                            CapsuleSectionKind::VisibilityDelta,
                            delta.encode().unwrap(),
                        )],
                    )
                    .unwrap(),
                )
                .unwrap()
            })
            .collect();
        CapsuleRun::compact(leaves).unwrap()
    }

    fn resign(mut body: Vec<u8>, footer: &CapsuleRunFooter) -> Bytes {
        let footer = serde_json::to_vec(footer).unwrap();
        body.extend_from_slice(&footer);
        body.extend_from_slice(&(footer.len() as u64).to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer).as_bytes());
        body.extend_from_slice(RUN_MAGIC);
        body.into()
    }

    #[test]
    fn invalid_pool_routing_fails_full_and_footer_only_decoding() {
        let run = compacted();
        for change in ["missing", "offset", "length", "hash"] {
            let mut footer = run.footer.clone();
            match change {
                "missing" => footer.visibility_pool = None,
                "offset" => footer.visibility_pool.as_mut().unwrap().offset += 1,
                "length" => footer.visibility_pool.as_mut().unwrap().length += 1,
                "hash" => footer.visibility_pool.as_mut().unwrap().hash = "invalid".into(),
                _ => unreachable!(),
            }
            let body = resign(
                run.bytes()[..run.control_offset() as usize].to_vec(),
                &footer,
            );
            let full = CapsuleRun::decode(body.clone()).is_err();
            let control = CapsuleRunControl::decode_suffix(
                body.slice(run.control_offset() as usize..),
                body.len() as u64,
                run.hash(),
                run.level(),
                &run.transaction_ids(),
                run.newest_base_root_digest(),
            )
            .is_err();
            assert_eq!((full, control), (true, true), "{change}");
        }
    }

    #[test]
    fn re_signed_pool_cannot_replace_original_capsule_evidence() {
        let run = compacted();
        let mut footer = run.footer.clone();
        let pool = footer.visibility_pool.as_mut().unwrap();
        let mut body = run.bytes()[..run.control_offset() as usize].to_vec();
        body[pool.offset as usize] ^= 1;
        pool.hash = blake3::hash(&body[pool.offset as usize..(pool.offset + pool.length) as usize])
            .to_hex()
            .to_string();
        assert!(CapsuleRun::decode(resign(body, &footer)).is_err());
    }
}
