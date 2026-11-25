# Crab v2 repository workflow parity

Audited 2026-11-21 at `a12fb2931fe`, verified against `origin/crab-v2`. The default remote branch is 106 commits behind this baseline. The `v2.0.0-alpha.1` tag points at `7e8aa458cc5`. The comparison covers the current v2 source, which includes changes after that tag.

## Implementation progress

- Compare and create now share merge-base reads and submit the inspected tips.
  Native submission decisions preserve both successful and stale attempts.
  Creation rechecks the inspected tips while holding the base and head Git ref
  leases through the Repository Cell decision, including on private fork PRs.
  Per-file diffs and inline review anchors now resolve one path at the pinned
  commits. A native 516-file fixture with diverged branches verified diff reads
  and review anchors under an entry budget the full comparison exceeds.
  Chromium opened the selected diff and rendered the persisted annotation.
- Protected merge checks direct reviewer grants, evaluates same-name statuses
  and check runs together, leases both refs and reconciles with a durable receipt.
  The prior tag's pending records still use their tagged retry contract.
- Branch protection and branch rulesets can require independent approval of
  the latest push. A review binds to the verified Git head publication and
  merge base. The merge panel reports pending approval or missing push
  attribution, and publication checks the reviewer's current access.
- Branch rulesets can allow topic-branch force pushes and block rewrites on
  matching branches. Git publication checks current rules, exact predecessor
  refs, and locked paths. A repeated wire request uses current ref
  transactions if a branch returns to its former tip.
- Merged pull requests retain access to check runs and commit statuses for
  the exact reviewed head and, when published, the tested candidate. The
  Checks tab separates these commits after squash or rebase and branch
  cleanup. Files changed, inline review navigation, and paginated original
  commits use the pre-merge comparison while its objects remain in storage.
  Commit links open individual diffs within the recorded PR range. The active
  v2 inventory preserves the original objects through pack replacement and
  recovery-history pruning.
- Open pull requests refresh required head checks and candidate progress while
  their browser view is visible. Checks stay attached to the exact head or
  candidate, and the selected run log refreshes with its recorded version.
  Conversation refreshes merge readiness after CI passes, updates the reviewer
  sidebar for approval or head changes, and disables outdated merge
  confirmations while preserving the author's draft.
- PR lists read transactionally maintained check summaries for their selected
  commits. Historical reports backfill in bounded batches, and new reports
  stay current during preparation.
- Writers can restore a missing head branch from a completed pull request.
  The panel shows the saved commit. Publication checks the current source
  repository, write grant and ref rules, and requires the branch name to be
  unused. Git objects must still be retained. Completed PRs can also delete
  a head that still matches the recorded commit. Current protection and open
  base or head dependencies can prevent deletion. The source repository can
  opt in to automatic cleanup after merge. For a same-repository merged PR,
  cleanup can move eligible dependent PRs to the recorded base before deleting
  the old branch.
- Reopening a closed PR checks both branch names under publication leases,
  including through the bulk and direct update paths. A rejected reopen rolls
  back its comment or other bulk edits in the same native transaction.
- Viewed progress now persists per account and PR, keyed to file diff identity.
- Release notes process merged pull requests in pages using the pinned Git
  range. A native fixture added 4,096 historical merge records, then published
  the selected release in Chromium. The same fixture verified that a draft
  covering all history reaches the 64 KiB output guard.
- Writers can mark repository issues to close when their linked PR merges into
  the default branch. Merge completion and the issue lifecycle update share the
  same transaction, and the issue timeline links back to the PR.
- PR descriptions can close issues in the target repository with closing
  keywords. Both sidebars show the relationship on the default branch. The
  merge snapshot preserves completed relationships through description edits.
- New issue reads Markdown and YAML templates from the default branch. The
  chooser supports contact links and maintainer-only blank issues. Structured
  answers become Markdown and issue creation applies verified template defaults.
  Organization and personal repositories can inherit these defaults from a
  readable `.github` repository under the same account. A valid local template
  or config overrides the inherited chooser.
- Pending review comments and suggestions persist privately across reloads.
  One submission publishes the review and comments with a durable retry receipt.
  The discussion editor retains Markdown commands, saved replies, references
  and repository-scoped attachments.
- Writers can apply selected suggestions across several files in one commit.
  The head, comment versions, file blobs, and line ranges are checked before publication.
  The originating browser tab retains the exact request across reloads, so a
  writer can recover its commit after a lost reply and a later head advance.
- Browser creation provisions empty private repositories in personal and
  organization namespaces. Organization owners inherit current Admin access.
  Retrying setup recovers the same catalog UUID after a lost response.
- Git imports keep their job and repository identity across worker restarts.
  Crab checks the recorded ref tips before exposing the destination. A resumed
  job can finish from committed refs or clone the same snapshot again. Source
  tokens stay in memory. Organization imports use current owner authority and
  support recovery by another owner from the shared job link.
- Organization owners can fork a private repository inside the same organization.
  The fork links to its source UUID, inherits current source ACLs, and supports
  independent native Git pushes. Creation uses the durable Git intake job,
  which copies and verifies reachable Git LFS payloads and publishes verified
  native Xet file bindings from shared origin storage with the refs.
- Fork writers can preview and fast-forward a same-named upstream branch in the
  browser. The submission binds both tips and uses normal Git rules and durable
  receipt recovery. Divergence is surfaced without changing either branch.
- Fork readers can compare a selected branch against an upstream branch in the
  browser. Changed files use the common Git ancestor and identify both captured
  tips. Contributors with Write access to the fork and Read access upstream can
  open a pull request from that comparison or a recent push to the fork. The
  upstream compare form also offers searchable writable forks and preserves the
  draft when the contributor switches head repositories. Teams
  can review the exact fork commit, report scoped Crab checks upstream, and publish
  merge commits, squash, rebase, or fast-forward through the upstream's merge
  policy. Writers can apply review suggestions to the fork branch, with one
  commit, an editable message, and credit for suggestion authors. The Checks
  tab shows a GitHub Actions mirror refspec for the exact fork head. A signed
  test delivery to the native server unlocked a protected merge in Chromium.
- Repository code search now matches literal text on a selected snapshot,
  with directory and case filters, bounded pages, and commit-pinned line links.
  Workspace search covers readable default branches with persistent path and
  text filters, pinned links, and visible indexing status. Both search views
  offer parser-backed definitions verified against the pinned commit. Workspace
  search prunes files with persistent definition filters. Both searches accept
  repository, organization, language, and path filters, plus explicit content
  and symbol terms. Regular expressions work across content, paths, and
  definitions. Both searches support Boolean expressions with grouped,
  quoted, and scoped terms. The index rejects candidates using trigrams
  required by the parsed query, including Unicode-aware case folding. Larger
  files have larger filters to keep selective queries from opening unrelated
  blobs. A native fixture covers a 4,097-path regex search across an empty page,
  and a mixed-case fixture verifies Unicode results against Git. The pinned
  Next.js, Neovim, and ripgrep fixture measures search over 38,135 indexed
  paths. The same selective literal dropped from 1,235 Git content
  verifications across 38 pages to 110 across four.
