---
illustration_id: 18
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 18-durable-truth-vs-acceleration.png
---

# Durable Truth vs Acceleration

Layout: two-tier architecture. The lower foundation contains authoritative durable state. The upper tier contains rebuildable acceleration structures. Verification and repair tools bridge the two tiers.

ZONES:
- Lower heading: `Authoritative durable truth`.
- Lower blocks: `Canonical origin`, `Manifest / ref journal`, `Immutable objects`.
- Upper heading: `Rebuildable acceleration`.
- Upper blocks: `Cache`, `File index`, `Chunk index`, `Visibility proof`.
- One rule banner between tiers: `Cache or index hit is a candidate, not origin proof`.
- Right tool rail: `crab doctor`, `crab fsck`, `JSON / JSONL`, `CRAB-E####`.
- Repair arrows run from durable truth upward to rebuild acceleration.

FLOW:
- Read candidates may come from acceleration, but authority arrows terminate at durable truth.
- Rebuild arrows originate at canonical state.
- Doctor/fsck inspect both tiers without becoming authoritative state.

LABELS: Render only these labels, spelled exactly: `Authoritative durable truth`, `Canonical origin`, `Manifest / ref journal`, `Immutable objects`, `Rebuildable acceleration`, `Cache`, `File index`, `Chunk index`, `Visibility proof`, `Cache or index hit is a candidate, not origin proof`, `crab doctor`, `crab fsck`, `JSON / JSONL`, `CRAB-E####`.

COLORS: off-white background, pale gray grid, deep slate structure, engineering-blue authoritative layer, navy canonical blocks, light-blue rebuildable blocks, amber rule banner and diagnostic rail. Color values are rendering guidance only; never display color names or hex values.

STYLE: clean two-tier reliability blueprint; flat 2D; foundation metaphor expressed structurally, not as a building illustration; exact tooling labels. No 3D, gradients, shadows, people, mascots, logos, or extra error codes.

ASPECT: 16:9 landscape.
