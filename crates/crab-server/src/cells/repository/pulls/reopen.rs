use super::*;

#[derive(Deserialize, Serialize)]
pub(crate) struct Branches {
    pub number: u64,
    pub base: String,
    pub head: String,
    pub fork: Option<uuid::Uuid>,
    pub issuer: String,
    pub subject: String,
}

pub(crate) struct ForReopen;

impl Read for ForReopen {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 446;
    type Input = Vec<(u64, u64)>;
    type Output = Vec<Branches>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if input.len() > crate::cells::repository::collaboration::bulk::MAX_ITEMS {
            return Err(crate::cells::Error::Invalid(
                "Too many pull requests to reopen",
            ));
        }
        let mut branches = Vec::with_capacity(input.len());
        for (number, version) in input {
            let rows = context.query(
                "SELECT base_ref,head_ref,author_issuer,author_subject FROM repository_pulls WHERE number=? AND version=? AND state=?",
                &[integer(number)?, integer(version)?, SqlValue::Integer(i64::from(PullState::Closed.code()))],
            )?;
            if let Some(row) = rows.rows.first() {
                branches.push(Branches {
                    number,
                    base: result_text(row, 0)?,
                    head: result_text(row, 1)?,
                    fork: storage::head_repository(context, number)?,
                    issuer: result_text(row, 2)?,
                    subject: result_text(row, 3)?,
                });
            }
        }
        Ok(branches)
    }
}
