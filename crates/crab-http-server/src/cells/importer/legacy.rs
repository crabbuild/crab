use serde::{Deserialize, Serialize};

use crate::auth::Identity;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum IssueState {
    #[default]
    Open,
    Closed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Issue {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    #[serde(default)]
    pub label_ids: Vec<u64>,
    #[serde(default)]
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Comment {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Label {
    pub number: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LabelReservation {
    pub request_id: String,
    pub author: Identity,
    pub label: Label,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeletedLabel {
    pub number: u64,
    pub version: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LabelCatalog {
    pub labels: Vec<Label>,
    #[serde(default)]
    pub deleted: Vec<DeletedLabel>,
}
