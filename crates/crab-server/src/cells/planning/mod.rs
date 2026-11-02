//! Planning revisions and append-only changes shared by native project owners.

use super::repository::{
    RepositoryAuthor, collaboration::Subject, collaboration::lifecycle::LifecycleStatus,
};
use super::{
    Error, Result,
    application::{SqlValue, Transaction},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const KEY: &str = "planning/v2/state";
const FRAME: usize = 64;
const MAX_VERSION: u64 = 9_007_199_254_740_991;

#[derive(Deserialize, Serialize)]
enum State {
    Migrating {
        after: Option<(Uuid, Uuid)>,
        copied: u64,
        version: u64,
    },
    Ready,
}

#[derive(Clone, Copy)]
pub(crate) enum Owner {
    Repository,
    Organization,
}
impl Owner {
    fn table(self) -> &'static str {
        match self {
            Self::Repository => "repository_project_items",
            Self::Organization => "organization_project_items",
        }
    }
    fn id(self, id: Uuid) -> SqlValue {
        match self {
            Self::Repository => text(id),
            Self::Organization => SqlValue::Blob(id.as_bytes().to_vec()),
        }
    }
}
fn text(id: Uuid) -> SqlValue {
    SqlValue::Text(id.to_string())
}
fn integer(value: u64) -> Result<SqlValue> {
    i64::try_from(value)
        .map(SqlValue::Integer)
        .map_err(|_| Error::Invalid("planning revision exceeds its range"))
}
fn uint(value: &SqlValue) -> Result<u64> {
    match value {
        SqlValue::Integer(value) => {
            u64::try_from(*value).map_err(|_| Error::Invalid("planning revision is negative"))
        }
        _ => Err(Error::Invalid("planning revision is invalid")),
    }
}
fn uuid(value: &SqlValue) -> Result<Uuid> {
    // Repository UUID columns are TEXT; Organization UUID columns are BLOB.
    let id = match value {
        SqlValue::Text(value) => Uuid::parse_str(value),
        SqlValue::Blob(value) => Uuid::from_slice(value),
        _ => return Err(Error::Invalid("planning source UUID is invalid")),
    }
    .map_err(Error::sdk)?;
    if id.is_nil() {
        return Err(Error::Invalid("planning source UUID is nil"));
    }
    Ok(id)
}
pub(crate) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(KEY, &State::Ready)
}
pub(crate) fn status(context: &Transaction<'_>) -> Result<LifecycleStatus> {
    Ok(match context.document::<State>(KEY)? {
        None => LifecycleStatus::Outdated,
        Some(State::Migrating {
            copied, version, ..
        }) => LifecycleStatus::Migrating { version, copied },
        Some(State::Ready) => LifecycleStatus::Ready,
    })
}
pub(crate) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    if status(context)? != LifecycleStatus::Ready {
        return Err(Error::SchemaUnavailable);
    }
    Ok(())
}
pub(crate) fn prepare(
    context: &Transaction<'_>,
    owner: Owner,
    expected: u64,
) -> Result<LifecycleStatus> {
    let state = context.document::<State>(KEY)?;
    let (after, copied, version) = match state {
        Some(State::Ready) => return Ok(LifecycleStatus::Ready),
        Some(State::Migrating {
            after,
            copied,
            version,
        }) if version == expected => (after, copied, version),
        None if expected == 0 => {
            context.install_schema(include_str!("schema.sql"))?;
            (None, 0, 0)
        }
        _ => return Err(Error::StaleRead),
    };
    let (project, item) = after.unwrap_or((Uuid::nil(), Uuid::nil()));
    let rows = context.query(&format!("SELECT project,id FROM {} WHERE project>? OR (project=? AND id>?) ORDER BY project,id LIMIT {FRAME}", owner.table()), &[owner.id(project), owner.id(project), owner.id(item)])?;
    for row in &rows.rows {
        let [project, item] = row.as_slice() else {
            return Err(Error::Invalid("planning preparation source is invalid"));
        };
        context.query(
            "INSERT INTO planning_item_revisions(project,item,version) VALUES(?,?,1)",
            &[text(uuid(project)?), text(uuid(item)?)],
        )?;
    }
    let new = if rows.rows.len() < FRAME {
        State::Ready
    } else {
        let last = rows
            .rows
            .last()
            .ok_or(Error::Invalid("planning preparation cursor is missing"))?;
        State::Migrating {
            after: Some((uuid(&last[0])?, uuid(&last[1])?)),
            copied: copied
                .checked_add(FRAME as u64)
                .ok_or(Error::Invalid("planning preparation count overflowed"))?,
            version: version
                .checked_add(1)
                .ok_or(Error::Invalid("planning preparation version overflowed"))?,
        }
    };
    context.put_document(KEY, &new)?;
    status(context)
}
pub(crate) fn item_version(context: &Transaction<'_>, project: Uuid, item: Uuid) -> Result<u64> {
    context
        .query(
            "SELECT version FROM planning_item_revisions WHERE project=? AND item=?",
            &[text(project), text(item)],
        )?
        .rows
        .first()
        .map(|row| uint(&row[0]))
        .transpose()
        .map(|value| value.unwrap_or(0))
}
pub(crate) fn advance_item(context: &Transaction<'_>, project: Uuid, item: Uuid) -> Result<u64> {
    let version = item_version(context, project, item)?;
    if version >= MAX_VERSION {
        return Err(Error::Invalid("planning item version limit reached"));
    }
    let next = version + 1;
    // Keep the revision after removal so an old create/edit cannot resurrect an
    // item through an absent-row precondition after a concurrent deletion.
    context.query("INSERT INTO planning_item_revisions(project,item,version) VALUES(?,?,?) ON CONFLICT(project,item) DO UPDATE SET version=excluded.version", &[text(project), text(item), integer(next)?])?;
    Ok(next)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Actor {
    Account { id: Uuid },
    Repository { author: RepositoryAuthor },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub repository: Uuid,
    pub subject: Subject,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Target {
    Project,
    Properties,
    Repository { id: Uuid },
    Item { id: Uuid, source: Option<Source> },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FieldChange {
    pub field: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub project: Uuid,
    pub version: u64,
    pub actor: Actor,
    pub at_ms: i64,
    pub target: Target,
    pub changes: Vec<FieldChange>,
}
pub(crate) fn item_snapshot(item: &super::repository::projects::Item) -> Result<Value> {
    let mut value = serde_json::to_value(item)?;
    let fields = value
        .as_object_mut()
        .ok_or(Error::Invalid("planning item is not an object"))?;
    // Source identity lives only in the event target, where HTTP can redact it.
    fields.remove("id");
    fields.remove("subject");
    Ok(value)
}
pub(crate) fn changes(before: Option<Value>, after: Option<Value>) -> Vec<FieldChange> {
    fn visit(
        path: &str,
        before: Option<&Value>,
        after: Option<&Value>,
        changes: &mut Vec<FieldChange>,
    ) {
        if before == after {
            return;
        }
        let old = before.and_then(Value::as_object);
        let new = after.and_then(Value::as_object);
        if (before.is_none() || old.is_some()) && (after.is_none() || new.is_some()) {
            let names = [old, new]
                .into_iter()
                .flatten()
                .flat_map(|object| object.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for name in names {
                let escaped = name.replace('~', "~0").replace('/', "~1");
                visit(
                    &format!("{path}/{escaped}"),
                    old.and_then(|object| object.get(name)),
                    new.and_then(|object| object.get(name)),
                    changes,
                );
            }
        } else {
            changes.push(FieldChange {
                field: path.to_owned(),
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }
    let mut changes = Vec::new();
    visit("", before.as_ref(), after.as_ref(), &mut changes);
    changes
}
pub(crate) fn append(context: &Transaction<'_>, event: Event) -> Result<()> {
    context.query(
        "INSERT INTO planning_history(project,version,data) VALUES(?,?,?)",
        &[
            text(event.project),
            integer(event.version)?,
            SqlValue::Text(serde_json::to_string(&event)?),
        ],
    )?;
    Ok(())
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct History {
    pub items: Vec<Event>,
    pub next: Option<u64>,
}
pub(crate) fn history(
    context: &Transaction<'_>,
    project: Uuid,
    before: Option<u64>,
    after: Option<u64>,
) -> Result<History> {
    if before.is_some() && after.is_some() {
        return Err(Error::Invalid("choose one project history direction"));
    }
    let (comparison, order, cursor) = match after {
        Some(after) => (">", "ASC", after),
        None => ("<", "DESC", before.unwrap_or(MAX_VERSION + 1)),
    };
    let rows = context.query(&format!("SELECT data FROM planning_history WHERE project=? AND version{comparison}? ORDER BY version {order} LIMIT 11"), &[text(project), integer(cursor)?])?;
    let items = rows
        .rows
        .iter()
        .take(10)
        .map(|row| {
            let SqlValue::Text(data) = &row[0] else {
                return Err(Error::Invalid("project history entry is invalid"));
            };
            serde_json::from_str::<Event>(data).map_err(Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let next = (rows.rows.len() > 10)
        .then(|| items.last().map(|event| event.version))
        .flatten();
    Ok(History { items, next })
}
