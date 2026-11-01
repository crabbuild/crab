use super::*;

pub(super) async fn open_for_writer(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    domain: &str,
) -> Result<Control> {
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let (mut control, version) = ready_control(store, path).await?;
        if control.epoch == u64::MAX || control.slot_limit() < 2 {
            return Err(malformed(path, "fence history exhausted"));
        }
        let Some(sweep) = &control.sweep else {
            return Ok(control);
        };
        let now = backend_unix_time(store, &Path::from(path)).await?;
        if sweep.blocks(now) {
            return Err(held(domain, sweep, true, control.epoch));
        }
        // Ordinary writers never mutate control on the hot path. Only an
        // expired sweep past its uncertainty window needs gate retirement.
        control.sweep = None;
        control.acquisition = None;
        control.epoch = next_epoch(path, control.epoch)?;
        match update(store, &Path::from(path), encode(path, &control)?, version).await {
            Ok(_) => {}
            Err(error) if conditional_conflict(&error) => {}
            Err(source) => return Err(store_error(path, source)),
        }
    }
    Err(conflict(path))
}

pub(super) async fn acquire(
    store: &Arc<dyn ObjectStore>,
    domain: &str,
    ttl: Duration,
    holder: String,
    recover: bool,
) -> Result<GcFenceLease> {
    let path = gc_fence_path(domain)?;
    // A journal holder survives recovery. Each acquisition still needs a
    // distinct identity so two resumptions cannot adopt each other's claim.
    let acquisition = uuid::Uuid::now_v7().to_string();
    let mut pending = None;
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let (mut control, version) = ready_control(store, &path).await?;
        let now = backend_unix_time(store, &Path::from(path.as_str())).await?;
        let retrying = pending.as_ref().is_some_and(|(incarnation, epoch)| {
            incarnation == &control.incarnation && *epoch == control.epoch
        }) && control.acquisition.as_ref() == Some(&acquisition)
            && control
                .sweep
                .as_ref()
                .is_some_and(|sweep| sweep.holder == holder && sweep.expires_at_backend > now);
        if let Some(sweep) = &control.sweep {
            let own_expired = recover && sweep.holder == holder && sweep.expires_at_backend <= now;
            if !retrying && !own_expired && sweep.blocks(now) {
                return Err(held(domain, sweep, true, control.epoch));
            }
        }
        // Reserve release before closing the gate. An exhausted counter must
        // not strand an acquired holder or make its history wrap.
        if !retrying {
            if control.epoch > u64::MAX - 2 {
                return Err(malformed(&path, "sweep history exhausted"));
            }
            control.epoch += 1;
        }
        pending = Some((control.incarnation.clone(), control.epoch));
        control.sweep = Some(Holder::new(&holder, now, ttl)?);
        control.acquisition = Some(acquisition.clone());
        match update(
            store,
            &Path::from(path.as_str()),
            encode(&path, &control)?,
            version,
        )
        .await
        {
            Ok(_) => return finish(store, path, domain, holder, ttl, control).await,
            Err(error) if conditional_conflict(&error) => continue,
            Err(source) => return Err(store_error(&path, source)),
        }
    }
    Err(conflict(&path))
}

async fn finish(
    store: &Arc<dyn ObjectStore>,
    path: String,
    domain: &str,
    holder: String,
    ttl: Duration,
    control: Control,
) -> Result<GcFenceLease> {
    let acquisition = control
        .acquisition
        .clone()
        .ok_or_else(|| malformed(&path, "missing sweep acquisition identity"))?;
    let mut lease = GcFenceLease::new(
        store,
        path.clone(),
        domain,
        holder,
        ttl,
        control.incarnation.clone(),
        Claim::Sweep {
            epoch: control.epoch,
            writer_epoch: control.writer_epoch,
            acquisition: acquisition.clone(),
        },
    );
    // The owned lease releases control if the scan is cancelled or fails.
    // A sweep is never exposed before every slot is checked.
    let writer_epoch = match scan(&lease.inner, &control).await {
        Ok(epoch) => epoch,
        Err(error) => {
            let _ = lease.release().await;
            return Err(error);
        }
    };
    let Some(inner) = Arc::get_mut(&mut lease.inner) else {
        return Err(malformed(&path, "sweep escaped before slot validation"));
    };
    inner.claim = Claim::Sweep {
        epoch: control.epoch,
        writer_epoch,
        acquisition,
    };
    Ok(lease)
}

async fn scan(inner: &LeaseInner, control: &Control) -> Result<u64> {
    let now = backend_unix_time(&inner.store, &Path::from(inner.path.as_str())).await?;
    let mut history = control.writer_epoch;
    let mut holders = std::collections::HashSet::new();
    for index in 0..GC_FENCE_MAX_WRITERS {
        let key = slot_path(&inner.path, &control.incarnation, index);
        let (slot, _) = load_slot(&inner.store, &key, control).await?;
        history = history
            .checked_add(slot.epoch)
            .ok_or_else(|| malformed(&key, "writer history sum exhausted"))?;
        if let Some(writer) = &slot.writer {
            if !holders.insert(writer.holder.clone()) {
                return Err(malformed(&key, "duplicate writer holder across slots"));
            }
            if writer.blocks(now) {
                return Err(held(&inner.domain, writer, false, control.epoch));
            }
        }
    }
    validate(inner).await?;
    Ok(history)
}

pub(super) async fn validate(inner: &LeaseInner) -> Result<()> {
    let Claim::Sweep {
        epoch,
        ref acquisition,
        ..
    } = inner.claim
    else {
        return Err(inner.lost());
    };
    let now = backend_unix_time(&inner.store, &Path::from(inner.path.as_str())).await?;
    let (control, _) = inner.control().await?;
    if control.epoch != epoch
        || control.acquisition.as_ref() != Some(acquisition)
        || !control
            .sweep
            .as_ref()
            .is_some_and(|sweep| sweep.holder == inner.holder && sweep.expires_at_backend > now)
    {
        return Err(inner.lost());
    }
    Ok(())
}

pub(super) async fn mutate(inner: &LeaseInner, renew: bool) -> Result<()> {
    let Claim::Sweep {
        epoch,
        ref acquisition,
        ..
    } = inner.claim
    else {
        return Err(inner.lost());
    };
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let now = backend_unix_time(&inner.store, &Path::from(inner.path.as_str())).await?;
        let (mut control, version) = inner.control().await?;
        let Some(sweep) = &control.sweep else {
            return if renew { Err(inner.lost()) } else { Ok(()) };
        };
        if control.epoch != epoch || control.acquisition.as_ref() != Some(acquisition) {
            return Err(inner.lost());
        }
        if sweep.holder != inner.holder {
            return if renew { Err(inner.lost()) } else { Ok(()) };
        }
        if sweep.expires_at_backend <= now {
            return Err(inner.lost());
        }
        if renew {
            control.sweep = Some(Holder::new(&inner.holder, now, inner.ttl)?);
        } else {
            control.sweep = None;
            control.acquisition = None;
            control.epoch = next_epoch(&inner.path, control.epoch)?;
        }
        match update(
            &inner.store,
            &Path::from(inner.path.as_str()),
            encode(&inner.path, &control)?,
            version,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) if conditional_conflict(&error) => continue,
            Err(source) => return Err(store_error(&inner.path, source)),
        }
    }
    Err(conflict(&inner.path))
}