- The account-wide in-app inbox delivers assignments, individual and team
  review requests, individual mentions, and configured closed-team mentions.
  Read state is durable, the inbox can filter by reason, and links target the
  specific contribution. Watch settings subscribe to future issue activity,
  pull request activity, or both. Release watching is independent and sends
  notifications when publication commits. Muting hides repository activity from the
  inbox and unread badge. Issue and pull request sidebars let readers follow
  all future discussion activity, follow state changes, unsubscribe, or inherit
  repository preferences. Authors, commenters, reviewers, and users who
  change discussion state automatically follow future activity. Publishing
  any of those actions also restores a follow after unsubscribing. New
  mentions and assignments also follow future activity after the worker
  resolves eligible recipients. Later replies wait for that decision, and a
  newer private choice takes precedence. The inbox groups updates by issue or
  PR, keeps the strongest reason, and links to the newest contribution. Read
  and triage changes use the activity version shown on the row. Saved and
  Done have durable views. A newer update reopens the conversation and keeps
  its bookmark. Bulk triage supports selections across loaded pages and
  repositories, and a stale row cancels the batch for read or filing changes.
  Inbox unsubscribe dismisses observed activity alongside the Repository watch
  preference and reports an outcome for each selected conversation. Saved
  bookmarks survive. New mentions and review requests reopen conversations.
  Watched repositories lists the account’s current issue, pull request, and
  release watches and lets readers edit or stop watching from Notifications.
  Bulk unwatch previews a selected owner's repository watches across pages and
  applies each confirmed change with its captured version. Subscriptions lists
  followed issues and pull requests, including quiet discussions, and lets
  readers filter and edit them from Notifications. Bulk unsubscribe stops
  following selected discussions across loaded pages and retains past inbox
  activity. A bounded Directory sweep cleans up expired Done entries while
  preserving bookmarks and suppressing older replays. Subscription controls
  filter by recorded reason and sort by follow time across accessible
  repositories. Historical follows with no recorded dates appear last.
  Live Resend delivery qualification remains open.
  [GitHub subscription management](https://docs.github.com/en/subscriptions-and-notifications/how-tos/managing-subscriptions-for-activity-on-github/managing-your-subscriptions)
- GitHub Actions reports signed push-workflow results for the PR head and can
  run required workflows automatically for queued candidates. The dispatch is
  scoped to the operator's repository, workflow, and branch binding. Candidate
  checkout and check results use the exact queued SHA. Signed outbound Git and
  PR webhooks include exact head changes on open PRs, including updates from
  private forks. Review subscriptions deliver signed events for published
  reviews, summary edits, and explicit dismissals with the reviewed commit
  and exact review revision. Private draft saves generate no delivery.
  Live GitHub qualification remains open.
- Repository admins can configure fast-forward, merge commit, squash and rebase.
  Squash and rebase use the protected merge and retry path. Rebase compares the
  final tree with the three-way merge result before publishing.
- Writers can update a PR head from its current base with a merge commit. The
  action works in the same repository and on private forks. Publication leases
  both tips, applies head-branch policy and records an idempotent receipt.
- Writers can resolve overlapping text changes in the browser and commit the
  results to the PR head. Conflict previews and edits bind the exact base and
  head and use the same protected branch update and retry receipt. Private
  fork resolution was verified in Chromium with a stock Git fetch and a lost
  response. Binary and structural conflicts use Git.
- Writers can rebase the pull request head on the inspected base in the browser.
  Crab checks the replayed tree, applies current head policy, and records the
  result through the same ref leases and retry receipt. It replaces visibility
  when the old head is no longer reachable. Native Git rewrites require an
  active allow rule for the branch.
- Writers can resolve a protected-head conflict on a new branch in the head
  repository and switch the existing PR to it. Branch creation and the PR
  switch have durable receipts. The switch records the head change, expires
  prior approvals, and checks the captured tips before changing the PR.
- Repository admins can enable auto-merge. Writers schedule an inspected PR
  head and method, and the server recovers waiting requests after startup.
  A request follows head updates when every committed transition came from
  someone with durable Write access to the target repository. The worker
  rechecks the scheduled writer, the new head, and current protection before
  publication. A missing link, base switch, or replacement head branch
  requires fresh consent.
- PR Checks can prepare a prospective two-parent merge commit on a server-owned
  Git ref. Stock Git can fetch the candidate. A configured GitHub Actions push
  workflow can report a signed result on its exact mirror branch. Branch moves
  invalidate the candidate, and its check results are separate from PR head
  results. Writers can publish the exact tested commit once both the head and
  the candidate pass current branch requirements. Promotion uses the existing
  protected publication and receipt recovery path. The repository can opt in
  to FIFO publication per branch. The worker discovers durable entries and
  prepares up to 100 candidates per branch using the live base and earlier
  candidate commits. Prefix validation reads bounded Cell pages and a pinned
  Git snapshot. Queue changes invalidate affected builds, and a successor
  can retain its results when its predecessor publishes the tested SHA.
  Administrators can require this queue for exact
  classic protections and matching repository or organization branch rulesets.
  The effective policy applies when preparing candidates and at Git publication,
  including retries of unpublished direct merges.
  On required branches, writers can request **Merge when ready**. A durable
  request waits for the exact head to pass current reviews and checks before
  receiving a queue position and undergoing candidate CI.
  Required workflows can start automatically within the configured build limit.
  Candidate checks show the bound run and its GitHub Actions link. Repository
  settings choose merge commit, squash, or rebase for queued changes while
  preserving publication of the tested SHA. Exact branch overrides inherit
  unset fields and can independently select the method, build limit, and
  check timeout.
- Classic branch protection and branch rulesets can require the head to contain
  the current base and pass required checks on that exact head. Merge admission
  and publication evaluate the same policy for same-repository and fork PRs.
- Authors and writers can preview and change the target branch of an open PR.
  The mutation verifies both tips, records the change, expires active approvals
  and binds new reviews to the updated base revision. Fork PRs select the new
  target in the upstream repository.
- Organization owners can grant roles to their teams from repository settings. Browser and
  Git access and approval eligibility use current memberships. The PR picker
  supports team review requests and links to the fulfilling review. PR search
  finds outstanding requests by visible team or by the viewer's current team
  memberships. Automatic assignment, organization creation policy and base
  permissions remain open.
- Search by a person's open team review requests in repository and workspace
  search. Named review-request searches include direct requests and visible
  team requests. The direct-only and team-only qualifiers work independently,
  and pagination rechecks current team visibility and membership.
- Repository and workspace discussion search support exact target branch and
  source branch prefix filters. Repository search suggests both qualifiers.
- Repository and workspace discussion search filter pending, successful, and
  failed checks using the same exact-commit totals shown in PR lists. The
  filter includes live fork heads and completed merges. CI and source changes
  invalidate continuation cursors.
- Review status search uses the same captured upstream and fork heads as check
  search and PR list badges. A push to a fork can expire its approval while an
  approval on an identically named upstream branch stays current. Repository
  and workspace pagination bind the current review policy and captured refs.
- CODEOWNERS evaluates the base-commit policy against the merge-base diff.
  The UI shows owner review coverage, and branch protection rechecks required
  approvals before merge publication. Ready PR creation and transitions out
  of draft request owners and deliver deduplicated inbox notifications.
  The repository file browser links ownership to the rule at the displayed
  commit. Merged PRs show ownership from the saved comparison and open the
  exact policy text after branch cleanup. Verified email owner lookup remains
  open.

- Team maintainers can distribute review requests by repository-scoped
  rotation or fewest open requests and exclude selected members from both
  methods. Reviewers can set an expiring or indefinite Busy status for
  automatic assignment. Team routing can count eligible pending requests and
  keep the team on a pull request after individual assignment.
  Organization-wide reviewer balancing remains open.
  [Team review settings](https://docs.github.com/en/organizations/organizing-members-into-teams/managing-code-review-settings-for-your-team)

## Platform investments

| Investment | First production milestone | Prerequisites |
| --- | --- | --- |
| Hosted and self-hosted runners | Schedule and cancel an isolated CI job for an exact PR integration commit, then publish logs and its check result. | Scoped installations, event delivery, check provenance, workload isolation, secret access policy and quotas. |
| Package hosting | Publish and retrieve an immutable version for a private package through one conventional native package client. | Namespace authority, scoped publish credentials, durable manifests, accounting, artifact integrity and retention. |
| Enterprise administration | Provision and remove an organization member, observe effective repository access and export the recorded audit decision. | Organization-owned namespaces and team grants, a durable identity lifecycle, revocation tests and audit retention. |

## Target

Make Crab a day-to-day Git host for private teams. A contributor should be able to find work, create or import a repository, push a branch, propose a change, get reviewed, pass CI, merge and publish a release using the browser and stock Git. An administrator should be able to explain and audit who can act and what is blocking a change. Use GitHub’s workflow conventions for navigation, keyboard behavior, deep links and state feedback. Use Crab’s existing Geist theme and repository components. Public collaboration, hosted automation, enterprise administration and API compatibility each require their own acceptance scope.

Crab already has smart HTTP Git and LFS, ref and file browsing, commit history, blame, browser file changes, issues, discussion search and bulk triage, draft PRs, reviewers, inline threads and single suggestions, check runs, protected branches, releases with assets, organization teams and Projects. Extend these paths. The earlier parity documents capture many delivered slices and their proof. Current routes and owners resolve any stale inventory statements.

`crates/crab-server/src/api/mod.rs:459`, `crates/crab-server/REFERENCE.md:1094`, `crates/crab-server/README.md:649`, `packages/ui/src/features/repository/page.tsx:364`, `packages/ui/src/features/pull-requests/detail.tsx:160`, `crates/crab-server/src/api/releases/tests.rs:277`, `crab/docs/architecture/github-ui-parity.md:736`, `crab/docs/architecture/enterprise-server-ui.md:69`.

## Gap matrix

Effort is relative to the existing architecture. M is a bounded server and UI slice. L spans persistence, authority or multiple clients. XL adds a major cross-repository or platform capability. The risk column rates the change, especially its effect on deployed contracts. Observations below have direct source evidence or an explicit design boundary. Deeper operational risks have dedicated verification cases below.

| Priority | Workflow | Current behavior and gap | Delivery target | Effort / risk |
| --- | --- | --- | --- | --- |
| P0 | Compare and open a PR | Compare and PR file views use the merge base. Opening a PR from the browser binds submission to the reviewed base and head. The isolated RustFS corpus compares exact Next.js and Neovim trees against stock Git and exercises browser reconfirmation after a head push. The deep-history fixture checks comparison and PR creation with 10,050 shared commits and verifies that new commit cursors preserve topic lane order when the ancestry index becomes ready. `packages/ui/src/features/pull-requests/create.tsx`, `crates/crab-server/src/api/query.rs`, `crates/crab-server/src/api/pulls/tests/real_comparison/deep_history.rs` | Qualify more complex histories and the target deployment. Unindexed ranges without a bounded ancestry proof use the full base walk within the existing limits. | M–L / high |
| P0 | Review and merge with confidence | Protected merge evaluates current eligible reviewers and reads required reports for the exact head and tested candidate in one Repository Cell transaction. The merge panel shows both reports for a shared context and the commit that must pass. Review views follow the fork's source ref and the sidebar shows decision eligibility. Ref leases and publication receipts support recovery. `crates/crab-server/src/api/pulls/required_checks.rs`, `crates/crab-server/src/api/pulls/reviewers/mod.rs`, `crates/crab-server/src/api/pulls/merge/mod.rs` | Extend fault and live external CI qualification. Track additional protection policy in P2. | L / high |
| P1 | Review a substantial change | Private pending review, atomic submission, revision-aware viewed progress and multi-file suggestion batches are available. Reviewers can filter file paths, hide viewed files or whitespace, and jump to a thread annotation hidden by these choices. Exact conversation totals and previous and next unresolved controls cover threads past the loaded page and across revisions. Cross-page batches retain selected versions and show affected paths and lines. A 20-suggestion private fork batch uses one selection read for preparation and one for publication verification, and the native Chromium fixture recovers its original commit after a lost reply. Viewed progress loads a consistent snapshot across pages and retains existing marks. The native Chromium corpus covers the complete 1,179-file Next.js review and inline review on the 536-file Neovim change. `crates/crab-server/src/api/pulls/review_threads`, `packages/ui/src/features/pull-requests/reviews` | Measure cross-page review and publication latency on the target deployment. | L / medium–high |
| P1 | Create, import and find a repository | Personal and organization browser creation share durable setup. The home page and organization tabs search readable repositories by name, owner, or description and page the results on the server. The home and organization catalogs filter by archive and fork state. Organization search combines those filters with typed custom-property qualifiers. Filtered continuation tracks readable candidates, the property revision, and the archive revision at the page boundary. Repository administrators can rename or transfer a standalone repository while retaining its UUID, Git placement, and old addresses. Transfers require acceptance from the named account or a destination organization owner. Eligible recipients can open pending transfers from Notifications. Personal recipients can also receive an opt-in email alert. Git imports resume by job ID, verify the pinned refs before exposing a destination and exclude LFS object bytes. `crates/crab-server/src/api/catalog.rs`, `packages/ui/src/features/organizations/repositories.tsx`, `crates/crab-server/src/git_import/jobs.rs` | Qualify transfer email with a deployed provider and scope private fork-network transfers. | L / high |
| P1 | Get back to requested work | My work provides account-wide lists of authored and assigned issues and pull requests, plus individual and team review requests. The lists use current repository access and server-side pagination. Workspace search finds issues and PRs by discussion query across readable repositories, with optional owner or repository scope and creation or activity sorting. The inbox groups assignments, review requests, mentions, discussion watch activity, and release publications. Rows offer reason filters, private read and filing state, latest contribution links, versioned bulk triage, and inbox unsubscribe. Dedicated views manage repository watches, bulk unwatch by owner, and discussion follows with bulk unsubscribe. Opt-in email delivers generic inbox links to a verified sign-in address. `crates/crab-server/src/api/notifications/mod.rs`, `crates/crab-server/src/server/email.rs`, `packages/ui/src/features/notifications/index.tsx` | Qualify delivery against an operator-provisioned Resend tenant. Evaluate digests and email replies separately. | L / medium–high |
| P1 | Find code and symbols | Repository code search reads a pinned branch, tag, or commit. Workspace search uses a persistent path and trigram-filter index for readable default branches, verifies matches against Git, and shows index use and scan status. Continuations bind the query and readable repository set. Go to file and current-file Symbols are available. Both search views find definitions for eight languages. Cold indexing and a four-client mixed search are measured on five pinned repositories. `crates/crab-server/src/api/code_search/`, `crates/crab-server/src/repository/code_index/` | Reduce the cold build cost and qualify latency and index readiness on a larger multi-tenant deployment. | L / high |
| P1 | Connect external CI | GitHub Actions workflow results use repository-scoped credentials, signature verification, durable redelivery receipts and provider-bound merge requirements. PR checks link to the GitHub run. Required workflows can dispatch from the queue with a candidate-scoped Git checkout. Repository admins can inspect recent accepted deliveries, their exact commit and attempt, and whether the check changed. Up to 20 operator-approved outbound receivers per repository receive signed stock Git and browser ref updates and PR lifecycle events with independent subscriptions, signing secrets, retries, and redelivery. Repository admins can issue and individually revoke named, expiring deploy tokens for private Git and LFS checkout. `crates/crab-server/src/api/integrations/mod.rs`, `packages/ui/src/features/settings/integrations.tsx`, `crates/crab-server/src/server/webhooks.rs`, `packages/ui/src/features/settings/webhooks.tsx` | Qualify a deployed GitHub App with live deliveries. | L / high |
| P2 | Govern and integrate changes | Merge methods are configurable and include squash and rebase. PR writers can merge the current base into the head branch, rebase onto it, or resolve simple text conflicts in the browser, including on a replacement branch for protected heads. Team grants support Read, Triage, Moderate, Write, Maintain, and Admin. Moderate can edit issues and issue comments with editor attribution and manage label and milestone definitions without code write access. Moderate can lock issue conversations and PR locks require Write. Locks cover public contributions and reactions, preserve private review drafts, and record activity. Moderators can set a deadline, and readers can filter issues and PRs by effective lock state. Moderators can hide or restore published comments, reviews, and inline replies with reasons. Readers can expand hidden contributions. Edit history compares retained descriptions and contributions, including inline suggestions, and authors or writers can remove historical text with attribution. Maintain covers repository descriptions, topics, and ordinary merge settings across direct, team, and signed group grants. Repository topics appear on repository pages and support permission-aware name and count filters in the home and organization catalogs. Repository settings, delegated properties, and PR team review requests use current membership. Teams can rotate assignments or balance open and recent review requests across the organization. Both algorithms skip excluded or Busy members. CODEOWNERS requests owners, can require eligible approvals by changed path, and appears on repository files. Repository and inherited rulesets layer active branch and tag patterns with classic protection, including ref update restrictions at Git HTTP and browser publication. Either branch policy can require all published inline review conversations to be resolved and commits to follow a linear history. Repository and organization rulesets support current role and visible team bypass. Organization rulesets target all, selected, name-matched, or property-matched repositories and layer with local policy. Property targets combine custom values with current catalog names and source or fork type. Name targets can protect the current repository name during renames and name-changing transfers. Owners can define typed repository properties and defaults, delegate individual values to current repository administrators, and atomically edit overrides for up to 50 repositories under the publication policy lease. `crates/crab-server/REFERENCE.md`, `crates/crab-server/src/api/pulls/branch_update.rs`, `crates/crab-server/src/api/pulls/codeowners/mod.rs` | Qualify review routing on large organizations, including simultaneous requests. Add cross-repository issue transfer, delegated team-grant management, language and deployment-context ruleset targeting, required property values at creation and transfer, app-scoped property editing, and integration bypass authority. Add batched publication and qualify wider build concurrency under production load. | XL / high |
| P2 | Triage, plan and release | Issues, labels, milestones, pins, bulk edits, linked work, default-branch issue closure through manual links, PR descriptions, and published commit messages, Markdown and YAML issue templates with inherited account defaults, sub-issues, same-repository issue dependencies with live blocked indicators on issues and Projects, duplicate issue and PR marking with timeline undo, discussion attachments, shared Projects, release assets and Git-range-based editable release notes with configurable categories and exclusions are available. `crates/crab-server/README.md:649`, `crates/crab-server/REFERENCE.md`, `crates/crab-server/src/api/issues/templates/mod.rs` | Cross-repository issue links and dependencies, issue types, remaining moderation controls, and Projects automation. Prioritize the remaining closure paths before adding more planning surfaces. | L / medium–high |
| P2 | Contribute across repository boundaries | Private organization forks use stable source UUIDs, independent refs, current source ACLs, verified Git LFS copies, and verified native Xet bindings. The browser compares fork and upstream branches from their common ancestor, opens upstream PRs, applies review suggestions and text conflict resolutions to the fork, and supports the upstream's enabled merge methods. A GitHub Actions mirror branch lets the upstream require results for commits present only in the fork. `crates/crab-server/src/api/pulls/git.rs`, `crates/crab-server/src/api/integrations/mod.rs`, `crates/crab-server/src/git/receive/fork_merge.rs` | Qualify review search against large fork networks and add policy for wider fork networks. Public visibility introduces a separate policy for anonymous browse, clone, search, fork access and abuse controls. | XL / high |
| P2 | Review large assets | Native Crab and Git LFS files show their stored size, stream authorized downloads, and preview recognized formats on request at the displayed commit. The Parquet workbench uses byte ranges. Whole-file previews enforce format size limits, and raw Git pointer bytes remain available. `crates/crab-server/src/api/artifact_content.rs`, `packages/ui/src/features/repository/files/index.tsx` | Add representative dataset or model comparisons. | L / high |
| P3 | Extend the platform | The enterprise inventory identifies gaps in hosted workflow execution, package hosting, and other platform areas. `crab/docs/architecture/enterprise-server-ui.md` | Scope hosted runners, Packages, Pages, Discussions, Wiki, Insights, public discovery, SSH, and broader API compatibility as independent programs. See the delivery order below for enterprise access, audit, and security priorities. | XL / high |

Crab v2 uses separate issue and PR sequences, including at the alpha tag. A repository can therefore contain both issue `#123` and PR `#123`. Closing keywords resolve the issue number in the target repository. Future Markdown autolinks and cross-repository references need to preserve typed link destinations. `crates/crab-server/src/cells/schema.sql:8`, `crates/crab-server/src/cells/repository/collaboration/links/mod.rs`, `crates/crab-server/src/cells/repository/pulls/merge.rs`. [GitHub work numbering](https://docs.github.com/en/rest/using-the-rest-api/issue-event-types), [closing linked issues](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/linking-a-pull-request-to-an-issue)

## Team and enterprise delivery order

Prioritize the workflows needed to change, govern, and operate private code
with an external identity provider and CI. This order reflects workflow
criticality and adoption dependencies.

| Priority | Remaining delivery | Acceptance evidence |
| --- | --- | --- |
| P0 | The two-process RustFS gate uses Chromium for PR creation, review submission, and protected merge. The browser shows the required check provider and commit, blocks a revoked reviewer, and discards the successful merge reply. The test replays the browser's exact request on the surviving replica, verifies the merged browser view, and fetches the published commit with stock Git. Repeat on the target deployment with a live GitHub App. | With a deployed App and object store, prove that a revoked reviewer, an old head, and a wrong check provider cannot satisfy policy. Force a lost publication reply and peer handoff, then fetch the intended merge with stock Git. [Required check behavior](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks) |
| P0 | Qualify enterprise operations on the first supported deployment. RustFS fixtures restore exact live object versions and prove SCIM revocation of existing browser and Git credentials across two server processes and a restart. Cover authenticated ingress, least-privilege storage access, revocation, and rollback on the target deployment. | Restore an isolated repository and its access records on a fresh deployment. Prove the old browser and Git credentials are denied across replicas after deprovisioning, and measure recovery time and data loss against the operator's targets. [Enterprise qualification](../crates/crab-server/CELLAR.md) |
| P1 | Organization owners can filter and export committed SCIM team links, roster updates, and unlinks. The RustFS gate verifies automation token revocation across two Git-serving processes, including a push admitted before revocation and denial after restart. Qualify provisioning and group synchronization with one live IdP and repeat Git revocation qualification on the target deployment. Extend audit coverage to additional security events, streaming, and enterprise-wide views. | An administrator provisions or disables a user via one IdP and sees effective repository access and the audit decision. Prove exact subject mapping, cross-replica revocation, group reconciliation, service credential revocation, and administrator-only export. [SCIM provisioning](https://docs.github.com/en/enterprise-cloud@latest/admin/managing-iam/provisioning-user-accounts-with-scim) · [Enterprise audit](https://docs.github.com/en/enterprise-cloud@latest/admin/concepts/security-and-compliance/audit-log-for-an-enterprise) |
| P1 | Scale the daily review and discovery paths. The isolated RustFS qualification measures cold indexing and four concurrent search modes on five pinned repositories. It also measures comparison and individual file reads over two large divergent changes. Repeat these workflows on a larger multi-tenant deployment and qualify large fork review batches. | Track cold and concurrent latency, object-store reads, and index readiness. Preserve commit-pinned results and current access across browser navigation and continuation. [Code search syntax](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax) |
| P2 | Add enterprise security findings and stronger governance. Start with SARIF ingestion tied to exact commits, then add secret push protection, required commit signatures, and policy-managed exceptions. | An authorized scanner publishes a bounded result for an exact commit. The PR shows new findings, the configured protection gates publication, and an allowed exception appears in audit. [SARIF ingestion](https://docs.github.com/en/code-security/how-tos/find-and-fix-code-vulnerabilities/integrate-with-existing-tools/upload-sarif-file) |
| P3 | Scope hosted runners, package registries, Pages, Wikis, and broader API compatibility separately. | Each program has an explicit isolation, authorization, retention, and availability contract. |

## First milestone

Prove one private repository from an author’s browser and stock Git through a second account’s review, external CI, a protected merge and an authorized Git fetch that sees the expected result. Use an isolated catalog, real authentication, the embedded UI and real object storage in a dedicated test environment. Retain evidence for both successful and rejected actions. Put a browser-visible slice in CI from the start, then extend the same fixture as each capability lands.

### 1. Make comparison authoritative

- **Trace.** Compare creation and the PR file tab feed the shared `ComparisonView`, which sends `rev=head&base=base`. The API resolves both trees directly. Thread creation and freshness checks also use that comparison. Merge publication uses live base and head tips to guard the ref update. `packages/ui/src/features/pull-requests/create.tsx:217`, `packages/ui/src/features/pull-requests/reviews/threads.tsx:168`, `packages/ui/src/features/repository/diff/index.tsx:21`, `crates/crab-server/src/api/query.rs:277`, `crates/crab-server/src/api/pulls/review_threads/mod.rs:267`, `crates/crab-server/src/api/pulls/merge/mod.rs:337`.
- **Build.** Define the PR comparison identity in Rust with repository UUID, selected refs, exact tips and the comparison base OID. Return a typed contract for preview, files and anchor validation. Compare from the merge base and document the PR’s historical comparison behavior when refs move or disappear. Use a bounded Git-compatible graph operation. The existing private merge helper uses summed path distance, which can choose an ancestor of a better common ancestor in a graph with redundant parent edges. Test the merge-commit sibling against stock Git before sharing graph logic. Keep generic two-tip history/commit APIs and live ref preconditions available to their callers. Check the tagged inline-thread payloads before changing their stored meanings. `crates/crab-server/src/api/pulls/merge/tree.rs:68`, `crates/crab-server/src/api/pulls/review_threads/mod.rs:410`. [Git merge-base contract](https://git-scm.com/docs/git-merge-base), [GitHub branch comparison](https://docs.github.com/en/pull-requests/reference/branches), [GitHub PR behavior](https://docs.github.com/en/pull-requests/reference/pull-requests)
- **Complete when.** A divergent history returns only topic changes in the browser and agrees with stock `git diff base...head`. A base-only file stays out of review. Test multiple merge bases, no common ancestor, shallow or unavailable history, branch deletion and force-push, existing thread permalinks and stale anchors. An explicit missing ref keeps the form in an unavailable state. A tip change between preview and create requires reconfirmation and preserves the draft. Lost-response replay recovers the original PR and its comparison exactly once.

### 2. Make check policy explainable and publication recoverable

- **Trace.** Merge admission and PR detail read status and check-run records into `merge_requirements`. Today the latest report wins when both kinds share a name. A native HTTP test counts an approval from a read-only reviewer. GitHub requires both results to pass when their shared name is required and supports an expected reporting app. Required approvals come from eligible reviewers with write permission or code owners. Keep broad participation in reviews and evaluate merge eligibility from the current grant. Crab’s check policy exists on the tagged lineage. Specify the policy transition before changing installed requirements. `crates/crab-server/src/api/pulls/mod.rs:212`, `crates/crab-server/src/api/pulls/merge/mod.rs:345`, `crates/crab-server/src/test_support/http/mod.rs:221`, `crates/crab-server/src/api/pulls/tests/http.rs:383`, `crates/crab-server/REFERENCE.md:2429`. [GitHub required checks](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks), [GitHub protected branches](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)
- **Build.** Use the canonical policy from PR detail, merge admission and every Git write surface governed by the same rule. Expose which commit and reporter must pass. Characterize current behavior when the head changes between validation and publication or the base moves after a committed merge with a lost response. Verify which locks or preconditions and receipts actually cover each path. Close proven gaps at the shared Git publication owner. Keep durable PR reservation and ref publication linked with recoverable proof. `crates/crab-server/src/api/pulls/merge/mod.rs:142`, `crates/crab-server/src/git/receive/mod.rs:216`, `crates/crab-server/src/git/receive/publish.rs:151`, `crates/crab-server/src/git/receive/publish.rs:298`, `crates/crab-write/src/capsule_protocol.rs:390`.
- **Complete when.** An allowed reporter posts a passing result for the selected commit and the PR explains the merge decision. A wrong reporter, stale commit, missing result and colliding failed report all fail the declared policy. Check review eligibility after role removal. Exercise session and Git-token roles, a direct Git push to the protected branch, and two concurrent branch updates. After a lost reply or handoff, fetch sees the expected ref and replay returns the same merge outcome, including when the base has since moved. A failed attempt leaves no pending lock or falsely merged PR.

### 3. Complete the review workbench

- **Trace.** The detail page already separates Conversation, Commits, Checks and Files changed. Extend the review and diff owners, including the versioned thread and submission ledgers, and reuse the shared comparison from slice 1. `packages/ui/src/features/pull-requests/detail.tsx:160`, `packages/ui/src/features/pull-requests/reviews/form.tsx:58`, `packages/ui/src/features/pull-requests/reviews/threads.tsx:168`, `crates/crab-server/src/cells/repository/pulls/reviews/mod.rs`, `crates/crab-server/src/cells/repository/pulls/review_threads/mod.rs`.
- **Build.** Add a private pending review for one actor and comparison, with multiple file and line comments. Publish the summary, decision, comments and activity as one native command with replay. Keep immediate single comments available as an explicit action. Make Viewed persistent per actor and file version. Put file filtering and next/previous unresolved thread controls in the existing diff workspace. Current summary counts use loaded thread pages. Show the coverage of those counts or return canonical totals. Land batch suggestion application as a follow-on with one expected-head Git commit and line-eligibility checks. `crates/crab-server/src/api/pulls/review_threads/mod.rs:357`, `packages/ui/src/features/pull-requests/reviews/threads.tsx:64`. [GitHub review flow](https://docs.github.com/en/pull-requests/get-started/reviewing-pull-requests-quickstart), [GitHub suggestion batches](https://docs.github.com/en/pull-requests/how-tos/review-pull-requests/incorporating-feedback-in-your-pull-request)
- **Complete when.** A reviewer marks two files viewed, stages comments on multiple files, reloads, then submits one review. Another account sees the submitted comments together and sees none before submission. A new head invalidates affected progress, preserves drafts and requires explicit review of stale anchors. Retrying after a lost response produces one review and one set of comments. Keyboard navigation and focus work through file and thread links on desktop and narrow screens in both themes.

### 4. Open the onboarding and external CI paths

- **Repository path.** Reuse the catalog initialization path and the existing browser import. Reserve names for authorized owners, bind operations to stable UUIDs and keep import state durable across restarts and peers. Keep import credentials outside status, logs, events and rendered errors. Start with authenticated private repos. Decide the scope of LFS and metadata imports explicitly at the UI boundary. A new member should be able to create a repository, copy the clone URL, push and see the first commit. A denied name or access decision and an interrupted import should produce visible, recoverable outcomes. `crates/crab-server/src/catalog/mod.rs:309`, `crates/crab-server/src/git_import/mod.rs:297`, `packages/ui/src/features/imports/index.tsx:21`.
- **Integration path.** Reuse status and check storage. Add stable installation or service identities, the smallest repository and event grants for one external CI sender, and delivery records with deduplication and bounded retries. Align with slice 2’s provider identity. A real push emits one logical event, the CI job reports on the exact commit, the author sees its result and logs, and revocation prevents subsequent reporting or private delivery. `crates/crab-server/src/api/checks.rs`, `crates/crab-server/src/api/statuses.rs`, `crates/crab-server/src/server/http/boundary.rs:348`. [GitHub app permissions](https://docs.github.com/en/apps/using-github-apps/about-using-github-apps)

## Ordering and ownership

- Build slice 1 first. Run slice 2’s characterization alongside it. Files viewed, pending reviews, CODEOWNERS, merge methods and forks all benefit from the same comparison identity. Establish authoritative organization and team grants before team approvals or cross-repository write authority.
- Ship onboarding independently once its owner and placement policy are defined. Bind code indexes and cursors to repository UUID and snapshot, recheck current access before reading or returning results, and show when results are incomplete. Qualify email delivery with a verified sender domain. [GitHub code search](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax), [GitHub notifications](https://docs.github.com/en/subscriptions-and-notifications/concepts/about-notifications)
- Keep Git publication in the existing write and receive owners. Keep application data in the existing Cell domains. Use explicit, resumable preparation for existing repositories when durable schema changes. Cellar transactions and typed receipts operate at one Cell. For delivery or cross-repository side effects, use durable state and idempotent commands with authorization at each owner. Preserve ref locks, GC fences, pointer dependency proof and native output gating. `crates/crab-server/CELLAR.md:20`, `crates/crab-server/CELLAR.md:228`, `crates/crab-server/DESIGN.md:735`. [Cellar 0.1.2](https://github.com/crabbuild/cellar/releases/tag/v0.1.2)
- Introduce navigation alongside each authorized, durable workflow. Review the shell and new paths as a first-time contributor, a reviewer navigating a large diff, and an administrator diagnosing a blocked merge. Keep copyable links, browser Back, keyboard focus and actionable empty, indexing, stale, access-denied and uncertain-write states.
- Define enforcement coverage for deployments with direct object-storage writers. A mandatory hosted ruleset needs mediated writes and tested storage IAM. Verify browser writes, Git HTTP and any protected CLI paths claimed by the deployment. `crates/crab-server/REFERENCE.md:920`, `crates/crab-server/REFERENCE.md:2523`.

## Verification and release claims

- The concurrent workspace search fixture published five pinned Git snapshots to
  isolated RustFS and checked searches before and during index builds. Warm
  path, literal, regex, and Boolean content-and-symbol queries covered 42,479
  paths across 81 successful HTTP pages. Per-mode p95 page latency was 87–168 ms
  on the local test server. The largest cold index took 152 seconds to build.
  Embedded Chromium reopened a pinned source line, and revoking access
  invalidated the prior continuation.
  Reusing the packed bytes from the size check reduced backend reads during
  the same build workload by 10.5%. The largest build took 153 seconds in
  the follow-up run. Warm p95 page latency was 60–110 ms across all four modes.
  Parsing pages in parallel under the shared CPU limit reduced the largest
  build from 154 to 127 seconds in the next paired run. All 81 warm pages
  returned the expected results, and eight concurrent pages during indexing
  completed at 56 ms p95.

- Embedded Chromium previewed and downloaded a native Crab file through the
  same server API. Native HTTP tests exercised authorized full reads, ranges,
  and `If-Range` against real shard and xorb fixtures. They checked that
  a shard hint could not authorize an unlisted artifact, a range could not
  hide a pointer size mismatch, and transfer of corrupt stored bytes failed.

- Embedded Chromium queried a Git LFS Parquet file and downloaded its original
  bytes after the branch advanced. Playwright verified that oversized or
  unknown-size artifacts make no preview request. Native HTTP tests verified
  pinned ranges, `If-Range`, and denial after read access was revoked. The
  standalone RustFS qualifier checked a file-view range against a real 8 MiB
  object uploaded with stock Git LFS.

- Organization automation passed the standalone server and production UI
  builds, generated contract check, 36 focused native tests, 120 UI unit
  tests, and four organization Playwright cases. The browser issued a
  credential that stock Git accepted and then disabled it. Native coverage
  exercised Git push and clone, Git LFS upload and download, protected
  branches, repository rename and transfer, grant downgrades, individual
  revocation, and disable across two native replicas. The RustFS process
  gate pauses an admitted receive on one replica, revokes the credential
  on the other and verifies the branch is unchanged. It checks that Git
  fetch and push reject the revoked token, including after restart, then
  publishes the same commit with a replacement token.

- Organization audit history passed the server and production UI builds,
  generated contract check, 31 focused native tests, 120 UI unit tests,
  and four organization Playwright cases. Embedded Chromium filtered an
  organization log and downloaded the result. Native tests denied read
  and export access to a non-owner, split a 501-event search across two
  export parts and restored transfer history while retaining each
  organization's historical name and access boundary. The standalone
  RustFS qualifier compared the audit log after SIGKILL and a full-root
  restore into an empty cache.

- Owner-facing member access inspection passed the production UI build,
  generated contract check, 20 native tests, 120 UI unit tests and four
  organization Playwright cases. Native HTTP and embedded Chromium showed
  the role and its sources, then refreshed after the team grant disappeared.
  The native tests verified inherited fork grants, an invalidated continuation,
  owner-only access, suspension and denied Git write token issuance after
  team removal.

- The standalone macOS server with persistent indexing passed the isolated
  RustFS SIGKILL and full-root recovery gate. The fixture verified hashes
  and metadata for 364 restored objects and checked Git refs and application
  state after restoring into an empty cache. Deployment-provider and live IdP
  qualification remain open.

- The persistent default-tree search index passed standalone server and
  production UI builds, six focused native tests, 120 UI unit tests, and
  two Playwright cases. In a synthetic 514-path tree, a sparse search
  verified two Git candidates. The native test also covered Unicode
  matching and index refresh after a push. Embedded Chromium opened a
  match at the exact commit and line. A standalone RustFS process built
  the index from a stock Git push and reused it immediately after restart.

- An offline check used one selective case-insensitive expression per pinned
  snapshot. Across 13,199 eligible text paths, the filter admitted 3,376
  candidates and retained all five exact matches. The snapshots were
  TanStack Query `ee47993724e5`, MinIO `07c3a429bfed`, Eclipse Platform UI
  `e3bbb556534a`, and Zod `d2b135cfb7a3`. This measures candidate reduction.
  Large-catalog HTTP latency and concurrent load still need qualification.

- Workspace code and path search passed the standalone server and production
  UI builds, four native search and catalog tests, 120 UI unit tests, and
  four Playwright cases. Embedded Chromium searched two native Git
  repositories and opened the exact commit and line. Native tests verified
  organization scope, empty continuation pages, pinning after a branch
  update, and continuation rejection after revocation. The standalone
  RustFS process gate checked both search modes against real Git objects.

- Repository deploy tokens passed a production UI build, 120 UI unit
  tests, two focused Playwright cases, and 20 focused native tests.
  Embedded Chromium created a token, completed a fresh stock Git clone,
  then revoked the token and confirmed the next Git request was denied.
  Native tests checked Git and LFS read scope, session independence,
  repository isolation, transfer, archive, stale replay, and revocation
  across two replicas.

- The standalone macOS debug server passed the RustFS process gate at
  its default worker stack after boxing the commit callback under the
  publication leases. The gate verified stock Git push, PR merge and
  replay, clean restart, SIGKILL, and full-root restoration into an empty
  cache. It compared Git refs, application state, and object hashes and
  metadata, and verified an 8 MiB Git LFS object and range resumption.
  Separate RustFS tests passed for mTLS peer handoff and access revocation
  after a stale catalog refresh. Cloud and IdP qualification remain open.

- Browser Git webhooks passed the production UI build, 120 UI unit tests,
  six Playwright cases and 37 focused native tests. Native delivery covered
  repeated branch creation, tag publication, private candidate refs,
  exact-commit merge checks and a lost merge reply. An embedded browser edit
  reached the signed receiver with both commit OIDs and showed a delivered
  event in settings. The native peer test also verified team grant revocation
  for existing browser, Git, and LFS access through two replicas.

- Outbound webhooks passed the production UI build, 120 UI unit tests, 41
  Playwright cases, and 29 focused native tests. Stock Git sent exact branch
  updates to a local receiver. The native fixture proved that uncommitted
  plans were unsent, identical PR submissions generated one event, and manual
  redelivery reused the signed bytes. Chromium enabled a webhook and saw the
  delivery after the receiver returned HTTP 503 and the server retried.

- GitHub Actions delivery history passed the production UI build, 120 UI
  unit tests, and 40 Playwright cases. Native HTTP verified admin-only access,
  exact replay after history eviction, secret rotation, and late attempts
  that leave the selected check unchanged. Embedded Chromium refreshed
  Integrations after a signed report and showed the exact commit. The native
  lost-merge-reply and ordered-queue browser regressions also passed.

- The RustFS process gate verifies protected merges across two standalone
  servers with a signed App fixture. A same-name manual check, an unbound
  installation, and a previously passing result for an older commit cannot
  authorize publication. A downgraded reviewer no longer counts. After a
  fresh approval and workflow result, the original merge request recovers
  through the surviving replica with the same commit and timestamp.

- Repository transfer passed the production UI build, 76 focused native
  tests, 120 UI unit tests, nine Playwright cases, and three embedded
  Chromium workflows. Native HTTP checked personal consent, destination
  organization owners, revoked source authority, exact replay, and issue
  continuity. Stock Git read and pushed through the old address with a new
  token. Embedded Chromium accepted an offer with a different sign-in.

- Repository rename passed the production UI build, 71 native tests, 120 UI
  unit tests, eight focused Playwright cases, and two embedded Chromium
  workflows. Native HTTP preserved a real issue across rename, recovered
  an identical submission, and resolved the new route on a stale server.
  Stock Git cloned and pushed through the former address. Reusing that
  address left the original Git token bound to its original repository UUID.

- Catalog discovery passed the production UI build, 30 focused native tests,
  120 UI unit tests, 57 Playwright cases, and three embedded Chromium
  workflows. Native HTTP verified archive state, direct and team grants,
  account and query bound continuations, and revocation. Chromium signed in,
  searched the native catalog, and opened an authorized repository.
  Five readiness fixture failures also reproduced on the previous commit
  because the fixtures reject the existing unread notification request.

- Native HTTP verified OIDC and GitHub email binding, opt-in, stale writes,
  address changes, mute, and revoked access. Embedded Chromium enabled
  delivery for a real mention and a local provider accepted the email.
  Recovery after a lost acknowledgement reused the exact provider key and
  payload. The production UI build, 80 focused native tests, 120 UI unit
  tests, 88 Playwright cases, and seven embedded Chromium workflows passed.
  Live Resend delivery requires an operator-provisioned sending domain and key.

- Native HTTP verified subscription-time ordering across repositories,
  equal timestamp boundaries, unknown historical origins, and manual,
  participation, mention, and assignment sources. Embedded Chromium
  exercised reason and time controls against native storage. The production
  UI build, 72 focused native tests, 120 UI unit tests, 87 Playwright cases,
  and six embedded Chromium workflows passed.

- Native HTTP and Directory storage verified the 90-day completion boundary,
  bounded sweep continuation, saved bookmark retention, and replay suppression
  across both delivery channels. Newer activity reopened the conversation.
  The production UI build, 71 focused native tests, 120 UI unit tests,
  86 Playwright cases, and six embedded Chromium regressions passed.

- Native HTTP verified bulk discussion unsubscribe across repositories,
  mixed outcomes, saved activity retention, access revocation, and replay
  against newer follows. Embedded Chromium unsubscribed from quiet discussions
  and verified persistence after reload. The production UI build, 70 focused
  native tests, 120 UI unit tests, 86 Playwright cases, and five embedded
  notification regressions passed.

- Native HTTP listed quiet issue and pull request follows, continued from an
  empty scan page, checked current mute state, and hid revoked repositories.
  Embedded Chromium used real OIDC and native storage to change a follow and
  stop following it across reloads. The production UI build, 69 focused native
  tests, 120 UI unit tests, 85 Playwright cases, and four embedded notification
  regressions passed.

- Native HTTP verified bulk unwatch with mixed outcomes, revocation, wrong-owner
  rejection, CSRF protection, and retries that preserved newer settings.
  Embedded Chromium confirmed a two-repository unwatch persisted after reload.
  Playwright exercised a 52-watch preview spanning an empty page and retried
  the original versions after a lost reply. The production UI build, 68
  focused native tests, 120 UI unit tests, 84 Playwright cases, and three
  embedded notification regressions passed.

- Native HTTP verified release-only watches, account isolation, access
  revocation, and pagination past empty results. Embedded Chromium opened
  Watched repositories from the inbox, saved a watch, reloaded, and stopped
  watching using the real OIDC and Repository Cell workflow. The production
  UI build, 67 focused native tests, 120 UI unit tests, 83 Playwright cases,
  and two embedded notification regressions passed.

- Native HTTP verified release watch activation, draft publication,
  publisher exclusion, delayed delivery, and replay after reading. It also
  verified republication, deleted-release visibility, and links bound to
  the release number. Embedded Chromium enabled release watching and
  opened a published release from the real inbox. The production UI
  build, 63 focused native tests, 120 UI unit tests, and 82 Playwright
  cases passed.

- Native HTTP verified mixed unsubscribe outcomes, bookmark retention,
  rejected access, and late watch or assignment delivery staying hidden.
  A new mention or review request reopened the discussion, and replay
  preserved the later follow. Embedded Chromium unsubscribed from a real
  issue and showed it after a new mention. The production UI build,
  61 focused native tests, 120 UI unit tests, and 80 Playwright cases passed.

- Native HTTP verified bulk inbox triage across repositories. A stale row
  rolled back the batch, revocation blocked admission, and a committed
  receipt replay left newer activity unread. Embedded Chromium completed
  two real inbox rows. The production UI build, 49 focused native tests,
  120 UI unit tests, and 79 Playwright cases passed. A focused mobile run
  covered selection across pages, stale refresh, and lost-reply recovery.

- Native HTTP verified Saved and Done through recent-history eviction,
  archive pagination, a stale completion action, and repository access
  revocation. Chromium used the embedded UI and real native server to save,
  complete, reload, and restore a notification delivered from a mention.
  The production UI build, 48 focused native tests, 120 UI unit tests,
  and 78 Playwright cases passed.

- Native HTTP verified stable issue and PR inbox rows, distinct same-number
  discussions, replay and access-channel deduplication, and rejection of a
  stale read change. Chromium verified refresh and retry after a newer
  contribution arrived, plus navigation to the linked reply. The production
  UI build, 46 focused native tests, 120 unit tests, and 53 browser cases passed.

- Native HTTP verified automatic following from mentions and assignments. A
  reply survived a full outbox page ahead of its mention source and a partial
  delivery preserved the user's read state on replay. A newer unsubscribe
  survived delivery, and reassignment enabled future activity. Group-only
  enrollment required current signed access. All 44 focused native tests
  passed. The production UI build, 120 unit tests and 58 browser cases passed.

- Native HTTP exercised automatic following before the delivery worker ran,
  immediate replies, unsubscribing and re-following, stale writes, and replay.
  A pending review left the private preference and inbox untouched through a
  delivery sweep. Publication followed the discussion and delivered the
  committed watch and mention notices. All 37 focused native tests passed.
  The production UI build, 120 unit tests, and 53 browser cases passed.

- Native HTTP verified per-discussion follow, state changes, unsubscribe and
  inheritance. It covered independent issue and PR addresses, overlapping
  repository and discussion watches, private versioned saves, and direct
  mentions after unsubscribing. All 26 focused native tests passed. The
  production UI build, 120 unit tests and 61 browser cases passed, including
  issue preference persistence and PR preference conflict recovery.

- Native HTTP and stock Git verified issue and PR watch preferences, exclusion of
  earlier and self-authored activity, versioned retry, mute, revocation, and
  fanout across multiple recipient pages. Twenty-four focused native tests
  passed. Playwright verified lost-response recovery, persistence and the unread
  badge in an accessible watch dialog. The production UI build and 120 unit
  tests passed.

- Native HTTP and stock Git verified assignment notices on issue and PR
  creation and on bulk edits. Replayed requests, retained assignees and
  self-assignment produced no duplicate or self notices. Revocation hid
  existing assignments. The focused server suite passed 18 tests and
  Playwright passed 22 cases, including the inbox reason filter.
- After integrating the current `crab-v2` catalog, editor and notification
  changes, 30 focused native tests passed. Stock Git and embedded Chromium
  exercised owner-managed grants and revocation, CODEOWNERS, fork permissions,
  pending reviews, issue template inheritance and tested merge publication.
  The production UI build, 120 unit tests and 31 focused Playwright tests passed.
- Stock Git and embedded Chromium verified issue template inheritance after
  granting access to the source repository, creation with target-repository
  defaults, source access revocation, and a local config override. The
  focused native suite passed 15 tests, including browser contract checks.
- Stock Git published Markdown and YAML templates on the default branch and a
  draft template on another branch. Embedded Chromium showed the default-branch
  chooser and a reader submitted the YAML form. The native issue stored the
  rendered answers and configured label, and a changed label selection with
  the same proof was rejected. Playwright verified editable Markdown prefills,
  lost-reply recovery, and required YAML fields on a narrow dark-mode view.
- Embedded Chromium showed a description-derived link. Native Git publication
  closed the issue referenced in the description and kept the issue mentioned
  in inline code open. Native tests verified deduplication, exclusive pages,
  retargeting, replay and retention of completed links after a description edit.
  The discussion-index rebuild and repository and organization Projects suites
  exercised the shared projection.
- Embedded Chromium linked an issue and enabled auto-merge. The issue stayed
  open while CI was pending, then showed **Completed** and linked back to the
  merged pull request. Stock Git confirmed the published parents. Publishing
  a tested merge to a non-default branch kept its linked issue open through a
  lost response and retry. Native Cell replay closed the captured issues once
  after the live links changed.
- Native HTTP and stock Git verified independent queue settings on two
  branches. The worker removed the entry with the shorter deadline, preserved
  and published the sibling's tested squash SHA, and rejected an early timeout
  under the longer deadline. Adding and removing a method override expired the
  old enrollment. Playwright verified saving and reloading an exact branch
  override with an inherited build limit.
- Stock Git verified grouped CI with three entries and a limit of two. The
  second candidate included the first entry while the third waited. Canceling
  the first entry invalidated the old candidate and its check target. After
  rebuilding, the queue published the successor SHAs in order and reused an
  already-passing build when its predecessor became the base. Browser tests
  exercised the build limit editor and grouped candidate status.
- Stock Git verified the exact published SHA for both squash and rebase
  queue candidates. The test checked linear history, retained rebase commit
  messages, and proved that a successful check on an old candidate could
  not release it after the configured method changed and changed back.
  Browser tests exercised the queue method picker and enrollment retry.
- Native Git and server tests verified required queues for repository rulesets
  and inherited organization rulesets. The organization lease fenced an
  in-flight push, matching branches published the exact fetched candidate,
  and an unaffected branch remained writable. Browser tests verified both
  ruleset editors and the inherited policy display.
- Automatic queue dispatch was exercised with the Crab HTTP server, an isolated
  GitHub API fixture, and stock Git. A signed delivery recovered the run after
  the dispatch response was lost. The checkout credential exposed only the
  active candidate, and the target branch advanced to the exact fetched SHA.
  One operator App webhook now routes signed reports by installation,
  repository and workflow. Switching a connection to the App clears the
  previous authority for required head checks. Browser tests cover dispatch
  setup and the run link. A live GitHub App
  installation remains to be qualified.
- This assessment traced the source and existing tests and exercised stock Git on divergent history. The synthetic direct tip diff reported a base-only deletion and the topic addition. The three-dot diff reported only the topic addition. On Crab merge `9f58cf5d85`, the same comparison on its parents returned 64 paths tip-to-tip and 46 from the merge base to the second parent. A separate redundant-parent fixture showed stock Git choosing the better common ancestor while a transcription of Crab’s helper chose its parent. Crab’s frontend and server suites were not run during this audit. Visual and accessibility findings are source-based, pending hands-on review.
- Start with `npm run typecheck --prefix packages/ui`, `npm run test --prefix packages/ui`, the affected Playwright specs, and `npm run build --prefix packages/ui`. Build the UI before compiling `crab-server`. With a writable mounted workspace and this worktree’s own external `CARGO_TARGET_DIR`, run `cargo test -p crab-server --locked --lib api::pulls::tests` and the affected settings, Git receive and Cell suites. Run `npm run contracts:check --prefix packages/ui` for exported wire changes. The existing browser suite uses mocked transport per scenario. `packages/ui/README.md:51`, `crates/crab-server/AGENTS.md:215`, `.github/workflows/rust.yml:235`.
- Extend the real-process/native CI fixture with the browser milestone. Check the resulting Git refs and object contents with stock Git, durable discussion state with a new session, and permission revocation with a previously authorized session. Exercise unavailable/indexing and storage pressure states against a representative large repository. Record p95 and failure rate for tree navigation, search, PR diff and merge on a defined corpus and concurrency before setting commitments. Use the existing isolated RustFS process and Kubernetes suites as the starting evidence. `crates/crab-server/tests/qualify_cellar_process.py:467`, `.github/workflows/architecture.yml:288`, `crates/crab-server/REFERENCE.md:2469`.
- Track exact source, embedded UI, image, chart, Cellar version and storage provider for acceptance evidence. Release qualification includes provider identity and faults, complete-root restore, rollout and rollback, and resource limits on the actual deployment. The first tagged native server image/chart remains gated in the inspected workflow. `.github/workflows/http-server-release.yml:17`, `crates/crab-server/REFERENCE.md:2528`.
