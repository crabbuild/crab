use super::collaboration::{Subject, SubjectKind, SubjectSummary, Target, activity};
use super::{
    RepositoryAuthor, Transaction, advance_revision, integer, result_text, result_u64_from_row,
    validate_author, validate_number, validate_title,
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, SqlValue},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod commands;
pub(crate) mod hierarchy;
pub(crate) mod preparation;
mod reads;
mod templates;
pub(crate) use commands::{Change, Command, Outcome, Request};
pub(crate) use reads::{
    CandidateQuery, Candidates, GetProject, ItemQuery, Items, ListProjects, Memberships,
    ProjectQuery,
};
pub(crate) use templates::templates;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Layout {
    Table,
    Board,
    Roadmap,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct View {
    pub id: Uuid,
    pub name: String,
    pub layout: Layout,
    pub query: String,
    pub status: Option<String>,
    pub hide_closed: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FieldKind {
    Text,
    Number,
    Date,
    Select { options: Vec<String> },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Field {
    pub id: Uuid,
    pub name: String,
    pub definition: FieldKind,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    pub name: String,
    pub description: String,
    pub readme: String,
    pub statuses: Vec<String>,
    pub fields: Vec<Field>,
    pub views: Vec<View>,
    pub is_template: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Project {
    pub id: Uuid,
    pub definition: Definition,
    pub version: u64,
    pub archived: bool,
    pub created_at: u64,
    pub updated_at: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub id: Uuid,
    pub subject: Option<Subject>,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: u8,
    pub start: Option<String>,
    pub end: Option<String>,
    pub values: BTreeMap<Uuid, String>,
}
fn text_valid(value: &str, limit: usize, required: bool) -> bool {
    (!required || !value.is_empty() && value.trim() == value)
        && value.chars().count() <= limit
        && !value.chars().any(char::is_control)
}
fn date_valid(value: &str) -> bool {
    value.len() == 10
        && time::format_description::parse("[year]-[month]-[day]")
            .ok()
            .and_then(|format| time::Date::parse(value, &format).ok())
            .is_some()
}
fn names_valid(names: &[String], maximum: usize) -> bool {
    !names.is_empty()
        && names.len() <= maximum
        && names.iter().all(|s| text_valid(s, 60, true))
        && names.iter().collect::<BTreeSet<_>>().len() == names.len()
}
impl Definition {
    fn valid(&self) -> bool {
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= 64 * 1024)
            && text_valid(&self.name, 100, true)
            && text_valid(&self.description, 1000, false)
            && self.readme.len() <= 32 * 1024
            && !self.readme.contains('\0')
            && names_valid(&self.statuses, 12)
            && self.fields.len() <= 20
            && self
                .fields
                .iter()
                .map(|f| f.id)
                .collect::<BTreeSet<_>>()
                .len()
                == self.fields.len()
            && self.fields.iter().all(|f| {
                !f.id.is_nil()
                    && text_valid(&f.name, 60, true)
                    && match &f.definition {
                        FieldKind::Select { options } => names_valid(options, 20),
                        _ => true,
                    }
            })
            && !self.views.is_empty()
            && self.views.len() <= 12
            && self
                .views
                .iter()
                .map(|v| v.id)
                .collect::<BTreeSet<_>>()
                .len()
                == self.views.len()
            && self.views.iter().all(|v| {
                !v.id.is_nil()
                    && text_valid(&v.name, 60, true)
                    && text_valid(&v.query, 256, false)
                    && v.status.as_ref().is_none_or(|s| self.statuses.contains(s))
            })
    }
}
impl Item {
    fn valid(&self, definition: &Definition) -> bool {
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= 24 * 1024)
            && !self.id.is_nil()
            && definition.statuses.contains(&self.status)
            && self.priority <= 4
            && self.start.as_deref().is_none_or(date_valid)
            && self.end.as_deref().is_none_or(date_valid)
            && match (&self.start, &self.end) {
                (Some(a), Some(b)) => a <= b,
                _ => true,
            }
            && self
                .subject
                .is_none_or(|subject| validate_number(subject.number).is_ok())
            && if self.subject.is_some() {
                self.title.is_empty() && self.body.is_empty()
            } else {
                validate_title(&self.title).is_ok()
                    && self.body.len() <= 16 * 1024
                    && !self.body.contains('\0')
            }
            && self.values.iter().all(|(id, value)| {
                definition
                    .fields
                    .iter()
                    .find(|f| f.id == *id)
                    .is_some_and(|field| match &field.definition {
                        FieldKind::Text => text_valid(value, 1000, false),
                        FieldKind::Number => value.parse::<f64>().is_ok_and(f64::is_finite),
                        FieldKind::Date => date_valid(value),
                        FieldKind::Select { options } => options.contains(value),
                    })
            })
    }
}
fn id(value: Uuid) -> SqlValue {
    SqlValue::Text(value.to_string())
}
fn load(context: &Transaction<'_>, project: Uuid) -> Result<Option<Project>> {
    let rows = context.query(
        "SELECT data FROM repository_projects WHERE id=?",
        &[id(project)],
    )?;
    rows.rows
        .first()
        .map(|row| {
            let value: Project = serde_json::from_str(&result_text(row, 0)?)?;
            if value.id != project
                || value.id.is_nil()
                || validate_number(value.version).is_err()
                || value.updated_at < value.created_at
                || !value.definition.valid()
            {
                return Err(Error::Invalid("project record is invalid"));
            }
            Ok(value)
        })
        .transpose()
}
fn save(context: &Transaction<'_>, project: &Project) -> Result<()> {
    context.query("INSERT INTO repository_projects(id,archived,is_template,data) VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET archived=excluded.archived,is_template=excluded.is_template,data=excluded.data", &[id(project.id),SqlValue::Integer(i64::from(project.archived)),SqlValue::Integer(i64::from(project.definition.is_template)),SqlValue::Text(serde_json::to_string(project)?)])?;
    Ok(())
}
fn item(context: &Transaction<'_>, project: Uuid, item: Uuid) -> Result<Option<Item>> {
    let rows = context.query(
        "SELECT data FROM repository_project_items WHERE project=? AND id=?",
        &[id(project), id(item)],
    )?;
    rows.rows
        .first()
        .map(|row| Ok(serde_json::from_str(&result_text(row, 0)?)?))
        .transpose()
}
fn kind(subject: Subject) -> i64 {
    match subject.kind {
        SubjectKind::Issue => 0,
        SubjectKind::Pull => 1,
    }
}
fn target(subject: Subject) -> Target {
    match subject.kind {
        SubjectKind::Issue => Target::Issue {
            number: subject.number,
        },
        SubjectKind::Pull => Target::Pull {
            number: subject.number,
        },
    }
}

#[cfg(test)]
pub(crate) mod tests;
