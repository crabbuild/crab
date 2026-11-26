// Generated from server HTTP types; do not edit by hand.
// Run npm run contracts:generate to update.

// prettier-ignore
export type SessionIdentity = { issuer: string, subject: string, name: string, };

// prettier-ignore
export type SessionMode = "local" | "oidc" | "github";

// prettier-ignore
export type Session = { authenticated: boolean, mode: SessionMode, user: SessionIdentity | null, csrf: string | null, can_sign_up: boolean, avatar?: AvatarConfig, };

// prettier-ignore
export type MergeQueueState = "waiting" | "preparing" | "candidate_scheduled" | "publishing";

// prettier-ignore
export type MergeQueueItem = { pull: number, title: string, request_id: string, head_oid: string, state: MergeQueueState, can_remove: boolean, can_prioritize: boolean, };

// prettier-ignore
export type MergeQueuePage = { items: Array<MergeQueueItem>, next: string | null, };

// prettier-ignore
export type BranchProtection = { branch: string, required_approvals: number, required_code_owner_reviews?: boolean, require_last_push_approval?: boolean, require_conversation_resolution?: boolean, require_up_to_date?: boolean, require_merge_queue?: boolean, require_linear_history?: boolean, allow_deletions?: boolean, required_checks: Array<string>, required_integrations?: Array<string>, };

// prettier-ignore
export type FilePreviewState = { "state": "inline", text: string, } | { "state": "omitted_size" } | { "state": "binary" } | { "state": "unsupported_encoding" };

// prettier-ignore
export type FilePreview = { oid: string, size: number, stored_size?: number, mode: string, classification: string, commit: string, generation: number, preview: FilePreviewState, };

// prettier-ignore
export type RepositoryAccess = "read" | "triage" | "moderate" | "write" | "maintain" | "admin";

// prettier-ignore
export type RepositoryMember = { subject: string, name: string, access: RepositoryAccess, };

// prettier-ignore
export type RepositoryGroup = { group: string, access: RepositoryAccess, };

// prettier-ignore
export type AccessUpdate = { expected_revision: number, members: Array<RepositoryMember>, groups: Array<RepositoryGroup>, };

// prettier-ignore
export type MembershipState = { revision: number, members: Array<RepositoryMember>, groups: Array<RepositoryGroup>, can_admin: boolean, groups_supported: boolean, };

// prettier-ignore
export type TeamGrant = { team: string, access: RepositoryAccess, };

// prettier-ignore
export type OrganizationAccess = { id: string, teams: Array<TeamGrant>, };

// prettier-ignore
export type TeamAccessState = { revision: number, organization: OrganizationAccess, team_names: { [key in string]: string }, };

// prettier-ignore
export type TeamAccessUpdate = { expected_revision: number, teams: Array<TeamGrant>, };

// prettier-ignore
export type RepositoryAssignee = { id: string, name: string, profile: ActorSummary | null, };

// prettier-ignore
export type AvatarVariant = "beam" | "marble" | "pixel" | "sunset" | "ring" | "bauhaus" | "geometric" | "abstract";

// prettier-ignore
export type AvatarConfig = { variant: AvatarVariant, seed: string, colors: Array<string>, };

// prettier-ignore
export type ActorProfile = { id: string, handle: string, name: string, version: number, created_at_ms: number, updated_at_ms: number, avatar?: AvatarConfig, };

// prettier-ignore
export type ActorSummary = { id: string, handle: string, name: string, avatar?: AvatarConfig, };

// prettier-ignore
export type OwnProfile = { profile: ActorProfile | null, avatar?: AvatarConfig, };

// prettier-ignore
export type AvatarResponse = { avatar: AvatarConfig | null, };

// prettier-ignore
export type ReviewAvailability = { version: number, busy: boolean, expires_at_ms: number | null, };

// prettier-ignore
export type ReviewAvailabilityChoice = "available" | "thirty_minutes" | "one_hour" | "four_hours" | "one_day" | "one_week" | "until_cleared";

// prettier-ignore
export type OrganizationRole = "member" | "owner";

// prettier-ignore
export type Organization = { id: string, name: string, description: string, version: number, created_at_ms: number, updated_at_ms: number, };

// prettier-ignore
export type InvitationState = "pending" | "accepted" | "declined" | "cancelled" | "expired";

