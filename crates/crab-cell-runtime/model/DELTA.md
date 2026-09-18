# Rust-to-model delta ledger

The model checks the coordination protocol, not SQLite bytes, object-store
providers, cryptographic signatures, or HTTP. Each collapse is intentional and
must be reviewed when `src/coordination.rs` changes.

| Rust surface | Model surface | Abstraction | Safety consequence |
| --- | --- | --- | --- |
| `CoordinationState::lifecycle` | `state` and `owner` | Serving/fenced/draining is represented by serving/fenced/idle plus owner | The model still rejects dual serving and fenced admission |
| `CoordinationState::shutdown_requested` | `state`/`accepted` | A shutdown request is folded into the model's idle transition while queued work remains represented by the accepted set | Accepted work cannot be stranded behind terminal admission closure |
| `CoordinationState::publications` | `retained`, `published` | Byte counts and roots collapse to one retained obligation and a monotonic count | Early acknowledgement/release remains observable |
| `CoordinationState::residency` | omitted | Sparse/hydrating/resident is a local acceleration state with no authority effect | Hydration cannot make a fenced owner serve or change durability |
| `CoordinationState::busy` | `accepted`/`retained` action guards | One in-flight SQL/effect slot is represented by the bounded command and publication actions | Release cannot overtake accepted work or a retained cut |
| `CoordinationState::renewing` | `Renew` | Renewal in-flight identity is collapsed to one owner-preserving action | A late renewal cannot create a second owner |
| `CoordinationState::publisher_ready` | `state`/`retained` guards | Publisher availability is represented by the serving and publication preconditions | Release remains blocked while a publication obligation exists |
| `CoordinationState::pending_effects` | action interleavings | Stable effect IDs and stale completions are abstracted as independently schedulable actions | Duplicate/reordered completion cannot advance a publication twice |
| `CoordinationState::next_effect_id` | omitted | Numeric identity has no protocol meaning beyond exact membership | Stale-ID protection is checked in Rust and the simulator |
| `CoordinationInput::Admit` | `Admit` | Request IDs/digests collapse to finite command names | Admission-after-fence remains observable |
| `CoordinationInput::CallerCancel` | stuttering action | Caller cancellation does not mutate accepted durable work | Accepted work still reaches publication/terminal resolution |
| `CoordinationInput::FollowerProof` | `Publish`/`Ack` | An accepted follower proof is a bounded alternate durability witness | A follower proof can satisfy publication durability without weakening fencing |
| `CoordinationInput::Lookup` | `state`/`owner` predicates | Read-only local eligibility is represented by the serving-owner guard; the model does not return a handle | A fenced, idle, or quiescing owner cannot be modeled as a safe local hit |
| `BeginWork`/`FinishWork` | `Admit` plus the surrounding action boundary | SQL/query execution and caller channels collapse to a bounded accepted command; work completion has no durable state of its own | Publication and acknowledgement still cannot overtake the accepted command |
| `BeginEffect`/`CompleteEffect` | Individual TLA action interleavings | Monotonic local effect IDs and duplicate completion bookkeeping are abstracted because each action is independently schedulable | A completion cannot create a new owner, root, or acknowledgement; adapter stale-ID handling is covered by the simulator |
| `BeginDrain`/`BeginShutdown`/`FinishMigration` | `Fence`, `Release`, and `Takeover` | Runtime drain phases collapse to the authority-visible idle/fenced boundary | Release cannot overtake a retained publication, and takeover still requires a fenced/idle owner |
| `BeginDrain` | `Drain` in the stable-provider profile | The liveness configuration exposes the explicit quiesce event while omitting new failures | Fair publication, acknowledgement, and release steps must eventually complete after a drain request |
| `BeginHydration`/`FinishHydration` | No durable model action | Residency is local cache state and does not change ownership, roots, or acknowledgement | Hydration cannot weaken the modeled authority and durability predicates |
| Migration admission while a durable cut is pending | `Admit` may leave work in the accepted set while `Prepare`/`Publish` remains in flight | The Rust actor queues successor work behind the migration publication; the model does not execute that queued work until the serving owner returns | Migration cannot reject already accepted successor work, and publication still precedes acknowledgement |
| `BeginPublication`/`FinishPublication` | `Prepare`/`Publish` | Upload and CAS effects collapse to one atomic publication step | Acknowledgement still requires publication |
| `Fence` and takeover | `Fence`/`Takeover` | Lease/session signatures collapse to owner identity | Different-winner and dual-owner faults remain observable |
| `CoordinationInput::FinishRenewal` | `Renew`/`Fence` | Provider success/fence outcome collapses to owner-preserving renewal or fenced state | Late renewal cannot revive a fenced owner |
| `CoordinationInput::BeginPublication`/`FinishPublication` | `Prepare`/`Publish` | Publication result and ambiguous CAS collapse to retained/published transitions | Exact winner can complete; a different winner is a fault |
| `CoordinationInput::FinishHydration` | omitted | Hydration completion is local and has no modeled authority transition | Incomplete/stale hydration cannot be a durability proof |
| `CoordinationInput::FinishMigration` | `Takeover`/`Release` boundary | Migration code/schema details collapse to the authority handoff boundary | Release remains ordered after accepted work |
| `CoordinationDecision::ResolveUnknown` | `Ack` omitted from accepted set | Resolve outcomes do not mutate the durable command state | Unknown resolution cannot acknowledge a command |
| Renewal inputs | `Renew` | Provider response details are omitted | Renewal cannot create a second serving owner |
| Async task IDs/completions | TLA action interleavings | Every completion is modeled as an independently schedulable action | Reordering is not hidden by a combined action |

The model does not prove provider semantics, byte integrity, or Rust adapter
correctness. Those claims remain covered by the deterministic simulator and
the runtime/LTX qualification suites.
