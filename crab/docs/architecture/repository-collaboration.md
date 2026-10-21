# Repository collaboration expansion

Status: all six capabilities implemented with native, HTTP, browser, and RustFS
recovery proof. Broad runtime and cross-platform gates run in CI.

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
binary record codecs remain unchanged. New entity state is canonical native
storage, not a process cache or a fallback reader. Existing repositories gain entities
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

This verifies the milestone workflow locally. Cross-platform/runtime proof
remains a CI responsibility; the other capability evidence follows below.

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

### Draft readiness contract

Pull creation accepts `draft` (default false). The PR and its readiness document
commit together; ordinary requests retain their original submission identity.
Creation replay validates the original payload in the native command, so editing
an existing PR or changing readiness does not invalidate recovery of its creation.
The original request cannot be reused for another creation mode.

Readiness lives under `collaboration/v1/pulls/` without changing the existing SQL
schema or binary record codecs. Absence means normal ready state; reads do not
create documents or historical events. Native PR results load readiness and the
PR version in the same transaction. JSON command replies include the draft flag;
previously stored replies without it retain their original ready semantics.
Only the author or a repository writer can convert an open PR. The existing PR
version orders readiness, PR edits, review decisions, and merge admission.
An unchanged desired readiness does not advance it.

Drafts remain open for discussion and reviews but reject fresh merge reservation
and admission. A readiness change after reservation invalidates the old PR
version; a pending merge blocks conversion. Closing/reopening does not change
readiness. Lists and detail views expose the draft flag, and conversion records
one actor/action event.

Every native PR mutation checks archive inside its transaction. Existing command
receipts replay their original decision. Merge completion and abort may recover
admitted work after archive: the HTTP boundary defers to these guarded commands,
and Git publication retains its own lifecycle check. An already-published ref can
complete metadata; an unpublished merge denied by archive clears its pending
admission without changing Git. Other HTTP mutation families retain their existing
preflight boundary.

### Draft readiness evidence

Native tests cover creation mode conflicts and replay, role/stale-version checks,
unchanged-state retries, draft reservation/admission rejection, conversion between
reservation and admission, conversion racing a merge, pending-merge protection,
archive rejection, atomic readiness/version read snapshots, pre-readiness JSON
receipt decoding, and readiness/receipt recovery after native restart. Lifecycle
coverage exercises every fresh PR mutation, concurrent archive/edit receipt order,
and cleanup of admitted work.

HTTP tests cover CSRF, permissions, readiness changes and replay, and recovery of
creation after body/readiness edits. Real Git HTTP pushes seed archive-recovery
regressions: published merges complete metadata after archive, while unpublished
merges abort without moving the base ref. The original PR review/protection and
merge-method suites retain their assertions and require the new explicit draft
flag in list responses.

Browser tests exercise draft creation/reload, truthful merge controls, failed
writes, explicit conflict refresh, conversion, draft list badges, permissions,
archive views, recovery of already-admitted merges, and light/dark mobile
accessibility. Archived conversations retain merge outcomes and expose a retry
for pending publications; abort errors offer an explicit metadata refresh.
Dedicated RustFS process qualification restores a draft after restart, proves
a draft merge leaves the base ref unchanged, marks it ready, publishes a real
merge, and restores the merged state and activity through another restart.

### Review request contract

A PR retains one request state per qualified reviewer (identity issuer and stable
subject). Names are snapshots, not keys. States are requested, cancelled, and
fulfilled with the submitted review number. Re-requesting a cancelled or fulfilled
reviewer starts another request; cancellation retains the record. Pages use
bounded, exclusive opaque identity cursors.

Authors or repository writers request and cancel reviews on open PRs. The author
cannot be selected as a reviewer. The HTTP boundary resolves durable catalog
membership for participant selection and current caller access, so background
catalog materialization cannot re-offer a removed member. Renamed members keep
their identity; removed members remain visible on existing requests and can still
be cancelled. Public request items expose opaque IDs and display names, not
identity claims. Local operator mode has no other identity-provider members.

The existing PR version orders requests, cancellation, review decisions, and
merge admission. Repeated desired state does not advance it. Pending merges
freeze request changes. Requests are invitations; they do not add a merge gate
or replace branch protection's approval requirements.

A newly submitted review fulfills its author's matching outstanding request in
the review's native transaction. Comment-only reviews count as submitted reviews.
Request fulfillment advances the PR version once, including for comment-only
reviews; approval decisions and fulfillment do not advance it twice. Review
submission replay returns the original review without fulfilling a later
re-request. The ordinary review activity event describes fulfillment; request
and cancellation have their own actor/action events.

### Review request evidence

Native tests cover qualified identity, renamed displays, concurrent duplicate
requests, desired-state retries, stale cancellation after re-request, permissions,
archive, exclusive pagination, merge admission, fulfillment, and native restart.
HTTP tests exercise actual Git pushes and review submission, CSRF, roles,
malformed cursors, membership rename/revocation, and cancellation after removal.
Browser tests cover failed writes with retained choices, reload, cancellation,
fulfilled status, explicit conflict refresh, read-only/archive views, pagination,
whole-widget timing, and mobile light/dark accessibility.

A dedicated RustFS instance with two synthetic OIDC identities exercised real
native Git pushes and browser PR creation. Alice requested, cancelled, reloaded,
and re-requested Bob's review. Bob signed in separately and submitted an approval;
the sidebar and activity reflected the native review. Another request survived a
server restart at PR version 6. Bob's comment-only follow-up fulfilled it as
review 2 and advanced the PR version once to 7. Another restart preserved the
request, both reviews, and activity. Finally, starting with an empty local Cellar
data directory recovered that same state from RustFS. The prior directory was
retained separately; no local-state fallback was used.
