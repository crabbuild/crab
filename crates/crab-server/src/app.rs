use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use crab_storage::StorageError;
use serde_json::json;

use crate::{
    auth::{Identity, Principal},
    catalog::{CatalogError, OrganizationAccess},
    cells::{
        Address,
        organization::teams::{GetRepositoryTeamAccess, RepositoryTeamAccess, RepositoryTeams},
    },
    repository::Repository,
    server::Server,
};

pub(crate) const MAX_NUMBER: u64 = 9_007_199_254_740_991;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Discussion creation was rejected: {0:?}")]
    CreationRejected(crate::cells::repository::collaboration::creation::Failure),
    #[error("Git references are being indexed. Retry shortly")]
    ReviewIndexPending,
    #[error("Sign in to use account features")]
    SessionRequired,
    #[error("Repository permissions are temporarily unavailable")]
    Membership(#[source] Box<crate::catalog::CatalogError>),
    #[error("Account authority is temporarily unavailable")]
    IdentityAuthority(#[source] Box<crate::auth::AuthError>),
    #[error("Repository transfer publication is unavailable")]
    TransferPublication(#[source] crab_remote::publication::Error),
    #[error("The organization’s transfer policy does not allow this repository to be transferred")]
    RepositoryTransferRestricted,
    #[error(
        "The organization’s policy does not allow this repository to be deleted by this account"
    )]
    RepositoryDeletionRestricted,
    #[error("The organization’s policy does not allow private forks")]
    RepositoryForkingRestricted,
    #[error("The organization’s policy restricts repository visibility changes")]
    RepositoryVisibilityRestricted,
    #[error(
        "The requester must have permission to create repositories in the destination organization"
    )]
    RepositoryTransferDestinationCreationRequired,
    #[error("Only a destination organization owner can change the repository name during transfer")]
    RepositoryTransferDestinationOwnerRequired,
    #[error("Incoming issue transfers must finish before repository ownership can change")]
    RepositoryIssueTransfersPending,
    #[error("This transfer expired. Reload the issue to start a new transfer.")]
    IssueTransferExpired,
    #[error("This username is already taken")]
    ProfileHandleTaken,
    #[error("Your profile is already registered; reload to edit it")]
    ProfileExists,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not found")]
    NotFound,
    #[error("Commit not found")]
    CommitNotFound,
    #[error("Only the author can edit this content")]
    Forbidden,
    #[error("You do not have permission to dismiss reviews for this branch")]
    ReviewDismissalPermission,
    #[error("You do not have permission to edit this issue or comment")]
    IssueEditPermission,
    #[error("This conversation is locked")]
    ConversationLocked,
    #[error("You do not have permission to lock or unlock this conversation")]
    ConversationLockPermission,
    #[error("You do not have permission to hide or unhide comments here")]
    CommentModerationPermission,
    #[error("Only the author or a repository writer can remove edit history")]
    EditHistoryPermission,
    #[error("Archived repositories are read-only")]
    Archived,
    #[error("Pull request authors cannot approve or request changes on their own changes")]
    OwnReview,
    #[error("Draft pull requests must be marked ready before merging")]
    DraftPull,
    #[error("This content changed; reload before saving your draft")]
    Conflict,
    #[error(
        "This repository was replaced; reload before starting another action. An earlier action may have completed in the original repository"
    )]
    RepositoryChanged,
    #[error("The repository name or creation request is already in use")]
    RepositoryNameUnavailable,
    #[error("An organization ruleset protects this repository name")]
    RepositoryNameProtected,
    #[error("Repository name changes are temporarily unavailable")]
    RepositoryNamePublication(#[source] crab_remote::publication::Error),
    #[error("The repository description changed. Reload the description before saving")]
    RepositoryDescriptionChanged,
    #[error("The repository topics changed. Reload the topics before saving")]
    RepositoryTopicsChanged,
    #[error("Repository attributes changed. Reload and try again")]
    RepositoryAttributesChanged,
    #[error("Repository creation failed")]
    RepositorySetup(#[source] Box<crate::Error>),
    #[error("This list changed; reload from the first page")]
    PageChanged,
    #[error("The pull request changes conflict or its branch tips changed")]
    MergeConflict,
    #[error("Write access is required to update the head branch")]
    BranchUpdatePermission,
    #[error("The pull request branch comparison changed")]
    BranchUpdateChanged,
    #[error("The replacement branch name is already in use")]
    BranchUpdateDestination,
    #[error("The pull request or branch changed")]
    PullBranchChanged,
    #[error("An open pull request still uses the branch")]
    PullBranchInUse,
    #[error("{0}")]
    StackedDeletionBlocked(&'static str),
    #[error("The pull request has a missing branch")]
    PullBranchesUnavailable,
    #[error("Pull request branch edit failed")]
    PullBranch(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("The base changes do not merge cleanly into the head branch")]
    BranchUpdateConflict,
    #[error("The pull request cannot be safely rebased")]
    UnsafeRebase,
    #[error("Auto-merge is disabled for this repository or merge method")]
    AutoMergeDisabled,
    #[error("The pull request is in the merge queue")]
    MergeQueued,
    #[error("The base branch requires the merge queue")]
    MergeQueueRequired,
    #[error("The auto-merge request or comparison changed")]
    AutoMergeChanged,
    #[error("This merge method is disabled for the repository")]
    MergeMethodDisabled,
    #[error("Repository merge methods changed")]
    MergeMethodsChanged,
    #[error("Maintain access is required to manage merge methods")]
    MergeSettingsPermission,
    #[error("Administrator access is required to manage merge queue settings")]
    MergeQueueSettingsPermission,
    #[error("Administrator access is required to manage automatic branch deletion")]
    BranchCleanupSettingsPermission,
    #[error("Git writes are busy")]
    MergeBusy,
    #[error("Write access is required to merge pull requests")]
    MergePermission,
    #[error("A pull request merge is in progress")]
    MergePending,
    #[error("Protected branch review requirements are not satisfied")]
    MergeBlocked,
    #[error("A ruleset restricts updates to the base branch")]
    MergeUpdatesRestricted,
    #[error("The base branch requires linear history")]
    MergeLinearHistory,
    #[error("Required checks are not satisfied on the merge candidate")]
    MergeCandidateChecks,
    #[error("Unresolved review conversations block the merge")]
    MergeUnresolvedConversations,
    #[error("The head branch is behind the base branch")]
    MergeHeadBehind,
    #[error("Ruleset bypass is no longer available")]
    MergeBypassUnavailable,
    #[error("Write access is required to report commit statuses")]
    StatusPermission,
    #[error("Write access is required to manage check runs")]
    CheckPermission,
    #[error("Check run not found")]
    CheckNotFound,
    #[error("You do not have permission to change labels")]
    LabelPermission,
    #[error("Write access is required to pin issues")]
    PinPermission,
    #[error("You do not have permission to change milestones")]
    MilestonePermission,
    #[error("Write access is required to manage linked work")]
    LinkPermission,
    #[error("Triage access is required to manage issue dependencies")]
    DependencyPermission,
    #[error("Triage access is required to manage sub-issues")]
    SubIssuePermission,
    #[error("Write access is required to manage projects")]
    ProjectPermission,
    #[error("A milestone with this title already exists")]
    MilestoneConflict,
    #[error("A repository label with this name already exists")]
    LabelConflict,
    #[error("A team with this name already exists")]
    TeamNameConflict,
    #[error("Repository label not found")]
    LabelNotFound,
    #[error("Triage access is required to manage assignees")]
    AssigneePermission,
    #[error("Write access is required to manage releases")]
    ReleasePermission,
    #[error("Invalid release notes YAML")]
    ReleaseNotesYaml(#[source] serde_yaml::Error),
    #[error("{0}")]
    RefRule(&'static str),
    #[error("A release already exists for this tag or the tag points to another commit")]
    ReleaseConflict,
    #[error("Git writes are busy")]
    ReleaseBusy,
    #[error("Release staging failed")]
    ReleaseStaging(#[source] crate::local_disk::Error),
    #[error("Release not found")]
    ReleaseNotFound,
    #[error("Release asset not found")]
    ReleaseAssetNotFound,
    #[error("A release asset with this name already exists")]
    ReleaseAssetConflict,
    #[error("Release asset is too large")]
    ReleaseAssetTooLarge,
    #[error("Release asset upload was cancelled or timed out")]
    ReleaseAssetCancelled,
    #[error("Invalid release asset body")]
    ReleaseAssetBody(#[source] axum::Error),
    #[error("Release asset temporary file operation failed")]
    ReleaseAssetIo(#[source] std::io::Error),
    #[error("Release asset transfer admission failed")]
    ReleaseAssetCoordination(#[source] crab_coordination::CoordinationError),
    #[error("Discussion attachment transfer failed")]
    AttachmentTransfer(#[source] crate::api::asset_transfer::Error),
    #[error("Discussion attachment transfer admission failed")]
    AttachmentAdmission(#[source] crate::transfer_admission::Error),
    #[error("Release tag publication failed")]
    Release(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("Pull request merge failed")]
    Merge(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("Pull request merge object construction failed")]
    MergeObject(#[source] Box<crab_remote::objects::Error>),
    #[error("Repository initial commit failed")]
    InitialCommit(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("Repository initial commit object construction failed")]
    InitialCommitObject(#[source] Box<crab_remote::objects::Error>),
    #[error(
        "This submission ID was already used for different content; check the existing result before submitting again"
    )]
    RequestConflict,
    #[error("Collaboration Cell routing is unavailable")]
    CellUnavailable,
    #[error("Collaboration Cell contract failed: {0}")]
    CellContract(&'static str),
    #[error("Collaboration Cell operation failed")]
    Cell(#[source] crate::cells::Error),
    #[error("Collaboration storage failed")]
    Storage(#[from] StorageError),
    #[error("Collaboration data encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("Invalid request body")]
    Body(#[from] JsonRejection),
    #[error("Clock failed")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("Repository read failed")]
    Repository(#[from] crate::Error),
}
pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn mutation_identity() -> Result<crate::cells::MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(crate::cells::MutationIdentity {
        request_id: uuid::Uuid::now_v7(),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

impl From<&Identity> for crate::cells::repository::RepositoryAuthor {
    fn from(identity: &Identity) -> Self {
        Self {
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
            name: identity.name.clone(),
        }
    }
}

pub(crate) fn repository_cell(
    server: &Server,
    repository: &Repository,
) -> Result<crate::cells::RepositoryCell> {
    server
        .cells()
        .ok_or(Error::CellUnavailable)?
        .repository(repository.id)
        .map_err(Error::Cell)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if matches!(self, Self::Cell(crate::cells::Error::AttachmentUnavailable)) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                [("x-crab-write-outcome", "rejected")],
                Json(json!({"error":{"code":"attachment_unavailable","message":"An attachment is unavailable or the attachment limit was exceeded. Remove it or upload it again."}})),
            )
                .into_response();
        }
        let expected_lock_conflict = matches!(
            &self,
            Self::Merge(error) if matches!(error.as_ref(), crate::git::receive::ReceiveError::Locked)
        );
        if !expected_lock_conflict
            && matches!(
                self,
                Self::Storage(_)
                    | Self::Json(_)
                    | Self::Clock(_)
                    | Self::Repository(_)
                    | Self::Membership(_)
                    | Self::IdentityAuthority(_)
                    | Self::TransferPublication(_)
                    | Self::RepositoryNamePublication(_)
                    | Self::Merge(_)
                    | Self::PullBranch(_)
                    | Self::MergeObject(_)
                    | Self::InitialCommit(_)
                    | Self::InitialCommitObject(_)
                    | Self::Release(_)
                    | Self::ReleaseAssetIo(_)
                    | Self::ReleaseAssetCoordination(_)
                    | Self::ReleaseStaging(_)
                    | Self::CellContract(_)
                    | Self::Cell(_)
            )
        {
            tracing::error!(error = ?self, "collaboration request failed");
        }
        let staging = match &self {
            Self::Merge(error)
            | Self::Release(error)
            | Self::PullBranch(error)
            | Self::InitialCommit(error) => error.staging_status(),
            _ => None,
        };
        if let Some((status, message)) = staging {
            return (
                status,
                Json(json!({"error":{"code":"staging","message":message}})),
            )
                .into_response();
        }
        let (status, code, message) = match &self {
            Self::Membership(_) | Self::IdentityAuthority(_) | Self::TransferPublication(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "membership_unavailable",
                "Repository permissions are temporarily unavailable.",
            ),
            Self::SessionRequired => (
                StatusCode::UNAUTHORIZED,
                "session_required",
                "Sign in to use account features",
            ),
            Self::ProfileHandleTaken => (
                StatusCode::CONFLICT,
                "handle_taken",
                "This username is already taken",
            ),
            Self::ProfileExists => (
                StatusCode::CONFLICT,
                "profile_exists",
                "Your profile is already registered; reload to edit it",
            ),
            Self::Cell(crate::cells::Error::AccessDenied) => (
                StatusCode::FORBIDDEN,
                "permission_required",
                "Current membership does not permit this operation",
            ),
            Self::Cell(crate::cells::Error::SchemaUnavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_upgrade_required",
                "Your server needs an update. Contact your administrator.",
            ),
            Self::Cell(crate::cells::Error::NotInitialized) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "directory_unavailable",
                "Profiles are not ready. Contact your server administrator.",
            ),
            Self::PageChanged => (
                StatusCode::CONFLICT,
                "page_changed",
                "This list changed; reload from the first page",
            ),
            Self::RepositoryIssueTransfersPending => (
                StatusCode::CONFLICT,
                "issue_transfers_pending",
                "Incoming issue transfers must finish before repository ownership can change",
            ),
            Self::RepositoryTransferRestricted => (
                StatusCode::FORBIDDEN,
                "repository_transfer_restricted",
                "The organization’s transfer policy does not allow this repository to be transferred",
            ),
            Self::RepositoryDeletionRestricted => (
                StatusCode::FORBIDDEN,
                "repository_deletion_restricted",
                "The organization’s policy does not allow this repository to be deleted by this account",
            ),
            Self::RepositoryForkingRestricted => (
                StatusCode::FORBIDDEN,
                "repository_forking_restricted",
                "The organization’s policy does not allow private forks",
            ),
            Self::RepositoryVisibilityRestricted => (
                StatusCode::FORBIDDEN,
                "repository_visibility_restricted",
                "The organization’s policy restricts repository visibility changes",
            ),
            Self::RepositoryTransferDestinationCreationRequired => (
                StatusCode::FORBIDDEN,
                "transfer_destination_creation_required",
                "The requester must have permission to create repositories in the destination organization",
            ),
            Self::RepositoryTransferDestinationOwnerRequired => (
                StatusCode::FORBIDDEN,
                "transfer_destination_owner_required",
                "Only a destination organization owner can change the repository name during transfer",
            ),
            Self::IssueTransferExpired => (
                StatusCode::CONFLICT,
                "issue_transfer_expired",
                "This transfer expired. Reload the issue to start a new transfer.",
            ),
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Resource not found"),
            Self::CommitNotFound => (StatusCode::NOT_FOUND, "not_found", "Commit not found"),
            Self::ReviewDismissalPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to dismiss reviews for this branch",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Only the author can edit this content",
            ),
            Self::IssueEditPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to edit this issue or comment",
            ),
            Self::ConversationLocked => (
                StatusCode::LOCKED,
                "conversation_locked",
                "This conversation is locked",
            ),
            Self::ConversationLockPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to lock or unlock this conversation",
            ),
            Self::CommentModerationPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to hide or unhide comments here",
            ),
            Self::EditHistoryPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Only the author or a repository writer can remove edit history",
            ),
            Self::Archived => (
                StatusCode::FORBIDDEN,
                "repository_archived",
                "This repository is archived and read-only",
            ),
            Self::OwnReview => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Pull request authors cannot approve or request changes on their own changes",
            ),
            Self::Conflict | Self::Storage(StorageError::StateConflict { .. }) => (
                StatusCode::CONFLICT,
                "conflict",
                "This content changed; reload before saving your draft",
            ),
            Self::MergeConflict => (
                StatusCode::CONFLICT,
                "merge_conflict",
                "The branch tips changed or these changes conflict. Reload and review the latest commits",
            ),
            Self::BranchUpdatePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to update the head branch",
            ),
            Self::BranchUpdateChanged => (
                StatusCode::CONFLICT,
                "branch_update_changed",
                "The pull request or branch tips changed. Refresh the comparison before updating",
            ),
            Self::BranchUpdateDestination => (
                StatusCode::CONFLICT,
                "branch_update_destination",
                "Choose an unused replacement branch name",
            ),
            Self::PullBranchChanged => (
                StatusCode::CONFLICT,
                "pull_branch_changed",
                "The pull request or branch changed. Reload the pull request.",
            ),
            Self::PullBranchInUse => (
                StatusCode::CONFLICT,
                "pull_branch_in_use",
                "An open pull request still uses this branch.",
            ),
            Self::StackedDeletionBlocked(message) => {
                (StatusCode::CONFLICT, "stacked_deletion_blocked", *message)
            }
            Self::PullBranchesUnavailable => (
                StatusCode::CONFLICT,
                "pull_branches_unavailable",
                "Restore the missing branch before reopening the pull request.",
            ),
            Self::PullBranch(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "pull_branch_failed",
                "Crab could not complete the branch action. Retry shortly.",
            ),
            Self::BranchUpdateConflict => (
                StatusCode::CONFLICT,
                "branch_update_conflict",
                "The base changes conflict with the head branch",
            ),
            Self::UnsafeRebase => (
                StatusCode::CONFLICT,
                "unsafe_rebase",
                "Crab could not replay these commits without changing the merge result. Rebase the topic branch in Git or choose another merge method",
            ),
            Self::MergeQueued => (
                StatusCode::CONFLICT,
                "merge_queued",
                "Remove the pull request from the merge queue before editing its candidate or merging it directly",
            ),
            Self::MergeQueueRequired => (
                StatusCode::CONFLICT,
                "merge_queue_required",
                "This branch requires the merge queue. Add the pull request to the queue to merge it",
            ),
            Self::AutoMergeDisabled => (
                StatusCode::CONFLICT,
                "auto_merge_disabled",
                "Auto-merge is disabled for this repository or merge method",
            ),
            Self::AutoMergeChanged => (
                StatusCode::CONFLICT,
                "auto_merge_changed",
                "The auto-merge request or comparison changed. Refresh the pull request",
            ),
            Self::MergeMethodDisabled => (
                StatusCode::CONFLICT,
                "merge_method_disabled",
                "This merge method is disabled. Refresh to select an enabled method",
            ),
            Self::MergeMethodsChanged => (
                StatusCode::CONFLICT,
                "merge_methods_changed",
                "Merge methods changed. Reload the settings and try again",
            ),
            Self::MergeSettingsPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Maintain access is required to manage merge methods",
            ),
            Self::MergeQueueSettingsPermission => (
                StatusCode::FORBIDDEN,
                "merge_queue_settings_permission",
                "Administrator access is required to manage merge queue settings",
            ),
            Self::BranchCleanupSettingsPermission => (
                StatusCode::FORBIDDEN,
                "branch_cleanup_settings_permission",
                "Administrator access is required to manage automatic branch deletion",
            ),
            Self::MergeBusy => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy. Retry this action shortly.",
            ),
            Self::MergePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to merge pull requests",
            ),
            Self::MergePending => (
                StatusCode::CONFLICT,
                "merge_pending",
                "A merge is in progress. Reload the pull request and retry that merge before editing",
            ),
            Self::DraftPull => (
                StatusCode::CONFLICT,
                "pull_draft",
                "Draft pull requests must be marked ready before merging",
            ),
            Self::MergeBypassUnavailable => (
                StatusCode::CONFLICT,
                "ruleset_bypass_unavailable",
                "The requested ruleset bypass is unavailable. Refresh the pull request",
            ),
            Self::MergeUnresolvedConversations => (
                StatusCode::CONFLICT,
                "unresolved_conversations",
                "Resolve all review conversations before merging",
            ),
            Self::MergeUpdatesRestricted => (
                StatusCode::FORBIDDEN,
                "ruleset_restricts_updates",
                "A ruleset restricts updates to the base branch",
            ),
            Self::MergeLinearHistory => (
                StatusCode::CONFLICT,
                "linear_history_required",
                "The base branch requires linear history. Use squash or rebase",
            ),
            Self::MergeHeadBehind => (
                StatusCode::CONFLICT,
                "head_branch_behind",
                "Update the head branch with the latest base commit before merging",
            ),
            Self::MergeBlocked => (
                StatusCode::CONFLICT,
                "merge_blocked",
                "Required reviews or status checks are not satisfied",
            ),
            Self::MergeCandidateChecks => (
                StatusCode::CONFLICT,
                "merge_candidate_checks",
                "Configure required checks on the base branch and pass them on the merge candidate",
            ),
            Self::StatusPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to report commit statuses",
            ),
            Self::CheckPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage check runs",
            ),
            Self::CheckNotFound => (StatusCode::NOT_FOUND, "not_found", "Check run not found"),
            Self::MilestonePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to change milestones",
            ),
            Self::ProjectPermission => (
                StatusCode::FORBIDDEN,
                "project_permission",
                "Write access is required to manage projects",
            ),
            Self::SubIssuePermission => (
                StatusCode::FORBIDDEN,
                "sub_issue_permission",
                "Triage access is required to manage sub-issues",
            ),
            Self::LinkPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage linked work",
            ),
            Self::DependencyPermission => (
                StatusCode::FORBIDDEN,
                "dependency_permission",
                "Triage access is required to manage issue dependencies",
            ),
            Self::MilestoneConflict => (
                StatusCode::CONFLICT,
                "milestone_conflict",
                "A milestone with this title already exists",
            ),
            Self::LabelPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to change labels",
            ),
            Self::PinPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to pin issues",
            ),
            Self::LabelConflict => (
                StatusCode::CONFLICT,
                "label_conflict",
                "A repository label with this name already exists",
            ),
            Self::TeamNameConflict => (
                StatusCode::CONFLICT,
                "team_name_conflict",
                "A team with this name already exists",
            ),
            Self::LabelNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Repository label not found",
            ),
            Self::AssigneePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Triage access is required to manage assignees",
            ),
            Self::RefRule(message) => (StatusCode::FORBIDDEN, "repository_ruleset", *message),
            Self::ReleasePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage releases",
            ),
            Self::ReleaseNotesYaml(_) => (
                StatusCode::BAD_REQUEST,
                "release_notes_yaml",
                "Use valid YAML with supported changelog fields in the release note configuration",
            ),
            Self::ReleaseConflict => (
                StatusCode::CONFLICT,
                "release_conflict",
                "A release already exists for this tag or the tag points to another commit",
            ),
            Self::ReleaseStaging(crate::local_disk::Error::TooLarge) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "asset_too_large",
                "Release staging exceeds the node disk capacity",
            ),
            Self::ReleaseBusy | Self::ReleaseStaging(crate::local_disk::Error::Busy(_)) => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy; retry this release shortly",
            ),
            Self::ReleaseNotFound => (StatusCode::NOT_FOUND, "not_found", "Release not found"),
            Self::ReleaseAssetNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Release asset not found",
            ),
            Self::ReleaseAssetConflict => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "asset_conflict",
                "A release asset with this name already exists",
            ),
            Self::ReleaseAssetTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "asset_too_large",
                "Release assets must be no larger than 512 MiB",
            ),
            Self::AttachmentTransfer(crate::api::asset_transfer::Error::TooLarge) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "attachment_too_large",
                "Attachments must be no larger than 25 MiB",
            ),
            Self::AttachmentTransfer(
                crate::api::asset_transfer::Error::Cancelled
                | crate::api::asset_transfer::Error::Staging(crate::local_disk::Error::Cancelled),
            )
            | Self::AttachmentAdmission(crate::transfer_admission::Error::Cancelled) => (
                StatusCode::REQUEST_TIMEOUT,
                "attachment_cancelled",
                "Attachment transfer was cancelled or timed out",
            ),
            Self::AttachmentTransfer(crate::api::asset_transfer::Error::Body(_)) => (
                StatusCode::BAD_REQUEST,
                "invalid_attachment_body",
                "The attachment body could not be read",
            ),
            Self::AttachmentTransfer(_) | Self::AttachmentAdmission(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "attachment_unavailable",
                "Attachment transfers are temporarily unavailable. Retry shortly.",
            ),
            Self::ReleaseAssetCancelled
            | Self::ReleaseStaging(crate::local_disk::Error::Cancelled) => (
                StatusCode::REQUEST_TIMEOUT,
                "asset_cancelled",
                "Release asset upload was cancelled or timed out",
            ),
            Self::ReleaseAssetBody(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid release asset body",
            ),
            Self::ReleaseAssetIo(_)
            | Self::ReleaseStaging(_)
            | Self::ReleaseAssetCoordination(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "asset_storage",
                "Release asset storage is unavailable. Retry shortly",
            ),
            Self::RepositoryChanged => (
                StatusCode::CONFLICT,
                "repository_changed",
                "This repository was replaced; reload before starting another action. An earlier action may have completed in the original repository",
            ),
            Self::RepositoryNameUnavailable => (
                StatusCode::CONFLICT,
                "repository_name_unavailable",
                "The repository name or creation request is already in use",
            ),
            Self::RepositoryNameProtected => (
                StatusCode::FORBIDDEN,
                "repository_name_protected",
                "An organization ruleset protects this repository name",
            ),
            Self::RepositoryNamePublication(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "repository_name_busy",
                "Repository name changes are temporarily unavailable. Retry the same submission shortly",
            ),
            Self::RepositoryDescriptionChanged => (
                StatusCode::CONFLICT,
                "repository_description_changed",
                "The repository description changed. Reload the description before saving",
            ),
            Self::RepositoryTopicsChanged => (
                StatusCode::CONFLICT,
                "repository_topics_changed",
                "The repository topics changed. Reload the topics before saving",
            ),
            Self::RepositoryAttributesChanged => (
                StatusCode::CONFLICT,
                "repository_attributes_changed",
                "Repository attributes changed. Reload and try again",
            ),
            Self::RepositorySetup(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "repository_setup",
                "Repository setup is incomplete. Retry the same submission to continue",
            ),
            Self::RequestConflict => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content; check the existing result before submitting again",
            ),
            Self::CreationRejected(reason) => match reason {
                crate::cells::repository::collaboration::creation::Failure::Forbidden => (
                    StatusCode::FORBIDDEN,
                    "creation_rejected",
                    "Triage access is required to set initial discussion metadata",
                ),
                crate::cells::repository::collaboration::creation::Failure::Labels => (
                    StatusCode::BAD_REQUEST,
                    "creation_rejected",
                    "An initial label is no longer available",
                ),
                crate::cells::repository::collaboration::creation::Failure::Assignees => (
                    StatusCode::BAD_REQUEST,
                    "creation_rejected",
                    "An initial assignee is no longer available",
                ),
                crate::cells::repository::collaboration::creation::Failure::Milestone => (
                    StatusCode::BAD_REQUEST,
                    "creation_rejected",
                    "The initial milestone is no longer available",
                ),
                crate::cells::repository::collaboration::creation::Failure::ComparisonChanged => (
                    StatusCode::CONFLICT,
                    "comparison_changed",
                    "A branch moved or was deleted. Review the updated comparison before creating the pull request",
                ),
                crate::cells::repository::collaboration::creation::Failure::ExistingPull(_) => (
                    StatusCode::CONFLICT,
                    "open_pull_exists",
                    "A pull request is already open for these branches",
                ),
            },
            Self::ReviewIndexPending => (
                StatusCode::SERVICE_UNAVAILABLE,
                "review_index_pending",
                "Git references are being indexed. Retry shortly",
            ),
            Self::Cell(crate::cells::Error::IndexUnavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "discussion_index_unavailable",
                "Repository search requires preparation. An administrator must rebuild the discussion index.",
            ),
            Self::Cell(crate::cells::Error::StaleSearch) => (
                StatusCode::CONFLICT,
                "stale_search_cursor",
                "This discussion list has changed. Return to the first page to continue.",
            ),
            Self::Cell(crate::cells::Error::StaleRead) => (
                StatusCode::CONFLICT,
                "stale_read",
                "Repository Cell has not reached the requested revision",
            ),
            Self::Cell(crate::cells::Error::Pending { .. }) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "outcome_unknown",
                "The write may have completed; retry the same submission to resolve it",
            ),
            Self::CellUnavailable | Self::Cell(crate::cells::Error::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "cell_unavailable",
                "Repository collaboration is temporarily unavailable; retry the same submission shortly",
            ),
            Self::Cell(crate::cells::Error::RequestConflict) => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content",
            ),
            Self::Cell(crate::cells::Error::Expired) => (
                StatusCode::GONE,
                "request_expired",
                "Repository request expired before it could execute",
            ),
            Self::CellContract(_) | Self::Cell(_) => (
                StatusCode::BAD_GATEWAY,
                "cell_error",
                "Repository collaboration runtime failed; retry the same submission after checking service health",
            ),
            Self::Body(error) => (
                error.status(),
                "invalid_request",
                "Invalid JSON request or request body too large",
            ),
            Self::Merge(error)
                if matches!(error.as_ref(), crate::git::receive::ReceiveError::Locked) =>
            {
                (
                    StatusCode::CONFLICT,
                    "path_locked",
                    "A changed path is locked by another user",
                )
            }
            Self::Merge(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "merge_failed",
                "The merge may have completed. Reload the pull request before retrying the same submission",
            ),
            Self::Release(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "release_failed",
                "The release tag may have been published. Retry the same submission to recover it",
            ),
            Self::InitialCommit(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "initial_commit_failed",
                "The repository was created, but the initial commit failed. Retry the same submission",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "storage_error",
                "Collaboration storage is unavailable. A write may have succeeded; retry the same submission to recover it",
            ),
        };

        let mut error = json!({"code":code,"message":message});
        if let Self::CreationRejected(
            crate::cells::repository::collaboration::creation::Failure::ExistingPull(number),
        ) = self
        {
            error["pull_number"] = json!(number);
        }
        (status, Json(json!({"error": error}))).into_response()
    }
}

