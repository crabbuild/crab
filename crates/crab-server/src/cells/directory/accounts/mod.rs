use super::*;

pub(super) fn ensure(context: &Transaction<'_>, identity: &IdentityKey) -> Result<(Uuid, bool)> {
    identity.validate()?;
    let rows = context.query(
        "SELECT id FROM accounts WHERE issuer = ? AND subject = ?",
        &[
            SqlValue::Text(identity.issuer.clone()),
            SqlValue::Text(identity.subject.clone()),
        ],
    )?;
    if let Some(row) = rows.rows.first() {
        let [SqlValue::Blob(id)] = row.as_slice() else {
            return Err(Error::Invalid("account identity record is invalid"));
        };
        return Ok((Uuid::from_slice(id).map_err(Error::sdk)?, false));
    }
    let id = Uuid::now_v7();
    context.query(
        "INSERT INTO accounts(id, issuer, subject, created_at_ms) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::Blob(id.as_bytes().to_vec()),
            SqlValue::Text(identity.issuer.clone()),
            SqlValue::Text(identity.subject.clone()),
            SqlValue::Integer(context.now_ms()),
        ],
    )?;
    Ok((id, true))
}

pub(crate) struct RegisterAccounts;
impl Mutation for RegisterAccounts {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 11;
    type Input = Vec<IdentityKey>;
    type Output = Vec<Uuid>;
    fn execute(
        context: &mut Transaction<'_>,
        identities: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        if identities.is_empty() || identities.len() > 100 {
            return Err(Error::Invalid(
                "Account registration requires 1–100 qualified identities",
            ));
        }
        let mut created = false;
        let mut ids = Vec::with_capacity(identities.len());
        for identity in identities {
            let (id, inserted) = ensure(context, &identity)?;
            created |= inserted;
            ids.push(id);
        }
        if created {
            advance(context)?;
        }
        Ok(MutationResult::Success(ids))
    }
}

#[cfg(test)]
mod tests;
