# Repository collaboration expansion

Status: implementation in progress. Extend Crab's existing repository application
with durable GitHub-style collaboration and matching browser workflows.

## Required capabilities

| Capability | Durable model | Browser workflow |
| --- | --- | --- |
| Milestones | Versioned milestone, due date, issue/PR assignment, progress counts | Create/edit/close/reopen, assign/unassign, browse progress |
| Reactions | Typed target, reaction kind, unique actor membership, aggregate counts | Add/remove reactions on issues, PRs, comments and review conversations |
| Activity | Ordered actor/action events, subject indexes | Paginated issue/PR timelines and repository activity |
| Linked issues | Bidirectional issue/PR relationships | Link/unlink and navigate between related work |
| Draft pull requests | Explicit readiness state | Create/convert drafts, mark ready, prevent draft merges |
| Review requests | Qualified member identity and request state | Request/cancel reviews, show outstanding requests, fulfill submitted reviews |

Relationships describe related work; linking an issue does not promise automatic
closure when its PR merges. Existing review, merge, authorization, and Git
publication contracts remain authoritative.

## Persistence boundary

Use typed, bounded JSON documents under `collaboration/v1/` in the repository
Cell's native KV storage. Cellar's KV and application SQL share the same SQLite
transaction and durability/output gate. Existing repository SQL schema and
stored records remain unchanged. New entity state is canonical native storage,
not a process cache or a fallback reader. Existing repositories gain entities
through explicit commands; reading optional metadata does not create data.

Use keyset indexes and bounded pages, including for state-filtered milestone
lists and subject timelines. Versioned edits reject stale submissions. Membership
checks precede effects; archived repositories reject fresh mutations. Command
receipts preserve decisions on retry, and activity/progress changes commit with
their triggering mutation. Reactions express desired membership rather than
non-idempotent toggle commands.

Historical activity is not fabricated. Events begin with recorded commands after
the feature is deployed; existing comments and reviews remain available.

## Verification

Require native Cell tests for replay, stale edits, transaction rollback, target
validation, bounded pagination, progress transitions, and process restart.
HTTP tests cover permissions, CSRF, archive policy, and malformed input. Browser
proof must exercise real writes followed by reload, including error/conflict and
empty states. Existing suites and public command/record contracts must continue
to pass. No milestone in this table is complete until its storage, API, UI, and
verification are all present.

### Milestone evidence

Local verification covers native replay/restart, stale edits, date validation,
assignment keyset pagination, reassignment cleanup, stable author identity,
archive rejection, and issue/PR close/reopen/merge progress. HTTP coverage checks
permissions, CSRF (including PUT), malformed pagination, creation replay, and
assignment retries. Repository route tests cover list/new/detail addresses.

A dedicated server and synthetic RustFS bucket exercised browser creation,
keyboard date entry, issue assignment, issue closure and 100% progress, stale-edit
recovery with a retained draft, and milestone close/reopen. A graceful process
restart restored the milestone, assignment, assigned-work list, and progress;
native Cellar objects were present in the bucket. Desktop and mobile dark views
had no accessibility violations; the existing footer contrast check required
manual review. The 390px viewport had no horizontal document overflow.

This verifies the milestone slice locally, not the entire expansion. Draft
readiness and review requests still require their models, HTTP/browser workflows,
and evidence. Linked work qualification is described below. Native PR mutations
need the same transactional archive enforcement as issue mutations. Full cross-platform/runtime
proof remains a CI responsibility.

### Reaction evidence

Native tests cover unique actor membership, desired-state retries, stable identity
when names change, concurrent add/remove, target/parent validation, archive
rejection, bounded batch reads, and recovery of membership/counts/receipts after
native restart. HTTP tests cover read-member participation, personalized views,
CSRF, malformed inputs, target isolation, list batching, and preservation through
creation replay and discussion edits.