pub(crate) fn account(principal: &Principal) -> Result<crate::cells::directory::IdentityKey> {
    // Git tokens are repository-scoped credentials, even when their issuing
    // account can administer a repository. They cannot manage account identity.
    if !(principal.is_user() || principal.is_local()) {
        return Err(Error::SessionRequired);
    }
    let actor = principal.identity().ok_or(Error::SessionRequired)?;
    Ok(crate::cells::directory::IdentityKey {
        issuer: actor.issuer,
        subject: actor.subject,
    })
}

pub(crate) async fn admit(
    State(server): State<Arc<Server>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = server.app_admission.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"Collaboration requests are busy; retry shortly"}})),
        )
            .into_response();
    };
    // Admission covers producing the response, not consuming its body. Streaming
    // handlers must retain their own transfer permit and cancellation ownership.
    let timeout_message = if request.method().is_safe() {
        "The read request timed out. Retry the read."
    } else {
        "Collaboration request timed out; retry the same submission to recover a possible completed write"
    };
    let started = Instant::now();
    let response = tokio::select! {
        () = server.cancellation.cancelled() => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"Server is shutting down; retry the same submission after restart"}}))).into_response(),
        response = tokio::time::timeout(Duration::from_secs(30), next.run(request)) => match response {
            Ok(response) => response,
            Err(_) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error":{"message":timeout_message}}))).into_response(),
        },
    };
    (
        [(
            "server-timing",
            format!("app;dur={:.3}", started.elapsed().as_secs_f64() * 1000.0),
        )],
        response,
    )
        .into_response()
}

