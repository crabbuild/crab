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

This verifies the milestone slice locally, not the entire expansion. Activity,
linked issues, draft readiness, and review requests still require their models,
HTTP/browser workflows, and evidence. Full cross-platform/runtime proof remains
a CI responsibility.

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
