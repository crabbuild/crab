use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    api::{catalog, discussion_attachments},
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        RepositoryCell,
        directory::IdentityKey,
        repository::{
            GetIssue,
            issues::transfer::{
                Capture, CaptureInput, CaptureOutcome, Frame, Progress, Source, Stage, StageInput,
                StageOutcome,
                publication::{
                    GetMove, Prepare, PrepareInput, PrepareOutcome, Publish, PublishOutcome,
                    Retire, RetireOutcome,
                },
            },
        },
    },
    repository::{Repository, settings},
    server::Server,
};

#[cfg(test)]
mod reactions_tests;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Search {
    q: String,
}

#[derive(Serialize)]
pub(super) struct Candidate {
    id: Uuid,
    owner: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Submission {
    request_id: Uuid,
    version: u64,
    destination: Uuid,
}

fn same_owner(source: &Repository, target: &Repository) -> bool {
    source.id != target.id
        && match source.organization {
            Some(id) => target.organization == Some(id),
            None => target.organization.is_none() && source.config.owner == target.config.owner,
        }
}

async fn moderate(server: &Server, actor: &Principal, repo: &Repository) -> Result<()> {
    if !app::membership(server, actor, repo)
        .await?
        .principal
        .can_moderate(repo.id)
    {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub(super) async fn destinations(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(input): Query<Search>,
) -> Result<Json<Vec<Candidate>>> {
    let source = app::repository(&server, &principal, &(owner, name))?;
    moderate(&server, &principal, &source).await?;
    let browser = principal
        .related_catalog(source.id)
        .ok_or(Error::NotFound)?;
    let number = app::number(number)?;
    let cell = app::repository_cell(&server, &source)?;
    let issue = app::query_output(
        cell.client
            .query::<GetIssue>(cell.target, None, number)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    if issue.state != 0 {
        return Err(Error::Invalid("Only open issues can be transferred"));
    }
    if input.q.len() > 100 || input.q.chars().any(char::is_control) {
        return Err(Error::Invalid("Enter a repository name of up to 100 bytes"));
    }
    let query = input.q.trim().to_ascii_lowercase();
    let mut available = catalog::visible_matching(&server, &browser, None, |repo| {
        same_owner(&source, repo) && repo.config.name.to_ascii_lowercase().contains(&query)
    })
    .await?;
    available.sort_unstable_by(|a, b| a.repository.config.name.cmp(&b.repository.config.name));
    let mut items = Vec::new();
    for entry in available
        .into_iter()
        .filter(|entry| entry.principal.can_moderate(entry.repository.id))
    {
        let repo = entry.repository;
        if settings::load_lifecycle(&server, &repo).await?.archived {
            continue;
        }
        items.push(Candidate {
            id: repo.id,
            owner: repo.config.owner.clone(),
            name: repo.config.name.clone(),
        });
        if items.len() == 30 {
            break;
        }
    }
    moderate(&server, &principal, &source).await?;
    Ok(Json(items))
}

pub(super) async fn submit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(input): Json<Submission>,
) -> Result<(StatusCode, Json<Value>)> {
    let source = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::account(&principal)?;
    let number = app::number(number)?;
    app::number(input.version)?;
    if input.request_id.is_nil() || input.destination.is_nil() {
        return Err(Error::Invalid("Choose a destination repository"));
    }
    let target = server
        .repositories
        .by_id(input.destination)
        .filter(|repo| same_owner(&source, repo))
        .ok_or(Error::NotFound)?;
    let recipient = principal
        .related_read(source.id, target.id)
        .ok_or(Error::NotFound)?;
    let source_cell = app::repository_cell(&server, &source)?;
    let target_cell = app::repository_cell(&server, &target)?;
    let transfer = Transfer {
        server: &server,
        source: &source,
        target: &target,
        principal: &principal,
        recipient,
        source_cell,
        target_cell,
        actor,
        input,
    };
    transfer.authorize().await?;
    let prior = app::query_output(
        transfer
            .source_cell
            .client
            .query::<GetMove>(source.id, None, number)
            .await,
    )?;
    if let Some(decision) = prior {
        if !decision.matches_request(
            transfer.input.request_id,
            target.id,
            transfer.input.version,
            &transfer.actor,
        ) {
            return Err(Error::RequestConflict);
        }
        return Ok((StatusCode::ACCEPTED, Json(json!({"accepted":true}))));
    }
    let (snapshot, progress) = transfer.stage(number).await?;
    transfer.retire(snapshot, progress).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted":true}))))
}

struct Transfer<'a> {
    server: &'a Server,
    source: &'a Repository,
    target: &'a Repository,
    principal: &'a Principal,
    recipient: Principal,
    source_cell: RepositoryCell,
    target_cell: RepositoryCell,
    actor: IdentityKey,
    input: Submission,
}

impl Transfer<'_> {
    async fn authorize(&self) -> Result<()> {
        moderate(self.server, self.principal, self.source).await?;
        moderate(self.server, &self.recipient, self.target).await?;
        // Local sessions have an implicit grant. Ownership is still bound to
        // both current catalog records, including their incarnation.
        if self.principal.is_local() {
            let catalog = self.server.catalog().ok_or(Error::NotFound)?;
            let (document, _) = catalog
                .load_snapshot()
                .await
                .map_err(|error| Error::Membership(Box::new(error)))?;
            for repo in [self.source, self.target] {
                if !document.repositories.iter().any(|record| {
                    record.id == repo.id
                        && record.owner == repo.config.owner
                        && record.name == repo.config.name
                        && record.organization_id() == repo.organization
                        && record.ownership_epoch == repo.ownership_epoch
                }) {
                    return Err(Error::NotFound);
                }
            }
        }
        Ok(())
    }

    async fn stage(&self, number: u64) -> Result<(Source, Progress)> {
        let mut input = CaptureInput::Start {
            number,
            expected_version: self.input.version,
        };
        loop {
            let frame = match app::query_output(
                self.source_cell
                    .client
                    .query::<Capture>(self.source.id, None, input)
                    .await,
            )? {
                CaptureOutcome::Ready { frame } => *frame,
                CaptureOutcome::Missing => return Err(Error::NotFound),
                CaptureOutcome::Changed => return Err(Error::Conflict),
                CaptureOutcome::Archived => return Err(Error::Archived),
                CaptureOutcome::Closed => {
                    return Err(Error::Invalid("Only open issues can be transferred"));
                }
                CaptureOutcome::LinkedWork => {
                    return Err(Error::Invalid(
                        "This issue has linked work that Crab cannot transfer yet",
                    ));
                }
            };
            let source = frame.source;
            let progress = self.stage_frame(frame).await?;
            let Some(cursor) = progress.next else {
                return Ok((source, progress));
            };
            input = CaptureInput::Continue { source, cursor };
        }
    }

    async fn stage_frame(&self, frame: Frame) -> Result<Progress> {
        // The receiver must claim all IDs before any object store writes.
        let mut input = StageInput {
            request: self.input.request_id,
            destination: self.target.id,
            actor: self.actor.clone(),
            frame,
            copied: BTreeMap::new(),
        };
        // The same exact frame can be sent twice without moving or reopening
        // its Git-independent discussion snapshot.
        let outcome = if input.frame.records.uploads().next().is_none() {
            self.send_frame(input).await?
        } else {
            let initial = self.send_frame(input.clone()).await?;
            if initial == StageOutcome::AwaitingCopies {
                for upload in input.frame.records.uploads() {
                    let id = discussion_attachments::copy_for_issue_transfer(
                        self.server,
                        self.source,
                        self.target,
                        input.request,
                        upload,
                    )
                    .await?;
                    input.copied.insert(id, upload.digest.clone());
                }
                self.send_frame(input).await?
            } else {
                initial
            }
        };
        match outcome {
            StageOutcome::Saved { progress } => Ok(progress),
            StageOutcome::Archived => Err(Error::Archived),
            StageOutcome::Conflict => Err(Error::RequestConflict),
            StageOutcome::Expired => Err(Error::IssueTransferExpired),
            StageOutcome::OutOfOrder | StageOutcome::AwaitingCopies => Err(Error::CellContract(
                "Issue transfer staging returned unexpected progress",
            )),
        }
    }

    async fn send_frame(&self, input: StageInput) -> Result<StageOutcome> {
        app::command_output(
            self.target_cell
                .client
                .command::<Stage>(self.target.id, app::mutation_identity()?, input)
                .await,
        )
    }

    async fn retire(&self, source: Source, progress: Progress) -> Result<()> {
        let (first, second) = if self.source.id < self.target.id {
            (self.source, self.target)
        } else {
            (self.target, self.source)
        };
        settings::with_ownership_lease(self.server, first, |first_cancel| async move {
            if self.source.organization.is_some() {
                // Both repositories share one Organization policy lease.
                self.commit(source, progress, &[first_cancel]).await
            } else {
                settings::with_ownership_lease(self.server, second, |second_cancel| async move {
                    self.commit(source, progress, &[first_cancel, second_cancel])
                        .await
                })
                .await
            }
        })
        .await
    }

    async fn commit(
        &self,
        source: Source,
        progress: Progress,
        cancellation: &[CancellationToken],
    ) -> Result<()> {
        self.authorize().await?;
        if cancellation.iter().any(CancellationToken::is_cancelled) {
            return Err(Error::MergeBusy);
        }
        let prepared = app::command_output(
            self.target_cell
                .client
                .command::<Prepare>(
                    self.target.id,
                    app::mutation_identity()?,
                    PrepareInput {
                        request: self.input.request_id,
                        source,
                        actor: self.actor.clone(),
                        commitment: progress.commitment,
                        ownership_epoch: self.target.ownership_epoch,
                    },
                )
                .await,
        )?;
        let reservation = match prepared {
            PrepareOutcome::Reserved { reservation } => reservation,
            PrepareOutcome::Archived => return Err(Error::Archived),
            PrepareOutcome::Expired => return Err(Error::IssueTransferExpired),
            PrepareOutcome::Conflict => return Err(Error::RequestConflict),
            PrepareOutcome::Missing | PrepareOutcome::Incomplete => {
                return Err(Error::CellContract(
                    "Issue transfer reservation has no complete staging job",
                ));
            }
        };
        self.authorize().await?;
        if cancellation.iter().any(CancellationToken::is_cancelled) {
            return Err(Error::MergeBusy);
        }
        let retired = app::command_output(
            self.source_cell
                .client
                .command::<Retire>(self.source.id, app::mutation_identity()?, reservation)
                .await,
        )?;
        let decision = match retired {
            RetireOutcome::Committed { decision } => decision,
            RetireOutcome::Archived => return Err(Error::Archived),
            RetireOutcome::Conflict => return Err(Error::RequestConflict),
            RetireOutcome::Changed => return Err(Error::Conflict),
            RetireOutcome::Missing => return Err(Error::NotFound),
            RetireOutcome::Expired => return Err(Error::IssueTransferExpired),
        };
        // The source alarm can finish this exact move if either the browser
        // or this server loses the acknowledgement after retirement.
        let destination = decision.destination();
        let publication = app::command_output(
            self.target_cell
                .client
                .command::<Publish>(self.target.id, app::mutation_identity()?, decision)
                .await,
        );
        if !matches!(publication, Ok(PublishOutcome::Published { destination: actual }) if actual == destination)
        {
            tracing::warn!(repository_id = %self.target.id, issue = destination.number, ?publication, "Issue transfer will recover publication");
        }
        Ok(())
    }
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI with native Cells"]
async fn browser_moves_an_issue_to_a_moderated_repository() {
    use crate::test_support::http::{Harness, browser::Browser};
    use reqwest::Method;

    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    let created: Value = h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Transfer the release notes","body":"Release notes stay attached to this issue."}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    let number = created["number"].as_u64().unwrap();
    let old = format!("{}/team/private/issues/{number}", h.origin);
    let browser = Browser::new("issue-transfer");
    browser.run(&["open", &old]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Transfer issue"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Transfer issue",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", r#"select[aria-label="Destination repository"]"#])
        .await;
    let destination = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    browser
        .run(&[
            "select",
            r#"select[aria-label="Destination repository"]"#,
            &destination.id.to_string(),
        ])
        .await;
    browser
        .run(&[
            "click",
            "form:has(#issue-transfer-search) button[type=submit]",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/team/library/issues/1*"])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Release notes stay attached to this issue.",
        ])
        .await;
    browser.run(&["close"]).await;
    h.close().await;
}
