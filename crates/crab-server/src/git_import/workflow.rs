use std::{path::Path, sync::Arc};

use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    FetchGrant, ImportAccount, ImportContext, ImportError, InternalGit, OrganizationOwner,
    git::{configure_git, default_branch, mirror_refs, mirror_size, push_mirror, run_git},
    jobs, repository_route,
    source::ValidatedImport,
};
use crate::{
    RepositoryAccess, RepositoryMember,
    catalog::{CatalogRecord, CatalogStore, MembershipActor, RepositoryApplicationState},
    repository::{
        MaintenanceTrigger, Repository,
        settings::{RulesetScope, with_ruleset_lease},
    },
    server::Server,
};

const MAX_IMPORT_REFS: usize = 100_000;
const MAX_IMPORT_MIRROR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const IMPORT_MIRROR_RESERVE_MARGIN: u64 = 64 * 1024 * 1024;

pub(super) async fn run_import(
    server: Arc<Server>,
    context: ImportContext,
    job: Arc<jobs::Writer>,
    request: ValidatedImport,
    account: ImportAccount,
    id: Uuid,
) {
    let finished = CancellationToken::new();
    let cancellation = server.cancellation.child_token();
    let run = async {
        let result = async {
            job.update(jobs::State::Running, "Checking import progress")
                .await?;
            let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
            let repository_id = account.jobs(catalog.root()).repository_id(id);
            import_repository(
                &server,
                &context,
                &job,
                &request,
                &account,
                repository_id,
                &cancellation,
            )
            .await
        }
        .await;
        finished.cancel();
        result
    };
    let heartbeat = async {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + jobs::HEARTBEAT,
            jobs::HEARTBEAT,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = finished.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = job.heartbeat().await {
                        tracing::warn!(error = ?error, "Git import status heartbeat failed");
                        cancellation.cancel();
                        break;
                    }
                }
            }
        }
    };
    let (result, ()) = tokio::join!(run, heartbeat);
    let (state, message) = match result {
        Ok(()) => (
            jobs::State::Succeeded,
            "Repository imported; indexing in background",
        ),
        Err(error) => {
            let message = match &error {
                ImportError::Git(_) if request.fork_source.is_some() => {
                    "The fork could not copy Git history or large-file payloads. Check the source and resume this job."
                }
                ImportError::Git(_) => {
                    "The Git source could not be cloned or published; verify the URL, access, token, and server capacity"
                }
                ImportError::Cancelled | ImportError::Timeout => {
                    "The import did not finish. Resume this job to continue."
                }
                ImportError::Busy => "Import storage is busy; retry shortly",
                ImportError::SourceChanged => {
                    "The source refs changed. Restore the original source snapshot before resuming."
                }
                ImportError::DestinationChanged => {
                    "The destination no longer matches this import. Contact your server administrator."
                }
                ImportError::NotFound => {
                    "Organization owner access ended. Share the import link with a current owner to resume."
                }
                ImportError::Ruleset => {
                    "Organization rules restrict the imported refs. Review the organization's rulesets before resuming."
                }
                ImportError::ForkingDisabled => {
                    "The organization or source repository disabled private forks. Restore permission before resuming."
                }
                ImportError::Organization(_) => {
                    "Organization permissions are unavailable. Resume this job to try again."
                }
                ImportError::LeaseLost => {
                    "The import worker lost its lease. Resume this job to continue."
                }
                ImportError::Catalog(crate::catalog::CatalogError::Conflict) => {
                    "A repository with this owner and name already exists"
                }
                _ => "The repository could not be prepared for import",
            };
            tracing::error!(error = ?error, "Git import job failed");
            (jobs::State::Failed, message)
        }
    };
    if let Err(error) = job.update(state, message).await {
        tracing::warn!(error = ?error, "Git import terminal status write failed");
    }
}

