use super::*;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Holder {
    pub holder: String,
    pub expires_at_backend: i64,
    pub lease_secs: u64,
}

impl Holder {
    pub fn new(holder: &str, now: i64, ttl: Duration) -> Result<Self> {
        let expires_at_backend = now
            .checked_add(i64::try_from(ttl.as_secs()).map_err(|_| {
                malformed(
                    "gc-fence",
                    "lease duration exceeds the backend expiry range",
                )
            })?)
            .ok_or_else(|| malformed("gc-fence", "backend expiry exhausted"))?;
        Ok(Self {
            holder: holder.to_owned(),
            expires_at_backend,
            lease_secs: ttl.as_secs(),
        })
    }

    fn validate(&self, path: &str) -> Result<()> {
        if self.holder.is_empty() || self.expires_at_backend <= 0 || self.lease_secs == 0 {
            return Err(malformed(path, "invalid holder lease fields"));
        }
        Ok(())
    }

    pub fn blocks(&self, now: i64) -> bool {
        self.expires_at_backend
            .saturating_add(DEFAULT_GC_FENCE_QUARANTINE.as_secs() as i64)
            > now
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Control {
    pub schema_version: u32,
    pub incarnation: String,
    pub epoch: u64,
    pub writer_epoch: u64,
    pub ready: bool,
    #[serde(deserialize_with = "required_option")]
    pub sweep: Option<Holder>,
    #[serde(deserialize_with = "required_option")]
    pub acquisition: Option<String>,
}

impl Control {
    pub fn empty() -> Self {
        Self {
            schema_version: GC_FENCE_SCHEMA_VERSION,
            incarnation: uuid::Uuid::now_v7().to_string(),
            epoch: 0,
            writer_epoch: 0,
            ready: false,
            sweep: None,
            acquisition: None,
        }
    }

    pub fn validate(&self, path: &str) -> Result<()> {
        validate_identity(path, self.schema_version, &self.incarnation)?;
        if !self.ready && self.sweep.is_some() {
            return Err(malformed(path, "uninitialized fence has a sweep holder"));
        }
        if self.sweep.is_some() != self.acquisition.is_some()
            || self.acquisition.as_ref().is_some_and(|id| {
                !uuid::Uuid::try_parse(id)
                    .is_ok_and(|uuid| uuid.get_version_num() == 7 && uuid.to_string() == *id)
            })
        {
            return Err(malformed(path, "invalid sweep acquisition identity"));
        }
        if let Some(holder) = &self.sweep {
            holder.validate(path)?;
        }
        Ok(())
    }

    pub fn slot_limit(&self) -> u64 {
        // Reserve an even budget per slot, including the release increment.
        // Summation can therefore never overflow the persisted journal token.
        ((u64::MAX - self.writer_epoch) / GC_FENCE_MAX_WRITERS as u64) & !1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Slot {
    pub schema_version: u32,
    pub incarnation: String,
    pub epoch: u64,
    #[serde(deserialize_with = "required_option")]
    pub writer: Option<Holder>,
}

// Nullable authority fields must still be present: an omitted holder is
// malformed storage, never permission to treat a live claim as released.
pub(super) fn required_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

impl Slot {
    pub fn empty(control: &Control) -> Self {
        Self {
            schema_version: GC_FENCE_SCHEMA_VERSION,
            incarnation: control.incarnation.clone(),
            epoch: 0,
            writer: None,
        }
    }

    pub fn validate(&self, path: &str, control: &Control) -> Result<()> {
        validate_identity(path, self.schema_version, &self.incarnation)?;
        if self.incarnation != control.incarnation
            || self.epoch > control.slot_limit()
            || self.writer.is_some() != (self.epoch % 2 == 1)
        {
            return Err(malformed(path, "invalid writer slot identity or history"));
        }
        if let Some(holder) = &self.writer {
            holder.validate(path)?;
        }
        Ok(())
    }
}

fn validate_identity(path: &str, version: u32, incarnation: &str) -> Result<()> {
    if version != GC_FENCE_SCHEMA_VERSION {
        return Err(malformed(
            path,
            "unsupported GC fence schema; quiesce and run crab migrate gc-fence",
        ));
    }
    if !uuid::Uuid::try_parse(incarnation)
        .is_ok_and(|id| id.get_version_num() == 7 && id.to_string() == incarnation)
    {
        return Err(malformed(path, "invalid domain incarnation"));
    }
    Ok(())
}

pub(super) fn slot_path(path: &str, incarnation: &str, slot: usize) -> String {
    format!("{path}/writers/{incarnation}/{slot:02}")
}

pub(super) fn encode<T: Serialize>(path: &str, value: &T) -> Result<Bytes> {
    serde_json::to_vec(value)
        .map(Bytes::from)
        .map_err(|source| CoordinationError::Serialize {
            key: path.to_owned(),
            context: "GC fence state",
            source,
        })
}

pub(super) fn decode<T: DeserializeOwned>(path: &str, body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|source| malformed(path, &source.to_string()))
}

pub(super) async fn load_control(
    store: &Arc<dyn ObjectStore>,
    path: &str,
) -> Result<(Control, UpdateVersion)> {
    let (body, version) = get_with_version(store, &Path::from(path))
        .await
        .map_err(|source| store_error(path, source))?;
    let control: Control = decode(path, &body)?;
    control.validate(path)?;
    Ok((control, version))
}

pub(super) async fn load_slot(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    control: &Control,
) -> Result<(Slot, UpdateVersion)> {
    // Missing initialized ledgers are corruption, never empty history. GC
    // reads every known slot rather than relying on provider LIST snapshots.
    let (body, version) = get_with_version(store, &Path::from(path))
        .await
        .map_err(|source| store_error(path, source))?;
    let slot: Slot = decode(path, &body)?;
    slot.validate(path, control)?;
    Ok((slot, version))
}

pub(super) async fn initialize_slots(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    control: &Control,
) -> Result<()> {
    for index in 0..GC_FENCE_MAX_WRITERS {
        let key = slot_path(path, &control.incarnation, index);
        let empty = Slot::empty(control);
        match create_strict(store, &Path::from(key.as_str()), encode(&key, &empty)?).await {
            Ok(_) => {}
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => {
                let (slot, _) = load_slot(store, &key, control).await?;
                if slot.epoch != 0 || slot.writer.is_some() {
                    return Err(malformed(
                        &key,
                        "uninitialized fence contains writer history",
                    ));
                }
            }
            Err(source) => return Err(store_error(&key, source)),
        }
    }
    Ok(())
}

pub(super) async fn ready_control(
    store: &Arc<dyn ObjectStore>,
    path: &str,
) -> Result<(Control, UpdateVersion)> {
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let loaded = match get_with_version(store, &Path::from(path)).await {
            Ok((body, version)) => {
                let control: Control = decode(path, &body)?;
                control.validate(path)?;
                Some((control, version))
            }
            Err(object_store::Error::NotFound { .. }) => None,
            Err(source) => return Err(store_error(path, source)),
        };
        let Some((mut control, version)) = loaded else {
            let control = Control::empty();
            match create_strict(store, &Path::from(path), encode(path, &control)?).await {
                Ok(_)
                | Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => continue,
                Err(source) => return Err(store_error(path, source)),
            }
        };
        if control.ready {
            return Ok((control, version));
        }
        if let Err(error) = initialize_slots(store, path, &control).await {
            // Another initializer can publish readiness and admit writers
            // while this initializer is still inspecting its zero ledgers.
            let (fresh, version) = load_control(store, path).await?;
            if fresh.ready && fresh.incarnation == control.incarnation {
                return Ok((fresh, version));
            }
            return Err(error);
        }
        control.ready = true;
        match update(store, &Path::from(path), encode(path, &control)?, version).await {
            Ok(version) => return Ok((control, version)),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => continue,
            Err(source) => return Err(store_error(path, source)),
        }
    }
    Err(conflict(path))
}