// prettier-ignore
export type Invitation = { recipient: string, role: OrganizationRole, version: number, member_version: number, state: InvitationState, created_at_ms: number, expires_at_ms: number, reinstates?: boolean, };

// prettier-ignore
export type Reinstatement = { version: number, role: OrganizationRole, };

// prettier-ignore
export type InvitationView = { organization: Organization | null, invitation: Invitation | null, reinstatement?: Reinstatement, };

// prettier-ignore
export type OrganizationView = { organization: Organization, actor: string, role: OrganizationRole, can_create: boolean, };

// prettier-ignore
export type OrganizationPage<T> = { items: Array<T>, next: string | null, revision: number, };

// prettier-ignore
export type OrganizationMembership = { actor: string, role: OrganizationRole, version: number, profile: ActorSummary | null, };

// prettier-ignore
export type OrganizationMembers = { can_manage: boolean, items: Array<OrganizationMembership>, next: string | null, revision: number, };

// prettier-ignore
export type OutsideCollaborator = { key: string, name: string, profile: ActorSummary | null, repository_count: number, };

// prettier-ignore
export type OutsideCollaboratorRepository = { id: string, owner: string, name: string, access: RepositoryAccess, };

// prettier-ignore
export type IssueTypeColor = "gray" | "blue" | "green" | "yellow" | "orange" | "red" | "purple" | "pink";

// prettier-ignore
export type IssueType = { id: string, name: string, description: string, color: IssueTypeColor, enabled: boolean, };

// prettier-ignore
export type IssueTypeSchema = { version: number, definitions: Array<IssueType>, };

// prettier-ignore
export type ProjectAccessRole = "none" | "read" | "write" | "admin";

// prettier-ignore
export type OrganizationProjectWorkspace = { name: string, can_create: boolean, };

// prettier-ignore
export type OrganizationProjectPolicy = { version: number, role: ProjectAccessRole, affected_projects: number, scope: string, };

// prettier-ignore
export type AuditAction = "organization_created" | "automation_changed" | "organization_edited" | "organization_membership" | "organization_invitation" | "rulesets_changed" | "repository_properties_changed" | "issue_types_changed" | "project_changed" | "team_created" | "team_edited" | "team_group_sync" | "team_deleted" | "team_membership" | "team_creation_policy" | "repository_policy_changed" | "project_policy_changed" | "repository_registered" | "repository_access_changed" | "repository_transferred_in" | "repository_transferred_out" | "repository_deleted" | "repository_restored" | "repository_visibility_changed" | "outside_collaborators_removed" | "collaborator_grants_retained";

// prettier-ignore
export type AuditRepository = { id: string, name: string, };

// prettier-ignore
export type AuditEvent = { id: string, occurred_at_ms: number, action: AuditAction, actor: string | null, actor_profile: ActorSummary | null, operator: boolean, account: string | null, team: string | null, project: string | null, repository: AuditRepository | null, detail: string | null, };

// prettier-ignore
export type AuditPage = { items: Array<AuditEvent>, next: string | null, };

// prettier-ignore
export type AutomationAccess = "read" | "write";

// prettier-ignore
export type AutomationToken = { id: string, expires_at: number, };

// prettier-ignore
export type AutomationGrant = { repository: string, name: string | null, access: AutomationAccess, current: boolean, };

// prettier-ignore
export type AutomationIdentity = { id: string, name: string, enabled: boolean, grants: Array<AutomationGrant>, tokens: Array<AutomationToken>, };

// prettier-ignore
export type AutomationSnapshot = { version: number, identities: Array<AutomationIdentity>, };

// prettier-ignore
export type MemberAccessAccount = "active" | "suspended" | "provider_unavailable";

// prettier-ignore
export type MemberAccessTeam = { id: string, name: string, access: RepositoryAccess, };

// prettier-ignore
export type AccessRepository = { id: string, owner: string, name: string, access: RepositoryAccess | null, direct: RepositoryAccess | null, fork_creator: RepositoryAccess | null, teams: Array<MemberAccessTeam>, organization_owner: boolean, baseline: RepositoryAccess | null, sso_group_ceiling: RepositoryAccess | null, inherited_from: string | null, };