pub(crate) fn repository(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<Arc<Repository>> {
    server
        .repositories
        .get(key)
        .filter(|repo| principal.can_read(repo.id))
        .ok_or(Error::NotFound)
}

pub(crate) struct Membership {
    pub members: Vec<crate::RepositoryMember>,
    pub groups: Vec<crate::RepositoryGroup>,
    pub principal: Principal,
    pub organization: Option<uuid::Uuid>,
    pub(crate) fork_destination: Option<uuid::Uuid>,
    fork_creator: Option<crate::catalog::ForkCreatorAccess>,
    baseline: crate::catalog::RepositoryBaseline,
    pub(crate) teams: Vec<crate::catalog::TeamGrant>,
    pub(crate) review_teams: Vec<uuid::Uuid>,
    organization_owner: bool,
    bypass_teams: Vec<uuid::Uuid>,
    organization_bypass_teams: Vec<uuid::Uuid>,
}

impl Membership {
    pub(crate) fn bypass_context(
        &self,
        repository: uuid::Uuid,
    ) -> crate::cells::repository::rulesets::BypassContext<'_> {
        crate::cells::repository::rulesets::BypassContext {
            role: if self.organization_owner && self.principal.import_grant().is_some() {
                // The intake credential writes only the pinned quarantine. Use
                // the live owner role solely when evaluating inherited rules.
                Some(crate::RepositoryAccess::Admin)
            } else {
                self.principal.write_role(repository)
            },
            organization_owner: self.organization_owner,
            teams: &self.bypass_teams,
            organization_teams: &self.organization_bypass_teams,
        }
    }

    pub(crate) async fn durable_members(
        &self,
        server: &Server,
    ) -> Result<Vec<crate::RepositoryMember>> {
        let mut members = self.members.clone();
        let source_owner_access = if self.fork_destination.is_some() {
            crate::RepositoryAccess::Read
        } else {
            crate::RepositoryAccess::Admin
        };
        let mut inherited = Vec::new();
        if let Some(organization) = self.organization {
            inherited.extend(
                crate::repository::access::organization_members(
                    server,
                    organization,
                    &self.teams,
                    self.baseline,
                    source_owner_access,
                    Some(&self.principal),
                )
                .await?,
            );
        }
        if let Some(destination) = self.fork_destination {
            inherited.extend(
                crate::repository::access::organization_members(
                    server,
                    destination,
                    &[],
                    crate::catalog::RepositoryBaseline::None,
                    crate::RepositoryAccess::Admin,
                    Some(&self.principal),
                )
                .await?,
            );
        }
        if let (Some(source), Some(creator)) = (self.organization, &self.fork_creator)
            && let Some((_, crate::repository::access::ForkCreatorGrant::Durable)) =
                crate::repository::access::fork_creator_grant(
                    server,
                    creator,
                    source,
                    &self.teams,
                    self.fork_destination,
                )
                .await?
        {
            inherited.push(crate::RepositoryMember {
                subject: creator.actor.subject.clone(),
                name: creator
                    .source_member
                    .as_ref()
                    .map_or_else(|| "Fork creator".into(), |member| member.name.clone()),
                access: crate::RepositoryAccess::Write,
            });
        }
        for inherited in inherited {
            if let Some(member) = members
                .iter_mut()
                .find(|member| member.subject == inherited.subject)
            {
                member.access = member.access.max(inherited.access);
            } else {
                members.push(inherited);
            }
        }
        Ok(members)
    }

    pub(crate) async fn merge_reviewers(
        &self,
        server: &Server,
    ) -> Result<crate::cells::repository::MergeReviewers> {
        use crate::cells::repository::MergeReviewers;
        if self.principal.is_local() {
            let operator = actor(&self.principal)?;
            return Ok(MergeReviewers {
                issuer: operator.issuer,
                subjects: vec![operator.subject],
            });
        }
        let issuer = server
            .auth
            .as_ref()
            .ok_or(Error::NotFound)?
            .issuer()
            .to_owned();
        // Only a current durable Write or Admin grant can qualify another
        // actor. OIDC group claims are evidence local to their own session.
        let mut subjects = self
            .durable_members(server)
            .await?
            .into_iter()
            .filter(|member| member.access >= crate::RepositoryAccess::Write)
            .map(|member| member.subject)
            .collect::<Vec<_>>();
        subjects.sort_unstable();
        Ok(MergeReviewers { issuer, subjects })
    }
}