async fn import_repository(
    server: &Arc<Server>,
    context: &ImportContext,
    job: &Arc<jobs::Writer>,
    request: &ValidatedImport,
    account: &ImportAccount,
    id: Uuid,
    cancellation: &CancellationToken,
) -> Result<(), ImportError> {
    account.check(server).await?;
    let namespace = account.namespace(&request.owner)?;
    let organization = account.organization;
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    if let Some(record) = document.repositories.iter().find(|record| {
        record.owner.eq_ignore_ascii_case(&request.owner)
            && record.name.eq_ignore_ascii_case(&request.name)
    }) {
        if record.id != id
            || record.owner != request.owner
            || record.name != request.name
            || record.organization_id() != organization.map(|owner| owner.id)
            || record.forked_from != request.fork_source
        {
            return Err(ImportError::Catalog(crate::catalog::CatalogError::Conflict));
        }
        if record.application == RepositoryApplicationState::CellReady {
            // Ready means an earlier attempt verified publication. A normal push may
            // already have advanced the new repository since it became visible.
            return install_destination(server, &catalog, id).await;
        }
        let snapshot = job
            .snapshot()
            .await
            .ok_or(ImportError::DestinationChanged)?;
        let repository = open_destination(&catalog, record, &snapshot).await?;
        let view = repository
            .open_ref_view()
            .await
            .map_err(ImportError::Setup)?;
        if snapshot.matches(view.head(), view.refs()) {
            // Receive verifies reachable payloads before committing refs. A
            // lost reply can finish from this snapshot without rereading them.
            return finish_destination(server, &catalog, job, id, organization, &snapshot).await;
        }
        if !view.refs().is_empty()
            || view.head().strip_prefix("refs/heads/") != Some(&snapshot.default_branch)
        {
            return Err(ImportError::DestinationChanged);
        }
    }

    job.update(jobs::State::Running, "Cloning from Git").await?;
    // Leave the receiver's initial pack reservation available while cloning.
    // A fixed 8 GiB reservation rejects small imports on a constrained node
    // before the mirror size is known, even though the mirror would fit.
    let clone_reservation = MAX_IMPORT_MIRROR_BYTES.min(
        server
            .local_staging
            .available_bytes()
            .saturating_sub(crate::git::receive::IMPORT_RECEIVE_INITIAL_RESERVATION),
    );
    if clone_reservation == 0 {
        return Err(ImportError::Busy);
    }
    let directory = match server
        .local_staging
        .create(clone_reservation, cancellation)
        .await
    {
        Ok(directory) => directory,
        Err(error) => return Err(ImportError::Staging(error)),
    };
    let mirror = directory.path().join("repository.git");
    let fork_source = match request.fork_source {
        None => None,
        Some(id) => {
            let organization = organization.ok_or(ImportError::NotFound)?;
            organization.require_fork(server, &document, id).await?;
            let source = server
                .repositories
                .by_id(id)
                .filter(|source| {
                    source.organization == Some(organization.id) && source.forked_from.is_none()
                })
                .ok_or(ImportError::NotFound)?;
            let current = crate::app::membership(server, &account.principal, &source)
                .await
                .map_err(ImportError::organization)?;
            let grant = context.register(InternalGit::Fetch(Arc::new(FetchGrant {
                route: repository_route(&source),
                principal: current.principal.scoped_to_repository(Some(source.id)),
                writer: Arc::clone(job),
                organization,
            })));
            Some((
                context.destination_url(&source.config.owner, &source.config.name),
                grant,
            ))
        }
    };
    let (source, key) = match &fork_source {
        Some((url, grant)) => (url.as_str(), Some(grant.key.to_hex())),
        None => (request.source.as_str(), None),
    };
    let key = key.as_ref().map(|value| value.as_str());
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--mirror", "--quiet", "--"])
        .arg(source)
        .arg(&mirror);
    configure_git(&mut clone, request.token.as_deref(), key);
    run_git(clone, cancellation, key.or(request.token.as_deref())).await?;
    drop(fork_source);

    // The upper bound protects the clone while it is being created, but keeping
    // that reservation during publication double-counts the local mirror and
    // incoming receive pack. Shrink it to the measured mirror plus a small
    // command margin before the first internal push so normal-sized imports can
    // share the node's bounded staging budget with the receiver.
    let mirror_bytes = mirror_size(&mirror).await?;
    let mirror_reservation = mirror_bytes
        .checked_add(IMPORT_MIRROR_RESERVE_MARGIN)
        .filter(|reservation| *reservation <= MAX_IMPORT_MIRROR_BYTES)
        .ok_or_else(|| ImportError::Git("Git source exceeds the import storage limit".into()))?;
    server
        .local_staging
        .resize(&directory, mirror_reservation)
        .map_err(ImportError::Staging)?;

    job.update(jobs::State::Running, "Validating Git history")
        .await?;
    let refs = mirror_refs(&mirror, cancellation).await?;
    if refs.len() > MAX_IMPORT_REFS {
        return Err(ImportError::Git(format!(
            "Git source has too many refs (maximum {MAX_IMPORT_REFS})"
        )));
    }
    let branch = default_branch(&mirror, &refs, cancellation).await?;
    let snapshot = jobs::Snapshot::new(branch, &refs);
    job.pin(&snapshot).await?;
    job.update(jobs::State::Running, "Preparing Crab repository")
        .await?;
    account.check(server).await?;
    let actor = &account.identity;
    let members = if server.auth.is_some() && organization.is_none() {
        vec![RepositoryMember {
            subject: actor.subject.clone(),
            name: actor.name.clone(),
            access: RepositoryAccess::Admin,
        }]
    } else {
        Vec::new()
    };
    let record = catalog
        .reserve_git_import(
            id,
            namespace,
            request.name.clone(),
            request.description.clone(),
            MembershipActor {
                issuer: actor.issuer.clone(),
                subject: actor.subject.clone(),
            },
            members,
            request.fork_source,
        )
        .await
        .map_err(ImportError::Catalog)?;
    let repository = open_destination(&catalog, &record, &snapshot).await?;
    let publication = if request.fork_source.is_some() {
        "Publishing Git history and large files"
    } else {
        "Publishing Git history"
    };
    job.update(
        jobs::State::Running,
        &format!("{publication} ({} refs)", refs.len()),
    )
    .await?;
    publish_snapshot(
        context,
        job,
        snapshot.clone(),
        repository,
        &mirror,
        cancellation,
        organization,
    )
    .await?;
    finish_destination(server, &catalog, job, id, organization, &snapshot).await
}

