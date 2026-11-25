use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    catalog::audit,
    cells::{
        directory::{ActorSummary, IdentityKey},
        organization,
    },
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) enum AuditAction {
    OrganizationCreated,
    AutomationChanged,
    OrganizationEdited,
    OrganizationMembership,
    OrganizationInvitation,
    RulesetsChanged,
    RepositoryPropertiesChanged,
    IssueTypesChanged,
    ProjectChanged,
    TeamCreated,
    TeamEdited,
    TeamGroupSync,
    TeamDeleted,
    TeamMembership,
    TeamCreationPolicy,
    RepositoryPolicyChanged,
    RepositoryRegistered,
    RepositoryAccessChanged,
    RepositoryTransferredIn,
    RepositoryTransferredOut,
    RepositoryDeleted,
    RepositoryRestored,
    RepositoryVisibilityChanged,
    OutsideCollaboratorsRemoved,
    CollaboratorGrantsRetained,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AuditRepository {
    pub(super) id: Uuid,
    pub(super) name: String,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AuditEvent {
    pub(super) id: String,
    pub(super) occurred_at_ms: u64,
    pub(super) action: AuditAction,
    pub(super) actor: Option<Uuid>,
    pub(super) actor_profile: Option<ActorSummary>,
    pub(super) operator: bool,
    pub(super) account: Option<Uuid>,
    pub(super) team: Option<Uuid>,
    pub(super) project: Option<Uuid>,
    pub(super) repository: Option<AuditRepository>,
    pub(super) detail: Option<String>,
}

pub(super) struct RawEvent {
    pub(super) event: AuditEvent,
    pub(super) identity: Option<IdentityKey>,
}

impl From<organization::Event> for RawEvent {
    fn from(value: organization::Event) -> Self {
        let mut event = AuditEvent {
            id: format!("organization:{}", value.revision),
            occurred_at_ms: value.created_at_ms,
            action: AuditAction::OrganizationCreated,
            actor: value.actor,
            actor_profile: None,
            operator: value.actor.is_none(),
            account: None,
            team: None,
            project: None,
            repository: None,
            detail: None,
        };
        use organization::Action;
        event.action = match value.action {
            Action::Created => AuditAction::OrganizationCreated,
            Action::RepositoryPolicyChanged => AuditAction::RepositoryPolicyChanged,
            Action::Automation { identity, action } => {
                use organization::automation::Audit;
                let label = match action {
                    Audit::Created => "Created",
                    Audit::Edited => "Access edited",
                    Audit::Enabled => "Enabled",
                    Audit::Disabled => "Disabled",
                    Audit::Deleted => "Deleted",
                    Audit::TokenIssued => "Token issued",
                    Audit::TokenRevoked => "Token revoked",
                };
                event.detail = Some(format!("{label} · {identity}"));
                AuditAction::AutomationChanged
            }
            Action::Edited { name, description } => {
                event.detail = Some(
                    match (name, description) {
                        (true, true) => "Name and description changed",
                        (true, false) => "Name changed",
                        _ => "Description changed",
                    }
                    .into(),
                );
                AuditAction::OrganizationEdited
            }
            Action::Membership {
                member,
                role,
                cancelled_invitations,
                ..
            } => {
                event.account = Some(member);
                event.detail = Some(membership_detail(
                    role.map_or("Removed", member_role),
                    cancelled_invitations,
                ));
                AuditAction::OrganizationMembership
            }
            Action::MemberConverted {
                member,
                cancelled_invitations,
                ..
            } => {
                event.account = Some(member);
                event.detail = Some(membership_detail(
                    "Converted to outside collaborator",
                    cancelled_invitations,
                ));
                AuditAction::OrganizationMembership
            }
            Action::Invitation {
                recipient,
                role,
                state,
                reinstates,
                ..
            } => {
                use organization::InvitationState;
                event.account = Some(recipient);
                let state = match state {
                    InvitationState::Pending => "Pending",
                    InvitationState::Accepted => "Accepted",
                    InvitationState::Declined => "Declined",
                    InvitationState::Cancelled => "Cancelled",
                    InvitationState::Expired => "Expired",
                };
                let kind = if reinstates { " · Reinstatement" } else { "" };
                event.detail = Some(format!("{state} · {}{kind}", member_role(role)));
                AuditAction::OrganizationInvitation
            }
            Action::RulesetsChanged { version } => {
                event.detail = Some(format!("Revision {version}"));
                AuditAction::RulesetsChanged
            }
            Action::RepositoryPropertiesChanged {
                repository,
                version,
            } => {
                event.detail = Some(match repository {
                    Some(id) => format!("Repository {id} · Revision {version}"),
                    None => format!("Definitions · Revision {version}"),
                });
                AuditAction::RepositoryPropertiesChanged
            }
            Action::IssueTypesChanged { version } => {
                event.detail = Some(format!("Revision {version}"));
                AuditAction::IssueTypesChanged
            }
            Action::ProjectChanged { project } => {
                event.project = Some(project);
                AuditAction::ProjectChanged
            }
            Action::TeamCreated { team } => {
                event.team = Some(team);
                AuditAction::TeamCreated
            }
            Action::TeamEdited { team } => {
                event.team = Some(team);
                AuditAction::TeamEdited
            }
            Action::TeamGroupSync {
                team,
                group,
                group_revision,
                change,
                updated,
                removed,
            } => {
                use organization::teams::sync::AuditChange;
                let label = match change {
                    AuditChange::Linked => "Linked",
                    AuditChange::Synchronized => "Synchronized",
                    AuditChange::Unlinked => "Unlinked",
                };
                event.team = Some(team);
                event.detail = Some(format!(
                    "{label} · Group {group} · Revision {group_revision} · {updated} added or updated · {removed} removed"
                ));
                AuditAction::TeamGroupSync
            }
            Action::TeamDeleted { team } => {
                event.team = Some(team);
                AuditAction::TeamDeleted
            }
            Action::TeamMembership { team, member, role } => {
                use organization::teams::Role;
                event.team = Some(team);
                event.account = Some(member);
                event.detail = Some(
                    match role {
                        Some(Role::Member) => "Member",
                        Some(Role::Maintainer) => "Maintainer",
                        None => "Removed",
                    }
                    .into(),
                );
                AuditAction::TeamMembership
            }
            Action::TeamCreationPolicy { member_creation } => {
                event.detail = Some(
                    if member_creation {
                        "Members can create teams"
                    } else {
                        "Owners can create teams"
                    }
                    .into(),
                );
                AuditAction::TeamCreationPolicy
            }
        };
        Self {
            event,
            identity: None,
        }
    }
}

fn membership_detail(action: &str, cancelled: u64) -> String {
    match cancelled {
        0 => action.to_owned(),
        1 => format!("{action} · 1 invitation cancelled"),
        _ => format!("{action} · {cancelled} invitations cancelled"),
    }
}
fn member_role(role: organization::Role) -> &'static str {
    match role {
        organization::Role::Member => "Member",
        organization::Role::Owner => "Owner",
    }
}

impl From<audit::Entry> for RawEvent {
    fn from(entry: audit::Entry) -> Self {
        let action = match entry.action {
            audit::Action::Registered => AuditAction::RepositoryRegistered,
            audit::Action::AccessChanged => AuditAction::RepositoryAccessChanged,
            audit::Action::TransferredIn => AuditAction::RepositoryTransferredIn,
            audit::Action::TransferredOut => AuditAction::RepositoryTransferredOut,
            audit::Action::Deleted => AuditAction::RepositoryDeleted,
            audit::Action::Restored => AuditAction::RepositoryRestored,
            audit::Action::VisibilityChanged => AuditAction::RepositoryVisibilityChanged,
            audit::Action::OutsideCollaboratorsRemoved => AuditAction::OutsideCollaboratorsRemoved,
            audit::Action::CollaboratorGrantsRetained => AuditAction::CollaboratorGrantsRetained,
            audit::Action::MemberGrantsRevoked | audit::Action::MemberGrantsRestored => {
                AuditAction::OrganizationMembership
            }
        };
        let operator = entry.actor.issuer == "urn:crab:local";
        Self {
            identity: (!operator).then_some(IdentityKey {
                issuer: entry.actor.issuer,
                subject: entry.actor.subject,
            }),
            event: AuditEvent {
                id: format!("repository:{}", entry.position.id),
                occurred_at_ms: entry.occurred_at_ms,
                action,
                actor: None,
                actor_profile: None,
                operator,
                account: None,
                team: None,
                project: None,
                repository: entry
                    .repository
                    .map(|(id, name)| AuditRepository { id, name }),
                detail: entry.outside_grants.map(|change| {
                    let description = match entry.action {
                        audit::Action::MemberGrantsRevoked => "Direct grants revoked",
                        audit::Action::MemberGrantsRestored => "Direct grants restored",
                        _ => {
                            return format!(
                                "{} collaborators · {} grants · {} repositories",
                                change.collaborators, change.grants, change.repositories,
                            );
                        }
                    };
                    format!(
                        "{description} · {} grants · {} repositories",
                        change.grants, change.repositories
                    )
                }),
            },
        }
    }
}