pub(crate) fn repository_access_error(error: CatalogError) -> Error {
    match error {
        CatalogError::Conflict => Error::Conflict,
        CatalogError::NotFound => Error::NotFound,
        CatalogError::Invalid(message) => Error::Invalid(message),
        error => Error::Membership(Box::new(error)),
    }
}

pub(crate) async fn repository_team_access(
    server: &Server,
    organization: &OrganizationAccess,
    actor: Option<uuid::Uuid>,
    include_names: bool,
) -> Result<RepositoryTeamAccess> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    query_output(
        cells
            .query::<GetRepositoryTeamAccess>(
                Address::Organization(organization.id),
                None,
                RepositoryTeams {
                    actor,
                    teams: organization.teams.iter().map(|grant| grant.team).collect(),
                    include_names,
                },
            )
            .await,
    )
}

pub(crate) async fn membership(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<Membership> {
    if principal.is_auto_merge() {
        // A persisted automation actor has no live session to enforce suspension.
        let auth = server.auth.as_ref().ok_or(Error::NotFound)?;
        let actor = principal.identity().ok_or(Error::NotFound)?;
        if actor.issuer != auth.issuer()
            || auth
                .account_state(&actor.subject)
                .await
                .map_err(|error| Error::IdentityAuthority(Box::new(error)))?
                .suspended
        {
            return Err(Error::NotFound);
        }
    }
    if principal.is_local() || principal.import_grant().is_some() || principal.is_mirror() {
        if !principal.can_read(repo.id) {
            return Err(Error::NotFound);
        }
        if principal.is_local() && repo.organization.is_some() {
            let snapshot = server
                .catalog()
                .ok_or(Error::NotFound)?
                .membership(&repo.config.owner, &repo.config.name)
                .await
                .map_err(repository_access_error)?;
            // A local credential must not bypass catalog removal on a stale peer.
            if snapshot.repository_id != repo.id
                || snapshot.name != repo.config.name
                || snapshot.ownership_epoch != repo.ownership_epoch
                || snapshot.forked_from != repo.forked_from
            {
                return Err(Error::NotFound);
            }
        }
        let organization_owner = if let Some(grant) = principal.import_grant() {
            grant.current_owner(server).await.ok_or(Error::NotFound)?
        } else {
            false
        };
        return Ok(Membership {
            members: Vec::new(),
            groups: Vec::new(),
            principal: principal.clone(),
            organization: repo.organization,
            fork_destination: None,
            fork_creator: None,
            baseline: Default::default(),
            teams: Vec::new(),
            review_teams: Vec::new(),
            organization_owner,
            bypass_teams: Vec::new(),
            organization_bypass_teams: Vec::new(),
        });
    }
    catalog_membership(server, principal, repo).await
}

pub(crate) async fn catalog_membership(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<Membership> {
    let snapshot = server
        .catalog()
        .ok_or(Error::NotFound)?
        .membership(&repo.config.owner, &repo.config.name)
        .await
        .map_err(|error| match error {
            crate::catalog::CatalogError::NotFound => Error::NotFound,
            error => Error::Membership(Box::new(error)),
        })?;
    // Catalog lookup is case-insensitive. A pinned route from before a
    // case-only rename must not authorize rules against the old spelling.
    if snapshot.name != repo.config.name {
        return Err(Error::NotFound);
    }
    let current = membership_snapshot(server, principal, repo, snapshot).await?;
    if !current.principal.can_read(repo.id) {
        return Err(Error::NotFound);
    }
    // A request that already entered with a grant can have fetched data that
    // matches that grant. Restart the read if it crosses the public boundary.
    if principal.can_read(repo.id)
        && principal.is_public_reader(repo.id) != current.principal.is_public_reader(repo.id)
    {
        return Err(Error::NotFound);
    }
    Ok(current)
}

pub(crate) async fn membership_snapshot(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    snapshot: crate::catalog::MembershipSnapshot,
) -> Result<Membership> {
    let organization = snapshot
        .fork_destination
        .or_else(|| snapshot.organization.as_ref().map(|access| access.id));
    if snapshot.repository_id != repo.id
        || organization != repo.organization
        || snapshot.ownership_epoch != repo.ownership_epoch
        || snapshot.forked_from != repo.forked_from
    {
        return Err(Error::NotFound);
    }
    membership_for_config(server, principal, &repo.config, snapshot).await
}

pub(crate) async fn require_related_read(
    server: &Server,
    principal: &Principal,
    origin: &Repository,
    related: &Repository,
) -> Result<()> {
    let current = membership(server, principal, origin).await?;
    let mut reader = current
        .principal
        .related_read(origin.id, related.id)
        .ok_or(Error::NotFound)?;
    if (origin.public_fork && origin.forked_from == Some(related.id))
        || (related.public_fork && related.forked_from == Some(origin.id))
    {
        reader.allow_public_reads();
    }
    membership(server, &reader, related).await.map(drop)
}

pub(crate) async fn membership_for_config(
    server: &Server,
    principal: &Principal,
    config: &crate::RepositoryConfig,
    snapshot: crate::catalog::MembershipSnapshot,
) -> Result<Membership> {
    let organization = snapshot.organization.as_ref().map(|access| access.id);
    if principal.is_local() {
        return Ok(Membership {
            members: snapshot.members,
            groups: snapshot.groups,
            principal: principal.clone(),
            organization: None,
            fork_destination: None,
            fork_creator: None,
            baseline: snapshot.baseline,
            teams: Vec::new(),
            review_teams: Vec::new(),
            organization_owner: false,
            bypass_teams: Vec::new(),
            organization_bypass_teams: Vec::new(),
        });
    }
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let mut organizations = std::collections::HashMap::new();
    if !principal.is_anonymous()
        && !principal.is_deploy()
        && principal.automation_credential().is_none()
    {
        if let Some(source) = &snapshot.organization {
            organizations.insert(source.id, !source.teams.is_empty());
        }
        if let Some(destination) = snapshot.fork_destination {
            organizations.insert(destination, true);
        }
    }
    let mut grants =
        crate::repository::access::organization_grants(server, principal, organizations).await?;
    let inherited = crate::repository::access::snapshot_organization_access(
        &snapshot, &grants, principal, config, issuer,
    );
    let teams = snapshot
        .organization
        .map_or_else(Vec::new, |access| access.teams);
    let mut config = config.clone();
    config.members = snapshot.members;
    config.groups = snapshot.groups;
    let principal = if let Some(credential) = principal.automation_credential() {
        credential
            .authorize(
                server,
                principal,
                snapshot.fork_destination.or(organization),
                snapshot.repository_id,
                snapshot.ownership_epoch,
            )
            .await?
    } else {
        principal
            .with_catalog_membership(
                snapshot.repository_id,
                &config,
                issuer,
                inherited,
                snapshot.renamed,
                snapshot.fork_owner.as_ref(),
            )
            .with_public_visibility(snapshot.repository_id, snapshot.visibility.is_public())
    };
    let grant = organization
        .and_then(|id| grants.remove(&id))
        .unwrap_or_default();
    let destination_grant = snapshot.fork_destination.and_then(|id| grants.remove(&id));
    let (organization_owner, organization_bypass_teams) = destination_grant
        .map_or((grant.owner, grant.bypass_teams.clone()), |grant| {
            (grant.owner, grant.bypass_teams)
        });
    let mut review_teams = grant.teams;
    let mut bypass_teams = grant.bypass_teams.clone();
    // A bypass actor still needs a current repository grant and active,
    // non-secret membership. This applies to refreshed Git credentials too.
    bypass_teams.retain(|id| teams.iter().any(|grant| grant.team == *id));
    review_teams.retain(|id| teams.iter().any(|grant| grant.team == *id));
    Ok(Membership {
        members: config.members,
        groups: config.groups,
        principal,
        organization,
        fork_destination: snapshot.fork_destination,
        fork_creator: snapshot.fork_creator,
        baseline: snapshot.baseline,
        teams,
        review_teams,
        organization_owner,
        bypass_teams,
        organization_bypass_teams,
    })
}

pub(crate) fn actor(principal: &Principal) -> Result<Identity> {
    let mut identity = principal.identity().ok_or(Error::Forbidden)?;
    identity.name = identity.name.chars().take(160).collect();
    Ok(identity)
}

pub(crate) fn number(value: u64) -> Result<u64> {
    if value == 0 || value >= MAX_NUMBER {
        return Err(Error::NotFound);
    }
    Ok(value)
}

pub(crate) fn submission(value: &str) -> Result<String> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(Error::Invalid("Submission ID must be a UUID"));
    }
    let value = value.to_ascii_lowercase();
    tracing::debug!(
        target: "crab_server::action",
        event = "application_submission",
        submission_id = %value,
    );
    Ok(value)
}

