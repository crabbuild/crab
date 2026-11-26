use super::*;

pub(crate) struct ResolveHandles;
impl Read for ResolveHandles {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 7;
    type Input = Vec<String>;
    type Output = Vec<Option<IdentityKey>>;
    fn execute(context: &mut Transaction<'_>, handles: Self::Input) -> Result<Self::Output> {
        if handles.len() > 64 {
            return Err(Error::Invalid("Handle resolution is limited to 64 names"));
        }
        for handle in &handles {
            validate_handle(handle).map_err(Error::Invalid)?;
        }
        handles
            .iter()
            .map(|handle| {
                let rows = context.query(
                    "SELECT a.issuer, a.subject FROM accounts a JOIN profiles p ON p.id = a.id WHERE p.handle = ?",
                    &[SqlValue::Text(handle.clone())],
                )?;
                match rows.rows.first().map(Vec::as_slice) {
                    None => Ok(None),
                    Some([SqlValue::Text(issuer), SqlValue::Text(subject)]) => {
                        let identity = IdentityKey {
                            issuer: issuer.clone(),
                            subject: subject.clone(),
                        };
                        identity.validate()?;
                        Ok(Some(identity))
                    }
                    _ => Err(Error::Invalid("actor identity record is invalid")),
                }
            })
            .collect()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandleMatches {
    pub items: Vec<ActorSummary>,
    pub truncated: bool,
}

pub(crate) fn validate_handle_prefix(prefix: &str) -> std::result::Result<(), &'static str> {
    if prefix.len() > 39
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Username prefix must contain at most 39 lowercase letters, digits or hyphens");
    }
    Ok(())
}

pub(crate) struct CompleteHandles;
impl Read for CompleteHandles {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 8;
    type Input = String;
    type Output = HandleMatches;
    fn execute(context: &mut Transaction<'_>, prefix: Self::Input) -> Result<Self::Output> {
        validate_handle_prefix(&prefix).map_err(Error::Invalid)?;
        // ASCII handles sort below '~'. The indexed range bounds work even
        // when the directory is large; completion never scans display names.
        let upper = format!("{prefix}~");
        let sql = format!(
            "SELECT {COLUMNS} FROM profiles p WHERE p.handle >= ? AND p.handle < ? ORDER BY p.handle LIMIT 21"
        );
        let rows = context.query(&sql, &[SqlValue::Text(prefix), SqlValue::Text(upper)])?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| {
                let mut actor = ActorProfile::try_from(row.as_slice())?;
                actor.avatar = avatar::load(context, actor.id)?;
                Ok(actor.into())
            })
            .collect::<Result<Vec<_>>>()?;
        let truncated = items.len() > 20;
        items.truncate(20);
        Ok(HandleMatches { items, truncated })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MentionSearch {
    pub issuer: String,
    pub search: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MentionCandidate {
    pub subject: String,
    pub actor: ActorSummary,
}

pub(crate) struct MentionCandidates;
impl Read for MentionCandidates {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 22;
    type Input = MentionSearch;
    type Output = Vec<MentionCandidate>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if input.issuer.is_empty()
            || input.issuer.chars().count() > 4096
            || input.issuer.chars().any(char::is_control)
            || input.search.chars().count() > 240
            || input.search.chars().any(char::is_control)
        {
            return Err(Error::Invalid("Mention candidate search is invalid"));
        }
        // Authorizing each possible group member requires a durable session
        // read. Rank exact and prefix handles before broad display-name hits.
        let rows = context.query(
            "SELECT a.subject, p.id, p.handle, p.name FROM profiles p JOIN accounts a ON a.id = p.id
             WHERE a.issuer = ?1 AND (instr(p.handle, ?2) > 0 OR instr(lower(p.name), ?2) > 0)
             ORDER BY (p.handle = ?2) DESC, (substr(p.handle, 1, length(?2)) = ?2) DESC, p.handle
             LIMIT 64",
            &[SqlValue::Text(input.issuer), SqlValue::Text(input.search)],
        )?;
        rows.rows
            .iter()
            .map(|row| {
                let [
                    SqlValue::Text(subject),
                    SqlValue::Blob(id),
                    SqlValue::Text(handle),
                    SqlValue::Text(name),
                ] = row.as_slice()
                else {
                    return Err(Error::Invalid("Mention profile is invalid"));
                };
                let actor_id = Uuid::from_slice(id).map_err(Error::sdk)?;
                let actor_avatar = avatar::load(context, actor_id)?;
                Ok(MentionCandidate {
                    subject: subject.clone(),
                    actor: ActorSummary {
                        id: actor_id,
                        handle: handle.clone(),
                        name: name.clone(),
                        avatar: actor_avatar,
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
