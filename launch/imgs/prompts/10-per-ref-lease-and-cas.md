---
illustration_id: 10
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 10-per-ref-lease-and-cas.png
---

# Per-Ref Lease and CAS

Layout: two symmetric writers race toward the same ref in the center. A per-ref lease serializes the critical publication window; an expected-old CAS gives one writer the new tip while the other is directed to reconcile.

ZONES:
- Left top: `Pusher A` with `expected old: R0` and candidate `R1`.
- Left bottom: `Pusher B` with `expected old: R0` and candidate `R2`.
- Center: lock boundary `Lease: refs/heads/main` with small `heartbeat` pulse.
- Right center: `CAS` comparing the expected old value.
- Right top outcome: `Winner: R1` and `New tip`.
- Right bottom outcome: `Conflict` → `Fetch + reconcile`.
- Small side note: `Different refs do not share this ref lock`.

FLOW:
- Both pushers enter the same per-ref lease queue.
- Pusher A succeeds at CAS; Pusher B sees changed canonical state and does not overwrite it.

LABELS: Render only these labels, spelled exactly: `Pusher A`, `Pusher B`, `expected old: R0`, `R1`, `R2`, `Lease: refs/heads/main`, `heartbeat`, `CAS`, `Winner: R1`, `New tip`, `Conflict`, `Fetch + reconcile`, `Different refs do not share this ref lock`.

COLORS: off-white background, pale gray grid, deep slate writers, engineering-blue winner path, navy lease and CAS blocks, light-blue new-tip state, amber conflict path. Color values are rendering guidance only; never display color names or hex values.

STYLE: concurrency-control blueprint; flat 2D; clean queue and state-transition symbols; never depict two winners. No 3D, gradients, shadows, people, mascots, logos, or extra branch names.

ASPECT: 16:9 landscape.