pub(crate) fn submission_id(value: &str) -> Result<[u8; 16]> {
    uuid::Uuid::parse_str(&submission(value)?)
        .map(uuid::Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

pub(crate) fn title(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Title must contain 1–256 characters without control characters",
        ));
    }
    Ok(value.to_owned())
}

pub(crate) fn search_query(value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Search query must be at most 256 characters without controls",
        ));
    }
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    crate::cells::repository::search::Query::parse(Some(value)).map_err(Error::Invalid)?;
    Ok(Some(value.to_owned()))
}

pub(crate) const MAX_TEXT_BODY_BYTES: usize = 64 * 1024;

pub(crate) fn body(value: &str, required: bool) -> Result<()> {
    if value.len() > MAX_TEXT_BODY_BYTES
        || value.contains('\0')
        || (required && value.trim().is_empty())
    {
        return Err(Error::Invalid(
            "Content must be at most 64 KiB, without NUL characters; comments cannot be empty",
        ));
    }
    Ok(())
}

pub(crate) fn command_output<T>(
    result: crate::cells::Result<crate::cells::Committed<T>>,
) -> Result<T> {
    result
        .map(|committed| committed.output)
        .map_err(Error::Cell)
}

pub(crate) fn query_output<T>(
    result: crate::cells::Result<crate::cells::Observed<T>>,
) -> Result<T> {
    result.map(|observed| observed.output).map_err(Error::Cell)
}
