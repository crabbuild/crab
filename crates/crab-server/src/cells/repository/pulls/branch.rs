use super::*;

#[derive(Deserialize, Serialize)]
pub(crate) struct Reference {
    pub name: String,
    pub fork: Option<uuid::Uuid>,
}

pub(crate) struct InUse;

impl Read for InUse {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 447;
    type Input = Reference;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        // Ref bindings and lifecycle state are projected in the same Cell
        // transaction as creation, retargeting, and reopening.
        search::index::ensure_ready(context)?;
        let head = qualified_head(&input.name, input.fork);
        let base = input.fork.is_none().then_some(input.name);
        let rows = context.query(
            "SELECT 1 FROM discussion_index d JOIN discussion_index_pull_sources s ON s.document=d.rowid
                WHERE d.kind=1 AND d.state=? AND (s.head_ref=? OR s.base_ref=?) LIMIT 1",
            &[
                SqlValue::Integer(i64::from(PullState::Open.code())),
                SqlValue::Blob(head.into_bytes()),
                base.map_or(SqlValue::Null, SqlValue::Text),
            ],
        )?;
        Ok(!rows.rows.is_empty())
    }
}

pub(super) fn qualified_head(reference: &str, fork: Option<uuid::Uuid>) -> String {
    // A fork ref must never match a same-named upstream ref.
    fork.map_or_else(
        || reference.to_owned(),
        |fork| format!("{fork}:{reference}"),
    )
}