pub(super) async fn open_destination(
    catalog: &CatalogStore,
    record: &CatalogRecord,
    snapshot: &jobs::Snapshot,
) -> Result<Arc<Repository>, ImportError> {
    if record.application != RepositoryApplicationState::GitImportPending {
        return Err(ImportError::DestinationChanged);
    }
    catalog
        .initialize_git(record, &snapshot.default_branch, true)
        .await
        .map_err(ImportError::Catalog)?;
    crate::repository::catalog::materialize_record(catalog, record)
        .await
        .map(Arc::new)
        .map_err(ImportError::Setup)
}

pub(super) async fn publish_snapshot(
    context: &ImportContext,
    job: &Arc<jobs::Writer>,
    snapshot: jobs::Snapshot,
    repository: Arc<Repository>,
    mirror: &Path,
    cancellation: &CancellationToken,
    organization: Option<OrganizationOwner>,
) -> Result<(), ImportError> {
    job.verify().await?;
    let grant = context.begin_repository(
        Arc::clone(&repository),
        Arc::clone(job),
        snapshot.clone(),
        organization,
    );
    let destination = context.destination_url(&repository.config.owner, &repository.config.name);
    let pushed = push_mirror(
        mirror,
        &destination,
        grant.key.to_hex().as_str(),
        cancellation,
    )
    .await;
    drop(grant);
    // A missing Git reply can follow a successful atomic commit. Prove the
    // complete snapshot from the authoritative journal before publishing it.
    let view = repository
        .open_ref_view()
        .await
        .map_err(ImportError::Setup)?;
    if snapshot.matches(view.head(), view.refs()) {
        return Ok(());
    }
    pushed?;
    Err(ImportError::DestinationChanged)
}

