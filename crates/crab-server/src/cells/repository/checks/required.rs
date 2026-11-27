use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CheckRunRecord, ListCheckRuns, ListCheckRunsInput, MAX_CHECK_RUNS_PER_COMMIT, validate_commit,
};
use crate::cells::{
    Result,
    application::{Read, Transaction},
    repository::{CLASS, CommitStatusRecord, ListCommitStatuses, integrations},
};

#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    pub head: String,
    pub candidate: Option<String>,
    pub contexts: Vec<String>,
    pub connections: Vec<Uuid>,
    pub queued_candidate: bool,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Reports {
    pub statuses: Vec<Status>,
    pub runs: Vec<Run>,
    pub connections: Vec<integrations::RequiredConnection>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Status {
    pub context: String,
    pub state: u8,
    pub updated_at: u64,
    pub evidence: Option<Evidence>,
}

impl Status {
    pub(super) fn from_record(record: CommitStatusRecord, include_evidence: bool) -> Self {
        Self {
            context: record.context,
            state: record.state,
            updated_at: record.created_at_ms,
            evidence: include_evidence.then_some(Evidence {
                author: record.author.name,
                description: record.description,
                target_url: record.target_url,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Run {
    pub name: String,
    pub number: u64,
    pub status: u8,
    pub conclusion: Option<u8>,
    pub updated_at: u64,
    pub evidence: Option<Evidence>,
}

impl Run {
    pub(in crate::cells::repository) fn from_record(
        record: CheckRunRecord,
        include_evidence: bool,
    ) -> Self {
        Self {
            name: record.name,
            number: record.number,
            status: record.status,
            conclusion: record.conclusion,
            updated_at: record.updated_at_ms,
            evidence: include_evidence.then_some(Evidence {
                author: record.author.name,
                description: Some(record.output_title),
                target_url: record.details_url,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Evidence {
    pub author: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub head: Reports,
    pub candidate: Option<Reports>,
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 407;
    type Input = Input;
    type Output = Snapshot;

    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<Snapshot> {
        let names: HashSet<_> = input
            .contexts
            .iter()
            .map(|name| name.to_lowercase())
            .collect();
        // One transaction keeps concurrent writes from combining reports that
        // never passed together, including reports for a tested candidate.
        // The candidate view shows candidate evidence. Keep only decision fields
        // for the head in that read so valid long reports fit the reply limit.
        let head = reports(
            context,
            &input.head,
            &names,
            &input.connections,
            false,
            input.candidate.is_none(),
        )?;
        let candidate = input
            .candidate
            .as_deref()
            .map(|oid| {
                reports(
                    context,
                    oid,
                    &names,
                    &input.connections,
                    input.queued_candidate,
                    true,
                )
            })
            .transpose()?;
        Ok(Snapshot { head, candidate })
    }
}

fn reports(
    context: &mut Transaction<'_>,
    oid: &str,
    names: &HashSet<String>,
    connections: &[Uuid],
    queued_candidate: bool,
    include_evidence: bool,
) -> Result<Reports> {
    validate_commit(oid)?;
    let (statuses, runs) = if names.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let statuses = ListCommitStatuses::execute(context, oid.to_owned())?
            .statuses
            .into_iter()
            .filter(|status| names.contains(&status.context.to_lowercase()))
            .map(|status| Status::from_record(status, include_evidence))
            .collect();
        let mut seen = HashSet::new();
        let runs = ListCheckRuns::execute(
            context,
            ListCheckRunsInput {
                oid: oid.to_owned(),
                before: None,
                limit: MAX_CHECK_RUNS_PER_COMMIT as u8,
                scans_only: false,
            },
        )?
        .runs
        .into_iter()
        .filter(|run| {
            let name = run.name.to_lowercase();
            integrations::github_connection(&run.author.issuer, &run.author.subject).is_none()
                && names.contains(&name)
                && seen.insert(name)
        })
        .map(|run| Run::from_record(run, include_evidence))
        .collect();
        (statuses, runs)
    };
    Ok(Reports {
        statuses,
        runs,
        connections: integrations::required_connections(
            context,
            oid,
            connections,
            queued_candidate,
            include_evidence,
        )?,
    })
}
