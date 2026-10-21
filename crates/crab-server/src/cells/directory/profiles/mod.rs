use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateProfileInput {
    pub submission_id: Uuid,
    pub identity: IdentityKey,
    pub handle: String,
    pub name: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditProfileInput {
    pub identity: IdentityKey,
    pub version: u64,
    pub handle: String,
    pub name: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ProfileOutcome {
    Saved(ActorProfile),
    AlreadyRegistered,
    HandleTaken,
    Conflict,
    RequestConflict,
    NotFound,
}

pub(crate) fn validate_profile(handle: &str, name: &str) -> std::result::Result<(), &'static str> {
    validate_handle(handle)?;
    if name.trim() != name
        || name.is_empty()
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
    {
        return Err(
            "Display name must contain 1–100 characters without surrounding whitespace or controls",
        );
    }
    Ok(())
}
pub(crate) fn validate_handle(handle: &str) -> std::result::Result<(), &'static str> {
    if handle.is_empty()
        || handle.len() > 39
        || handle.starts_with('-')
        || handle.ends_with('-')
        || handle.contains("--")
        || !handle
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Username must contain 1–39 lowercase letters, digits or single hyphens");
    }
    Ok(())
}
fn handle_taken(context: &Transaction<'_>, handle: &str, actor: Option<Uuid>) -> Result<bool> {
    let rows = context.query(
        "SELECT id FROM actors WHERE handle = ?",
        &[SqlValue::Text(handle.into())],
    )?;
    match rows.rows.first().map(Vec::as_slice) {
        None => Ok(false),
        Some([SqlValue::Blob(id)]) => Ok(Some(Uuid::from_slice(id).map_err(Error::sdk)?) != actor),
        _ => Err(Error::Invalid("actor handle record is invalid")),
    }
}

pub(crate) struct CreateProfile;
impl Mutation for CreateProfile {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 1;
    type Input = CreateProfileInput;
    type Output = ProfileOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        input.identity.validate()?;
        validate_profile(&input.handle, &input.name).map_err(Error::Invalid)?;
        if input.submission_id.is_nil() {
            return Err(Error::Invalid("profile submission ID is invalid"));
        }
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.identity,
            &input.handle,
            &input.name,
        ))?);
        let submission = context.query(
            "SELECT digest, actor_id FROM actor_submissions WHERE request_id = ?",
            &[SqlValue::Blob(input.submission_id.as_bytes().to_vec())],
        )?;
        if let Some(row) = submission.rows.first() {
            let [SqlValue::Blob(previous), SqlValue::Blob(id)] = row.as_slice() else {
                return Err(Error::Invalid("profile submission record is invalid"));
            };
            if previous.as_slice() != digest.as_bytes() {
                return Ok(MutationResult::Rejected(ProfileOutcome::RequestConflict));
            }
            let actor = profile(context, Uuid::from_slice(id).map_err(Error::sdk)?)?
                .ok_or(Error::Invalid("profile submission has no actor"))?;
            return Ok(MutationResult::Success(ProfileOutcome::Saved(actor)));
        }
        if own_profile(context, &input.identity)?.is_some() {
            return Ok(MutationResult::Rejected(ProfileOutcome::AlreadyRegistered));
        }
        if handle_taken(context, &input.handle, None)? {
            return Ok(MutationResult::Rejected(ProfileOutcome::HandleTaken));
        }
        let now = u64::try_from(context.now_ms())
            .map_err(|_| Error::Invalid("profile clock is negative"))?;
        let actor = ActorProfile {
            id: Uuid::now_v7(),
            handle: input.handle,
            name: input.name,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.query("INSERT INTO actors(id, issuer, subject, handle, name, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, 1, ?, ?)", &[
            SqlValue::Blob(actor.id.as_bytes().to_vec()), SqlValue::Text(input.identity.issuer), SqlValue::Text(input.identity.subject),
            SqlValue::Text(actor.handle.clone()), SqlValue::Text(actor.name.clone()), SqlValue::Integer(context.now_ms()), SqlValue::Integer(context.now_ms()),
        ])?;
        context.query(
            "INSERT INTO actor_submissions(request_id, digest, actor_id) VALUES (?, ?, ?)",
            &[
                SqlValue::Blob(input.submission_id.as_bytes().to_vec()),
                SqlValue::Blob(digest.as_bytes().to_vec()),
                SqlValue::Blob(actor.id.as_bytes().to_vec()),
            ],
        )?;
        advance(context)?;
        Ok(MutationResult::Success(ProfileOutcome::Saved(actor)))
    }
}

pub(crate) struct EditProfile;
impl Mutation for EditProfile {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 2;
    type Input = EditProfileInput;
    type Output = ProfileOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        input.identity.validate()?;
        validate_profile(&input.handle, &input.name).map_err(Error::Invalid)?;
        if !(1..=MAX_VERSION).contains(&input.version) {
            return Err(Error::Invalid("profile version is invalid"));
        }
        let Some(mut actor) = own_profile(context, &input.identity)? else {
            return Ok(MutationResult::Rejected(ProfileOutcome::NotFound));
        };
        let unchanged = actor.handle == input.handle && actor.name == input.name;
        // A lost HTTP response is retried with its captured version and values.
        // Only the immediate identical successor proves that edit already won.
        if unchanged
            && (actor.version == input.version || actor.version == input.version.saturating_add(1))
        {
            return Ok(MutationResult::Success(ProfileOutcome::Saved(actor)));
        }
        if actor.version != input.version {
            return Ok(MutationResult::Rejected(ProfileOutcome::Conflict));
        }
        if actor.version == MAX_VERSION {
            return Err(Error::Invalid("profile version limit reached"));
        }
        if handle_taken(context, &input.handle, Some(actor.id))? {
            return Ok(MutationResult::Rejected(ProfileOutcome::HandleTaken));
        }
        actor.version += 1;
        actor.handle = input.handle;
        actor.name = input.name;
        actor.updated_at_ms = u64::try_from(context.now_ms())
            .map_err(|_| Error::Invalid("profile clock is negative"))?
            .max(actor.updated_at_ms);
        let updated = i64::try_from(actor.updated_at_ms)
            .map_err(|_| Error::Invalid("profile timestamp exceeds its range"))?;
        context.query("UPDATE actors SET handle = ?, name = ?, version = ?, updated_at_ms = ? WHERE id = ? AND version = ?", &[
            SqlValue::Text(actor.handle.clone()), SqlValue::Text(actor.name.clone()), SqlValue::Integer(i64::try_from(actor.version).map_err(|_| Error::Invalid("profile version exceeds its range"))?),
            SqlValue::Integer(updated), SqlValue::Blob(actor.id.as_bytes().to_vec()), SqlValue::Integer(i64::try_from(input.version).map_err(|_| Error::Invalid("profile version exceeds its range"))?),
        ])?;
        advance(context)?;
        Ok(MutationResult::Success(ProfileOutcome::Saved(actor)))
    }
}

#[cfg(test)]
mod tests;
