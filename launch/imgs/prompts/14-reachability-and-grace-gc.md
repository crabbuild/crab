---
illustration_id: 14
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 14-reachability-and-grace-gc.png
---

# Reachability and Grace-Period GC

Layout: a left-to-right mark-and-sweep diagram. Repository roots traverse to shards, files, and xorbs. Below, object inventory is compared with the reachable set, then filtered by age before collection.

ZONES:
- Top graph: `Manifest / refs` → `Shards` → `Files` → `Xorbs`.
- Reachable nodes enclosed by a blue boundary labeled `Reachable: keep`.
- Bottom set operation: `Object inventory` minus `Reachable set` equals `Unreachable`.
- Age gate after unreachable: `Grace period ≥ 1 hour`.
- Two outcomes: `Recent: keep` and `Unreachable + old: collect`.

FLOW:
- Solid traversal arrows from roots to every reachable xorb.
- Unreachable objects enter the age gate.
- Recent objects cannot enter collection.

LABELS: Render only these labels, spelled exactly: `Manifest / refs`, `Shards`, `Files`, `Xorbs`, `Reachable: keep`, `Object inventory`, `Reachable set`, `Unreachable`, `Grace period ≥ 1 hour`, `Recent: keep`, `Unreachable + old: collect`.

COLORS: off-white background, pale gray grid, deep slate graph structure, engineering-blue reachable set, navy roots, light-blue keep outcomes, amber age gate and collect-eligible outcome. Color values are rendering guidance only; never display color names or hex values.

STYLE: safety-first garbage-collection blueprint; flat 2D; exact set subtraction and age gate; collection is an eligible outcome, not a dramatic deletion icon. No 3D, gradients, shadows, trash-can mascots, people, logos, or extra time values.

ASPECT: 16:9 landscape.
