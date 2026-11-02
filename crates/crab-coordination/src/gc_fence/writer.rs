use super::*;

pub(super) async fn acquire(
    store: &Arc<dyn ObjectStore>,
    domain: &str,
    ttl: Duration,
    holder: String,
) -> Result<GcFenceLease> {
    let path = gc_fence_path(domain)?;
    let control = sweep::open_for_writer(store, &path, domain).await?;
    // The holder's random suffix distributes private slots. Colliding owners
    // probe the remaining slots, preserving all 64 concurrent admissions.
    let start = usize::from(blake3::hash(holder.as_bytes()).as_bytes()[0]) % GC_FENCE_MAX_WRITERS;
    let mut conflicts = 0;
    for offset in 0..GC_FENCE_MAX_WRITERS {
        let key = slot_path(
            &path,
            &control.incarnation,
            (start + offset) % GC_FENCE_MAX_WRITERS,
        );
        loop {
            let now = backend_unix_time(store, &Path::from(key.as_str())).await?;
            let (mut slot, version) = load_slot(store, &key, &control).await?;
            let retrying = slot
                .writer
                .as_ref()
                .is_some_and(|writer| writer.holder == holder);
            if let Some(writer) = &slot.writer {
                if retrying && writer.expires_at_backend <= now {
                    return Err(CoordinationError::GcFenceLost {
                        domain: domain.to_owned(),
                        holder,
                    });
                }
                if !retrying && writer.blocks(now) {
                    break;
                }
            }
            if !retrying {
                let increment = if slot.writer.is_some() { 2 } else { 1 };
                slot.epoch = slot
                    .epoch
                    .checked_add(increment)
                    .filter(|epoch| *epoch < control.slot_limit())
                    .ok_or_else(|| {
                        malformed(
                            &key,
                            "writer history exhausted; release increment is reserved",
                        )
                    })?;
            }
            let writer = Holder::new(&holder, now, ttl)?;
            let expires_at_backend = writer.expires_at_backend;
            slot.writer = Some(writer);
            match update(
                store,
                &Path::from(key.as_str()),
                encode(&key, &slot)?,
                version,
            )
            .await
            {
                Ok(version) => {
                    let lease = GcFenceLease::new(
                        store,
                        path.clone(),
                        domain,
                        holder,
                        ttl,
                        control.incarnation.clone(),
                        Claim::Writer {
                            path: key,
                            admitted: slot,
                            version,
                        },
                    );
                    // A stale contender may fill a slot after GC closed its
                    // gate and scanned that slot. It must never start uploads.
                    let checked = lease.inner.control().await;
                    if matches!(&checked, Ok((fresh, _)) if fresh.epoch == control.epoch && fresh.sweep.is_none())
                    {
                        lease.inner.validate_expiry(expires_at_backend).await?;
                        return Ok(lease);
                    }
                    let primary = checked.err().unwrap_or_else(|| conflict(&path));
                    let _ = lease.release().await;
                    return Err(primary);
                }
                Err(error) if conditional_conflict(&error) => {
                    conflicts += 1;
                    if conflicts >= GC_FENCE_MAX_CAS_ATTEMPTS {
                        return Err(conflict(&key));
                    }
                    // Keep this candidate until its outcome is known. A PUT
                    // can commit before a lost response is retried by the SDK.
                }
                Err(source) => return Err(store_error(&key, source)),
            }
        }
    }
    Err(CoordinationError::GcFenceHeld {
        domain: domain.to_owned(),
        holder: "writer-capacity".to_owned(),
        sweep: false,
        epoch: control.epoch,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mutation {
    Renew,
    Release,
    ReleaseCommitted,
}

pub(super) async fn mutate(inner: &LeaseInner, mutation: Mutation) -> Result<()> {
    let Claim::Writer {
        path,
        admitted,
        version,
    } = &inner.claim
    else {
        return Err(inner.lost());
    };
    let (control, _) = inner.control().await?;
    if mutation == Mutation::ReleaseCommitted {
        let mut released = admitted.clone();
        released.epoch = next_epoch(path, released.epoch)?;
        released.writer = None;
        match update(
            &inner.store,
            &Path::from(path.as_str()),
            encode(path, &released)?,
            version.clone(),
        )
        .await
        {
            Ok(_) => {
                inner.control().await?;
                return Ok(());
            }
            Err(error) if conditional_conflict(&error) => {}
            Err(source) => return Err(store_error(path, source)),
        }
    }
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let now = if mutation == Mutation::ReleaseCommitted {
            None
        } else {
            Some(backend_unix_time(&inner.store, &Path::from(path.as_str())).await?)
        };
        let (mut slot, version) = load_slot(&inner.store, path, &control).await?;
        let owned = slot
            .writer
            .as_ref()
            .is_some_and(|writer| writer.holder == inner.holder);
        if !owned {
            return if mutation == Mutation::Renew {
                Err(inner.lost())
            } else {
                Ok(())
            };
        }
        if slot.epoch != admitted.epoch
            || now.is_some_and(|now| {
                slot.writer
                    .as_ref()
                    .is_some_and(|writer| writer.expires_at_backend <= now)
            })
        {
            return Err(inner.lost());
        }
        match mutation {
            Mutation::Renew => {
                slot.writer = Some(Holder::new(
                    &inner.holder,
                    now.ok_or_else(|| inner.lost())?,
                    inner.ttl,
                )?)
            }
            Mutation::Release | Mutation::ReleaseCommitted => {
                slot.writer = None;
                slot.epoch = next_epoch(path, slot.epoch)?;
            }
        }
        let expiry = slot.writer.as_ref().map(|writer| writer.expires_at_backend);
        match update(
            &inner.store,
            &Path::from(path.as_str()),
            encode(path, &slot)?,
            version,
        )
        .await
        {
            Ok(_) => {
                inner.control().await?;
                if let Some(expiry) = expiry {
                    inner.validate_expiry(expiry).await?;
                }
                return Ok(());
            }
            Err(error) if conditional_conflict(&error) => continue,
            Err(source) => return Err(store_error(path, source)),
        }
    }
    Err(conflict(path))
}
