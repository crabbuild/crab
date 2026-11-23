use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BranchProtection,
    api::{
        checks::{self, CheckConclusion, CheckStatus},
        statuses::{self, StatusState},
    },
    app::{self, Result},
    cells::repository::required_checks::{Get, Input, Reports, Run, Status},
    repository::Repository,
    server::Server,
};

pub(super) struct RequiredCheck {
    context: String,
    report: Report,
    additional_report: Option<Report>,
    pub(super) integration: Option<Uuid>,
}

impl RequiredCheck {
    pub(super) fn state(&self) -> Option<StatusState> {
        self.report.state
    }

    pub(super) fn view(&self, oid: &str) -> Value {
        #[derive(Serialize)]
        struct View<'a> {
            context: &'a str,
            head_sha: &'a str,
            integration_id: Option<Uuid>,
            #[serde(flatten)]
            report: &'a Report,
            #[serde(skip_serializing_if = "Option::is_none")]
            additional_report: Option<&'a Report>,
        }
        json!(View {
            context: &self.context,
            head_sha: oid,
            integration_id: self.integration,
            report: &self.report,
            additional_report: self.additional_report.as_ref(),
        })
    }
}

#[derive(Serialize)]
struct Report {
    state: Option<StatusState>,
    description: Option<String>,
    target_url: Option<String>,
    author: Option<String>,
    updated_at: Option<u64>,
    run_id: Option<u64>,
    run_status: Option<CheckStatus>,
    conclusion: Option<CheckConclusion>,
    provider: &'static str,
}

impl Report {
    fn waiting(provider: &'static str, description: Option<&str>) -> Self {
        Self {
            state: None,
            description: description.map(str::to_owned),
            target_url: None,
            author: None,
            updated_at: None,
            run_id: None,
            run_status: None,
            conclusion: None,
            provider,
        }
    }

    fn status(status: &Status) -> Result<Self> {
        let evidence = status.evidence.as_ref();
        Ok(Self {
            state: Some(statuses::status_state(status.state)?),
            description: evidence.and_then(|evidence| evidence.description.clone()),
            target_url: evidence.and_then(|evidence| evidence.target_url.clone()),
            author: evidence.map(|evidence| evidence.author.clone()),
            updated_at: Some(status.updated_at),
            ..Self::waiting("crab_status", None)
        })
    }

    fn run(run: &Run, integration: bool) -> Result<Self> {
        let status = checks::check_status(run.status)?;
        let conclusion = run.conclusion.map(checks::check_conclusion).transpose()?;
        let evidence = run.evidence.as_ref();
        Ok(Self {
            state: Some(checks::requirement_state(integration, status, conclusion)),
            description: evidence.and_then(|evidence| evidence.description.clone()),
            target_url: evidence.and_then(|evidence| evidence.target_url.clone()),
            author: evidence.map(|evidence| evidence.author.clone()),
            updated_at: Some(run.updated_at),
            run_id: Some(run.number),
            run_status: Some(status),
            conclusion,
            provider: if integration {
                "github_actions"
            } else {
                "crab_check_run"
            },
        })
    }

    fn priority(&self) -> (u8, Option<u64>) {
        let blocking = match self.state {
            Some(StatusState::Error | StatusState::Failure) => 2,
            Some(StatusState::Pending) | None => 1,
            Some(StatusState::Success) => 0,
        };
        (blocking, self.updated_at)
    }
}

pub(super) async fn load(
    server: &Server,
    repo: &Repository,
    rule: &BranchProtection,
    head: &str,
    candidate: Option<&str>,
    queued_candidate: bool,
) -> Result<(Vec<RequiredCheck>, Option<Vec<RequiredCheck>>)> {
    if rule.required_checks.is_empty() && rule.required_integrations.is_empty() {
        return Ok((vec![], candidate.map(|_| vec![])));
    }
    let cell = app::repository_cell(server, repo)?;
    let reports = app::query_output(
        cell.client
            .query::<Get>(
                cell.target,
                None,
                Input {
                    head: head.to_owned(),
                    candidate: candidate.map(str::to_owned),
                    contexts: rule.required_checks.clone(),
                    connections: if crate::api::integrations::shared_key(server).is_some() {
                        rule.required_integrations.clone()
                    } else {
                        Vec::new()
                    },
                    queued_candidate,
                },
            )
            .await,
    )?;
    Ok((
        evaluate(rule, reports.head)?,
        reports
            .candidate
            .map(|reports| evaluate(rule, reports))
            .transpose()?,
    ))
}

fn evaluate(rule: &BranchProtection, reports: Reports) -> Result<Vec<RequiredCheck>> {
    let mut required =
        Vec::with_capacity(rule.required_checks.len() + rule.required_integrations.len());
    for context in &rule.required_checks {
        let status = reports
            .statuses
            .iter()
            .find(|status| statuses::same_context(&status.context, context))
            .map(Report::status)
            .transpose()?;
        let run = reports
            .runs
            .iter()
            .find(|run| statuses::same_context(&run.name, context))
            .map(|run| Report::run(run, false))
            .transpose()?;
        // All reported sources must pass. Keep the strongest blocker in the
        // published summary fields and expose the other source as evidence.
        let (report, additional_report) = match (status, run) {
            (Some(status), Some(run)) if run.priority() >= status.priority() => (run, Some(status)),
            (Some(status), run) => (status, run),
            (None, Some(run)) => (run, None),
            (None, None) => (Report::waiting("crab_status_or_check_run", None), None),
        };
        required.push(RequiredCheck {
            context: context.clone(),
            report,
            additional_report,
            integration: None,
        });
    }
    for id in &rule.required_integrations {
        let connection = reports
            .connections
            .iter()
            .find(|connection| connection.id == *id);
        let (context, report) = if let Some(run) =
            connection.and_then(|connection| connection.run.as_ref())
        {
            (run.name.clone(), Report::run(run, true)?)
        } else if let Some(connection) = connection {
            (
                connection.name.clone(),
                Report::waiting(
                    "github_actions",
                    Some("Waiting for GitHub Actions on this commit."),
                ),
            )
        } else {
            (
                "GitHub Actions workflow".into(),
                Report::waiting(
                    "github_actions",
                    Some(
                        "GitHub Actions connection is unavailable. Review the branch protection rule.",
                    ),
                ),
            )
        };
        required.push(RequiredCheck {
            context,
            report,
            additional_report: None,
            integration: Some(*id),
        });
    }
    Ok(required)
}
