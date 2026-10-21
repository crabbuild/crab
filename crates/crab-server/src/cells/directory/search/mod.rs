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
            .map(|row| ActorProfile::try_from(row.as_slice()).map(Into::into))
            .collect::<Result<Vec<_>>>()?;
        let truncated = items.len() > 20;
        items.truncate(20);
        Ok(HandleMatches { items, truncated })
    }
}

#[cfg(test)]
mod tests;