Browser tests exercise all seven target types, stale refresh protection, reload,
failed writes and retry, archived counts, and light/dark accessibility. Dedicated
RustFS-backed browser verification creates the discussion objects and adds
reactions through the UI, including an inline thread anchored by selecting a diff
line. Removing a reaction and reloading retains the removal. Process restart
proof compares saved memberships/counts with restored HTTP views and checks the
visible reaction chips after reload.

### Activity contract

Repository activity and issue/PR timelines record collaboration changes from new
commands. Events cover discussion creation and edits, state transitions, reviews,
inline conversations, replies, merge completion, and milestone relationships.
Existing comments and reviews remain available through their original views;
activity does not synthesize historical events. Administrative settings retain
their existing catalog audit.

Each event captures the actor name and subject title at the time of the action.
A native sequence orders events independently of wall-clock ties, and a subject
index supports exclusive keyset pagination. Event records, indexes, sequence,
and the originating SQL/KV changes share the native command transaction. Creation
reservations and command replay do not duplicate events. Rejected and unchanged
edits do not create activity. Merge reservation and preparation do not claim that
a merge completed; completion records the merge event.

The HTTP feed exposes display names and typed resource references. Identity
issuer/subject claims and discussion bodies stay out of activity responses.
Browser panels retain loaded pages on failure, support retry and refresh, and
link milestone changes to their milestone. The timing flag wraps the whole
activity widget, including its controls.

### Activity evidence

Native and HTTP tests cover command/submission replay, concurrent ordering,
rejected and unchanged edits, all seven discussion targets, review/thread/merge
transitions, milestone reassignment, authorization, bounded pagination, and
restart recovery. Browser tests cover retained pages through failures, retry,
refresh, milestone navigation, automatic updates after issue/comment writes,
whole-widget timing, and inline-link accessibility in dark mode.

A dedicated RustFS-backed browser session began with an empty feed on an
existing repository. Reopening/closing an issue, posting a comment, removing and
reassigning its milestone, resolving/reopening an inline thread, and submitting
a review produced eight events. A graceful server restart preserved all eight,
the five-event issue index, and the three-event PR index. Exclusive two-item
pagination matched the complete feed before and after restart. The same restart
also retained milestone progress and all seven reaction memberships. A separate
native-process qualifier verifies activity persistence and creation replay
alongside milestone/reaction state.

### Linked work contract

An issue and a PR in the same repository have one versioned relationship. Active
relationships appear in both subject indexes; removing one retains its version
so a stale retry cannot overwrite a later link or unlink. Desired-state retries
do not increment the version or create another event. Both subject indexes and
one canonical activity event commit with the relationship in the same native
transaction. The event appears in both timelines and once in repository activity.

Lists use bounded, exclusive keyset pages ordered by the related subject number.
They resolve current titles and open/closed/merged states from the native SQL
records. No historical relationship or activity is inferred from Markdown.
Writers manage links, readers follow backlinks, and archived repositories reject
fresh mutations. The browser retains failed input, requires explicit refresh
after a conflict, and preserves backlinks in read-only and archived views.
Linking does not automatically close an issue or alter merge requirements.

### Linked work evidence

Native tests cover both indexes, receipt replay, concurrent duplicate links,
versioned tombstones, stale add/unlink rejection, current subject summaries,
exclusive pagination, invalid/missing targets, archive rejection, and native
restart. HTTP tests cover membership, writer permissions, CSRF, input bounds,
identity privacy, archive policy, and unchanged desired-state retries.

Browser tests exercise failed writes with retained issue numbers, explicit
conflict refresh, linking from a PR, navigating to its issue backlink, unlinking
from the issue side, reload, read-only/archived views, and light/dark accessibility.
The synthetic RustFS-backed browser repeats link → backlink → unlink → reload →
relink. The process qualifier creates a real Git branch and PR, links/unlinks/
relinks its issue, and restores both indexes and version 3 after restart. An old
unlink is rejected and leaves the restored relationship and activity unchanged.