// prettier-ignore
export type MemberRepositoryAccess = { account: MemberAccessAccount, managed: boolean, items: Array<AccessRepository>, next: string | null, };

// prettier-ignore
export type OutsideRemovalImpact = { collaborators: number, repositories: number, grants: number, };

// prettier-ignore
export type OutsideRemovalPlan = { organization_revision: number, catalog_revision: number, collaborators: Array<string>, confirmation: string, };

// prettier-ignore
export type OutsideRemovalPreview = { plan: OutsideRemovalPlan, impact: OutsideRemovalImpact, repository_names: Array<string>, };

// prettier-ignore
export type InvitationOffer = { invitation: Invitation, profile: ActorSummary | null, };

// prettier-ignore
export type TeamVisibility = "closed" | "secret";

// prettier-ignore
export type TeamRole = "member" | "maintainer";

// prettier-ignore
export type Team = { id: string, name: string, description: string, visibility: TeamVisibility, parent?: string, version: number, created_at_ms: number, updated_at_ms: number, };

// prettier-ignore
export type TeamView = { team: Team, role: TeamRole | null, can_manage: boolean, ancestors?: Array<string>, tree_version?: number, };

// prettier-ignore
export type TeamSummary = { members: number, team: Team, role: TeamRole | null, can_manage: boolean, ancestors?: Array<string>, tree_version?: number, };

// prettier-ignore
export type TeamMembership = { actor: string, role: TeamRole | null, version: number, };

// prettier-ignore
export type TeamPolicy = { member_creation: boolean, version: number, };

// prettier-ignore
export type ReviewRoutingAlgorithm = "disabled" | "round_robin" | "fewest_open";

// prettier-ignore
export type ReviewRoutingOptions = { count_existing: boolean, keep_team: boolean, };

// prettier-ignore
export type ReviewRoutingSettings = { version: number, algorithm: ReviewRoutingAlgorithm, reviewers: number, excluded: Array<string>, options: ReviewRoutingOptions, };

// prettier-ignore
export type TeamHierarchySide = "child" | "parent";

// prettier-ignore
export type TeamHierarchyDecision = "approve" | "decline" | "withdraw";

// prettier-ignore
export type TeamHierarchyRequest = { id: string, child: string, child_name: string, parent: string, parent_name: string, side: TeamHierarchySide, can_review: boolean, can_approve: boolean, can_withdraw: boolean, };

// prettier-ignore
export type TeamHierarchyRequests = { items: Array<TeamHierarchyRequest>, next: string | null, organization_name?: string, };

// prettier-ignore
export type TeamRequestInboxItem = { organization: string, organization_name: string, request: TeamHierarchyRequest, };

// prettier-ignore
export type TeamRequestInboxPage = { items: Array<TeamRequestInboxItem>, next: string | null, };

// prettier-ignore
export type TeamCommand = { "kind": "create", name: string, description: string, visibility: TeamVisibility, parent?: string, } | { "kind": "edit", team: string, version: number, name: string, description: string, visibility: TeamVisibility, } | { "kind": "delete", team: string, version: number, hierarchy_version?: number, } | { "kind": "reparent", team: string, version: number, parent: string | null, } | { "kind": "propose_hierarchy", team: string, parent: string, version: number, side: TeamHierarchySide, } | { "kind": "decide_hierarchy", team: string, request: string, decision: TeamHierarchyDecision, } | { "kind": "member", team: string, member: string, version: number, role: TeamRole | null, } | { "kind": "review_routing", team: string, version: number, algorithm: ReviewRoutingAlgorithm, reviewers: number, excluded?: Array<string>, options?: ReviewRoutingOptions, } | { "kind": "policy", version: number, member_creation: boolean, };

// prettier-ignore
export type TeamSubmission = { request_id: string, command: TeamCommand, };

// prettier-ignore
export type TeamAcknowledgement = { "outcome": "applied", id: string, } | { "outcome": "cancelled", id: string, };

// prettier-ignore
export type TeamListing = { can_create: boolean, items: Array<TeamSummary>, next: string | null, revision: number, };

// prettier-ignore
export type TeamMember = { actor: string, role: TeamRole | null, version: number, profile: ActorSummary | null, inherited?: boolean, };

// prettier-ignore
export type TeamRoster = { can_manage: boolean, items: Array<TeamMember>, next: string | null, revision: number, };
