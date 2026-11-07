// Generated from server HTTP types; do not edit by hand.
// Run npm run contracts:generate to update.

// prettier-ignore
export type SessionIdentity = { issuer: string, subject: string, name: string, };

// prettier-ignore
export type SessionMode = "local" | "oidc" | "github";

// prettier-ignore
export type Session = { authenticated: boolean, mode: SessionMode, user: SessionIdentity | null, csrf: string | null, };

// prettier-ignore
export type BranchProtection = { branch: string, required_approvals: number, required_checks: Array<string>, };

// prettier-ignore
export type RepositoryAccess = "read" | "triage" | "write" | "admin";

// prettier-ignore
export type RepositoryMember = { subject: string, name: string, access: RepositoryAccess, };

// prettier-ignore
export type RepositoryGroup = { group: string, access: RepositoryAccess, };

// prettier-ignore
export type AccessUpdate = { expected_revision: number, members: Array<RepositoryMember>, groups: Array<RepositoryGroup>, };

// prettier-ignore
export type MembershipState = { revision: number, members: Array<RepositoryMember>, groups: Array<RepositoryGroup>, can_admin: boolean, groups_supported: boolean, };

// prettier-ignore
export type RepositoryAssignee = { id: string, name: string, profile: ActorSummary | null, };

// prettier-ignore
export type ActorProfile = { id: string, handle: string, name: string, version: number, created_at_ms: number, updated_at_ms: number, };

// prettier-ignore
export type ActorSummary = { id: string, handle: string, name: string, };

// prettier-ignore
export type OwnProfile = { profile: ActorProfile | null, };

// prettier-ignore
export type OrganizationRole = "member" | "owner";

// prettier-ignore
export type Organization = { id: string, name: string, description: string, version: number, created_at_ms: number, updated_at_ms: number, };

// prettier-ignore
export type InvitationState = "pending" | "accepted" | "declined" | "cancelled" | "expired";

// prettier-ignore
export type Invitation = { recipient: string, role: OrganizationRole, version: number, member_version: number, state: InvitationState, created_at_ms: number, expires_at_ms: number, };

// prettier-ignore
export type InvitationView = { organization: Organization | null, invitation: Invitation | null, };

// prettier-ignore
export type OrganizationView = { organization: Organization, actor: string, role: OrganizationRole, };

// prettier-ignore
export type OrganizationPage<T> = { items: Array<T>, next: string | null, revision: number, };

// prettier-ignore
export type OrganizationMembership = { actor: string, role: OrganizationRole, version: number, profile: ActorSummary | null, };

// prettier-ignore
export type OrganizationMembers = { can_manage: boolean, items: Array<OrganizationMembership>, next: string | null, revision: number, };

// prettier-ignore
export type InvitationOffer = { invitation: Invitation, profile: ActorSummary | null, };

// prettier-ignore
export type TeamVisibility = "closed" | "secret";

// prettier-ignore
export type TeamRole = "member" | "maintainer";

// prettier-ignore
export type Team = { id: string, name: string, description: string, visibility: TeamVisibility, version: number, created_at_ms: number, updated_at_ms: number, };

// prettier-ignore
export type TeamView = { team: Team, role: TeamRole | null, can_manage: boolean, };

// prettier-ignore
export type TeamSummary = { members: number, team: Team, role: TeamRole | null, can_manage: boolean, };

// prettier-ignore
export type TeamMembership = { actor: string, role: TeamRole | null, version: number, };

// prettier-ignore
export type TeamPolicy = { member_creation: boolean, version: number, };

// prettier-ignore
export type TeamCommand = { "kind": "create", name: string, description: string, visibility: TeamVisibility, } | { "kind": "edit", team: string, version: number, name: string, description: string, visibility: TeamVisibility, } | { "kind": "delete", team: string, version: number, } | { "kind": "member", team: string, member: string, version: number, role: TeamRole | null, } | { "kind": "policy", version: number, member_creation: boolean, };

// prettier-ignore
export type TeamSubmission = { request_id: string, command: TeamCommand, };

// prettier-ignore
export type TeamAcknowledgement = { "outcome": "applied", id: string, } | { "outcome": "cancelled", id: string, };

// prettier-ignore
export type TeamListing = { can_create: boolean, items: Array<TeamSummary>, next: string | null, revision: number, };

// prettier-ignore
export type TeamMember = { actor: string, role: TeamRole | null, version: number, profile: ActorSummary | null, };

// prettier-ignore
export type TeamRoster = { can_manage: boolean, items: Array<TeamMember>, next: string | null, revision: number, };