pub(super) async fn finish_destination(
    server: &Arc<Server>,
    catalog: &CatalogStore,
    job: &jobs::Writer,
    id: Uuid,
    organization: Option<OrganizationOwner>,
    snapshot: &jobs::Snapshot,
) -> Result<(), ImportError> {
    job.update(jobs::State::Running, "Making repository available")
        .await?;
    server
        .prepare_catalog_repository(catalog, id, RepositoryApplicationState::GitImportPending)
        .await
        .map_err(ImportError::Setup)?;
    job.verify().await?;
    let mark_ready = || async {
        catalog
            .mark_cell_ready(id)
            .await
            .map(|_| ())
            .map_err(ImportError::Catalog)
    };
    if let Some(owner) = organization {
        // The quarantine stays private until the catalog commit. Hold the
        // policy lease through that commit so a default or ruleset edit
        // cannot retarget the repository after the last policy check.
        let published: crate::app::Result<_> = with_ruleset_lease(
            server,
            RulesetScope::Organization(owner.id),
            &server.cancellation,
            |cancel| async move {
                let outcome = async {
                    if cancel.is_cancelled() {
                        return Err(ImportError::Cancelled);
                    }
                    let policy =
                        validate_destination_policy(server, catalog, id, owner, snapshot).await?;
                    owner
                        .authorize_publication(server, id, snapshot, policy)
                        .await?;
                    if cancel.is_cancelled() {
                        return Err(ImportError::Cancelled);
                    }
                    mark_ready().await
                }
                .await;
                Ok(outcome)
            },
        )
        .await;
        published.map_err(ImportError::organization)??;
    } else {
        mark_ready().await?;
    }
    install_destination(server, catalog, id).await
}

async fn validate_destination_policy(
    server: &Server,
    catalog: &CatalogStore,
    id: Uuid,
    owner: OrganizationOwner,
    snapshot: &jobs::Snapshot,
) -> Result<u64, ImportError> {
    use crate::cells::repository::rulesets::{ActiveRulesets, BypassActor, BypassContext};
    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    let record = document
        .repositories
        .iter()
        .find(|row| row.id == id && row.organization_id() == Some(owner.id))
        .ok_or(ImportError::DestinationChanged)?;
    if let Some(source) = record.forked_from {
        // This read shares the organization's policy lease with both settings
        // writers. A disabled fork stays quarantined even after its refs commit.
        owner.require_fork(server, &document, source).await?;
    }
    let repository = crate::repository::catalog::materialize_record(catalog, record)
        .await
        .map_err(ImportError::Setup)?;
    let view = repository
        .open_ref_view()
        .await
        .map_err(ImportError::Setup)?;
    if !snapshot.matches(view.head(), view.refs()) {
        return Err(ImportError::DestinationChanged);
    }
    let inherited = crate::repository::settings::inherited_rulesets(server, &repository)
        .await
        .map_err(ImportError::organization)?;
    let teams = if inherited.rulesets.iter().any(|rule| {
        rule.bypass_actors
            .iter()
            .any(|entry| matches!(entry.actor, BypassActor::Team { .. }))
    }) {
        owner
            .policy_grant(server)
            .await
            .map_err(ImportError::organization)?
            .bypass_teams
    } else {
        Vec::new()
    };
    let actor = BypassContext {
        role: Some(RepositoryAccess::Admin),
        organization_owner: true,
        teams: &teams,
        organization_teams: &teams,
    };
    let rules = ActiveRulesets::new(&inherited)
        .map_err(|error| ImportError::organization(crate::app::Error::Cell(error)))?;
    for reference in view.refs().keys() {
        if rules
            .matches(reference, view.head())
            .any(|rule| rule.restrict_creations && !rule.allows_bypass(actor, false))
        {
            return Err(ImportError::Ruleset);
        }
    }
    Ok(inherited.version)
}

async fn install_destination(
    server: &Arc<Server>,
    catalog: &CatalogStore,
    id: Uuid,
) -> Result<(), ImportError> {
    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    server
        .install_catalog(document)
        .await
        .map_err(ImportError::Setup)?;
    // Import receive defers projection because nothing can read the reservation.
    if let Some(repository) = server.repositories.by_id(id)
        && let Err(error) = repository
            .schedule_maintenance(server, MaintenanceTrigger::Repair)
            .await
    {
        tracing::warn!(repository_id = %id, error = ?error, "import projection scheduling deferred");
    }
    Ok(())
}
