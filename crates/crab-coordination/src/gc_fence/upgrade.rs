//! Explicit schema-1 migration; never part of ordinary writer admission.

use super::*;

/// Validates or upgrades one quiesced GC domain using its observed version.
///
/// Before `apply`, the caller must stop every writer and sweeper, including
/// older binaries, across this physical domain. Any recorded holder or
/// quarantine blocks migration, even if expired. A conflict requires a fresh
/// operator inspection, not an automatic retry. Returns whether state changed.
pub async fn upgrade_gc_fence(
    store: &Arc<dyn ObjectStore>,
    domain: &str,
    apply: bool,
) -> Result<bool> {
    let path = gc_fence_path(domain)?;
    let (body, version) = get_with_version(store, &Path::from(path.as_str()))
        .await
        .map_err(|source| store_error(&path, source))?;
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&body).map_err(|source| CoordinationError::Serialize {
            key: path.clone(),
            context: "GC fence migration input",
            source,
        })?;
    if legacy["schema_version"] == GC_FENCE_SCHEMA_VERSION {
        deserialize_state(&path, &body)?.validate(&path)?;
        return Ok(false);
    }
    if legacy["schema_version"] != 1 || legacy.get("incarnation").is_some() {
        return Err(CoordinationError::GcFenceMalformed {
            path,
            reason: "migration requires an unmodified schema-1 fence".to_owned(),
        });
    }
    // Legacy decoding is confined to migration. Normal admission must refuse
    // old state, and old binaries must refuse the new required field/version.
    legacy["schema_version"] = GC_FENCE_SCHEMA_VERSION.into();
    legacy["incarnation"] = uuid::Uuid::now_v7().to_string().into();
    let state: GcFenceState =
        serde_json::from_value(legacy).map_err(|source| CoordinationError::Serialize {
            key: path.clone(),
            context: "GC fence migration input",
            source,
        })?;
    state.validate(&path)?;
    if !state.writers.is_empty()
        || state.sweep.is_some()
        || !state.quarantine.is_empty()
        || state.quarantine_block_until_backend.is_some()
        || state.epoch == u64::MAX
        || state.writer_epoch == u64::MAX
    {
        return Err(CoordinationError::GcFenceMalformed {
            path,
            reason: "migration requires released holders, cleared quarantine and unexhausted epochs; quiesce and recover with the previous binary first".to_owned(),
        });
    }
    if !apply {
        return Ok(false);
    }
    update(
        store,
        &Path::from(path.as_str()),
        serialize_state(&path, &state)?,
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

    fn legacy() -> serde_json::Value {
        let mut value = serde_json::to_value(GcFenceState::empty()).unwrap();
        value["schema_version"] = 1.into();
        value.as_object_mut().unwrap().remove("incarnation");
        value
    }

    #[tokio::test]
    async fn migration_is_explicit_idempotent_and_preserves_epochs() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from(gc_fence_path("repo").unwrap());
        let mut old = legacy();
        old["epoch"] = 12.into();
        old["writer_epoch"] = 8.into();
        let body = Bytes::from(serde_json::to_vec(&old).unwrap());
        store.put(&path, body.clone().into()).await.unwrap();
        assert!(
            GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
                .await
                .is_err()
        );
        assert!(!upgrade_gc_fence(&store, "repo", false).await.unwrap());
        assert_eq!(store.get(&path).await.unwrap().bytes().await.unwrap(), body);
        assert!(upgrade_gc_fence(&store, "repo", true).await.unwrap());
        let upgraded = store.get(&path).await.unwrap().bytes().await.unwrap();
        let state = deserialize_state(path.as_ref(), &upgraded).unwrap();
        assert_eq!(
            (state.schema_version, state.epoch, state.writer_epoch),
            (2, 12, 8)
        );
        state.validate(path.as_ref()).unwrap();
        assert!(!upgrade_gc_fence(&store, "repo", true).await.unwrap());
        assert_eq!(
            store.get(&path).await.unwrap().bytes().await.unwrap(),
            upgraded
        );
        let writer = GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
            .await
            .unwrap();
        writer.release().await.unwrap();
    }

    #[tokio::test]
    async fn migration_refuses_uncertain_or_malformed_state_without_writes() {
        for (key, value) in [
            (
                "writers",
                serde_json::json!([{"holder":"old", "expires_at_backend":1, "lease_secs":30}]),
            ),
            (
                "sweep",
                serde_json::json!({"holder":"old", "expires_at_backend":1, "lease_secs":30}),
            ),
            (
                "quarantine",
                serde_json::json!([{"holder":"old", "mode":"writer", "expired_at_backend":1, "quarantine_until_backend":2}]),
            ),
            ("quarantine_block_until_backend", 2.into()),
            ("epoch", u64::MAX.into()),
            ("schema_version", 99.into()),
            ("unexpected", true.into()),
        ] {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path = Path::from(gc_fence_path("repo").unwrap());
            let mut old = legacy();
            old[key] = value;
            let body = Bytes::from(serde_json::to_vec(&old).unwrap());
            store.put(&path, body.clone().into()).await.unwrap();
            assert!(
                upgrade_gc_fence(&store, "repo", true).await.is_err(),
                "{key}"
            );
            assert_eq!(store.get(&path).await.unwrap().bytes().await.unwrap(), body);
        }
    }
}
