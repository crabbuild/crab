use super::*;
use crate::cells::planning as ledger;
use serde_json::{Value, json};

pub(super) fn project_snapshot(project: &Project) -> Value {
    json!({"definition":project.definition,"archived":project.archived})
}
pub(super) fn item_target(context: &Transaction<'_>, item: &Item) -> Result<ledger::Target> {
    let source = item
        .subject
        .map(|subject| {
            let identity = context.query(
                "SELECT repository_uuid FROM repository_identity WHERE singleton=1",
                &[],
            )?;
            let Some(SqlValue::Blob(bytes)) = identity.rows.first().and_then(|row| row.first())
            else {
                return Err(Error::Invalid(
                    "project history repository identity is missing",
                ));
            };
            Ok(ledger::Source {
                repository: Uuid::from_slice(bytes).map_err(Error::sdk)?,
                subject,
            })
        })
        .transpose()?;
    Ok(ledger::Target::Item {
        id: item.id,
        source,
    })
}
pub(super) fn append(
    context: &Transaction<'_>,
    actor: &RepositoryAuthor,
    project: &Project,
    target: ledger::Target,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    ledger::append(
        context,
        ledger::Event {
            project: project.id,
            version: project.version,
            actor: ledger::Actor::Repository {
                author: actor.clone(),
            },
            at_ms: context.now_ms(),
            target,
            changes: ledger::changes(before, after),
        },
    )
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryQuery {
    pub project: Uuid,
    pub before: Option<u64>,
    pub after: Option<u64>,
}
pub(crate) struct GetHistory;
impl Read for GetHistory {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 398;
    type Input = HistoryQuery;
    type Output = Option<ledger::History>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        if load(context, input.project)?.is_none() {
            return Ok(None);
        }
        ledger::history(context, input.project, input.before, input.after).map(Some)
    }
}
