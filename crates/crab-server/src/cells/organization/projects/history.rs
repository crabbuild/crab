use super::*;
use serde_json::json;

pub(super) use crate::cells::planning::item_snapshot;
pub(super) fn project_snapshot(project: &Project) -> Value {
    json!({"definition":project.definition,"properties":project.properties,"archived":project.archived})
}
pub(super) fn append(
    context: &Transaction<'_>,
    input: &Request,
    project: &Project,
    target: crate::cells::planning::Target,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    crate::cells::planning::append(
        context,
        crate::cells::planning::Event {
            project: project.id,
            version: project.version,
            actor: crate::cells::planning::Actor::Account { id: input.actor },
            at_ms: context.now_ms(),
            target,
            changes: crate::cells::planning::changes(before, after),
        },
    )
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryQuery {
    pub actor: Uuid,
    pub project: Uuid,
    pub before: Option<u64>,
    pub after: Option<u64>,
}
pub(crate) struct GetHistory;
impl Read for GetHistory {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 34;
    type Input = HistoryQuery;
    type Output = Option<crate::cells::planning::History>;
    fn execute(context: &mut Transaction<'_>, input: HistoryQuery) -> Result<Self::Output> {
        if load(context, input.project)?.is_none() {
            return Ok(None);
        }
        crate::cells::planning::history(context, input.project, input.before, input.after).map(Some)
    }
}
