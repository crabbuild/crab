//! Retired fence decoding is confined to explicit, quiesced migration.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retired {
    schema_version: u32,
    incarnation: Option<String>,
    epoch: u64,
    writer_epoch: u64,
    writers: Vec<serde_json::Value>,
    #[serde(deserialize_with = "required_option")]
    sweep: Option<serde_json::Value>,
    quarantine: Vec<serde_json::Value>,
    #[serde(deserialize_with = "required_option")]
    quarantine_block_until_backend: Option<i64>,
}

/// Validate or upgrade one externally quiesced GC domain using its observed version.
///
/// Before applying, stop all writers/sweepers, including old binaries. Any
/// recorded holder or quarantine blocks migration, even if expired. A conflict
/// requires fresh operator inspection. Restart GC with a fresh plan afterward.
pub async fn upgrade_gc_fence(
    store: &Arc<dyn ObjectStore>,
    domain: &str,
    apply: bool,
) -> Result<bool> {
    let path = gc_fence_path(domain)?;
    let (body, version) = get_with_version(store, &Path::from(path.as_str()))
        .await
        .map_err(|source| store_error(&path, source))?;
    let value: serde_json::Value = decode(&path, &body)?;
    if value["schema_version"] == GC_FENCE_SCHEMA_VERSION {
        let control: Control = decode(&path, &body)?;
        control.validate(&path)?;
        if control.ready {
            for index in 0..GC_FENCE_MAX_WRITERS {
                load_slot(
                    store,
                    &slot_path(&path, &control.incarnation, index),
                    &control,
                )
                .await?;
            }
            return Ok(false);
        }
        if !apply {
            return Ok(false);
        }
        ready_control(store, &path).await?;
        return Ok(true);
    }
    let retired: Retired = decode(&path, &body)?;
    let valid_shape = match retired.schema_version {
        1 => value.get("incarnation").is_none(),
        2 => retired.incarnation.is_some(),
        _ => false,
    };
    if !valid_shape {
        return Err(malformed(
            &path,
            "migration requires an unmodified schema-1 or schema-2 fence",
        ));
    }
    if !retired.writers.is_empty()
        || retired.sweep.is_some()
        || !retired.quarantine.is_empty()
        || retired.quarantine_block_until_backend.is_some()
        || retired.writer_epoch > retired.epoch
    {
        return Err(malformed(
            &path,
            "migration requires released holders and cleared quarantine; quiesce and recover with the previous binary first",
        ));
    }
    let control = Control {
        schema_version: GC_FENCE_SCHEMA_VERSION,
        incarnation: retired
            .incarnation
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
        epoch: retired.epoch - retired.writer_epoch,
        writer_epoch: retired.writer_epoch,
        ready: true,
        sweep: None,
        acquisition: None,
    };
    control.validate(&path)?;
    if control.slot_limit() < 2 || control.epoch > u64::MAX - 2 {
        return Err(malformed(&path, "migration requires unexhausted epochs"));
    }
    if !apply {
        return Ok(false);
    }
    // Prepare persistent zero ledgers before publishing the new control. An
    // old writer crossing operator quiescence changes the CAS and aborts this
    // migration; it can never be hidden behind a ready new control record.
    initialize_slots(store, &path, &control).await?;
    update(
        store,
        &Path::from(path.as_str()),
        encode(&path, &control)?,
        version,
    )
    .await
    .map_err(|source| store_error(&path, source))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{ObjectStoreExt, memory::InMemory};

    fn retired(version: u32) -> serde_json::Value {
        let mut value = serde_json::json!({ "schema_version":version, "epoch":12, "writer_epoch":8, "writers":[], "sweep":null, "quarantine":[], "quarantine_block_until_backend":null });
        if version == 2 {
            value["incarnation"] = uuid::Uuid::now_v7().to_string().into();
        }
        value
    }

    #[tokio::test]
    async fn migration_is_explicit_idempotent_and_preserves_writer_history() {
        for version in [1, 2] {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path = gc_fence_path("repo").unwrap();
            let old = retired(version);
            let body = encode(&path, &old).unwrap();
            store
                .put(&Path::from(path.as_str()), body.clone().into())
                .await
                .unwrap();
            assert!(
                GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
                    .await
                    .is_err()
            );
            assert!(!upgrade_gc_fence(&store, "repo", false).await.unwrap());
            assert_eq!(
                store
                    .get(&Path::from(path.as_str()))
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
                body
            );
            assert!(upgrade_gc_fence(&store, "repo", true).await.unwrap());
            let (control, _) = load_control(&store, &path).await.unwrap();
            if version == 2 {
                assert_eq!(control.incarnation, old["incarnation"].as_str().unwrap());
            }
            assert_eq!((control.epoch, control.writer_epoch), (4, 8));
            assert!(!upgrade_gc_fence(&store, "repo", true).await.unwrap());
            let sweep = GcFenceLease::acquire_sweep(&store, "repo", Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(sweep.writer_epoch().unwrap().epoch, 8);
            sweep.release().await.unwrap();
            let writer = GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
                .await
                .unwrap();
            writer.release().await.unwrap();
            let sweep = GcFenceLease::acquire_sweep(&store, "repo", Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(sweep.writer_epoch().unwrap().epoch, 10);
            sweep.release().await.unwrap();
        }
    }

    #[tokio::test]
    async fn migration_refuses_uncertain_or_malformed_state_without_writes() {
        for version in [1, 2] {
            for (key, value) in [
                (
                    "writers",
                    serde_json::json!([{"holder":"old", "expires_at_backend":1, "lease_secs":30}]),
                ),
                (
                    "sweep",
                    serde_json::json!({"holder":"old", "expires_at_backend":1, "lease_secs":30}),
                ),
                ("quarantine", serde_json::json!([{"holder":"old"}])),
                ("quarantine_block_until_backend", 2.into()),
                ("writer_epoch", u64::MAX.into()),
                ("schema_version", 99.into()),
                ("unexpected", true.into()),
            ] {
                let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                let path = gc_fence_path("repo").unwrap();
                let mut old = retired(version);
                old[key] = value;
                let body = encode(&path, &old).unwrap();
                store
                    .put(&Path::from(path.as_str()), body.clone().into())
                    .await
                    .unwrap();
                assert!(
                    upgrade_gc_fence(&store, "repo", true).await.is_err(),
                    "{version}:{key}"
                );
                assert_eq!(
                    store
                        .get(&Path::from(path.as_str()))
                        .await
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap(),
                    body
                );
            }
        }
    }
}
