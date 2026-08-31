---
illustration_id: 17
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 17-failure-stage-outcomes.png
---

# Failure Stage Outcomes

Layout: compact five-row failure matrix. Each row has exactly three columns: stage, interruption/failure, and durable outcome. Use simple state icons and short exact labels.

ZONES:
- Header columns: `Stage`, `Failure`, `Durable outcome`.
- Row 1: `Local staging` | `fails` | `No pointer committed`.
- Row 2: `Immutable upload` | `fails` | `Old tip remains`.
- Row 3: `Manifest CAS` | `conflict` | `Winning tip remains`.
- Row 4: `Response after commit` | `lost` | `Read canonical state`.
- Row 5: `Derived index` | `fails` | `Rebuild index`.
- Bottom summary: `Failure before visibility preserves prior state`.

LABELS: Render only these labels, spelled exactly: `Stage`, `Failure`, `Durable outcome`, `Local staging`, `fails`, `No pointer committed`, `Immutable upload`, `Old tip remains`, `Manifest CAS`, `conflict`, `Winning tip remains`, `Response after commit`, `lost`, `Read canonical state`, `Derived index`, `Rebuild index`, `Failure before visibility preserves prior state`.

COLORS: off-white background, pale gray grid, deep slate table structure, engineering-blue safe outcomes, navy stage cells, light-blue canonical/rebuild outcomes, amber failure cells. Color values are rendering guidance only; never display color names or hex values.

STYLE: highly legible systems-failure matrix; flat 2D; consistent rows and column alignment; no paragraph text; use exact causal outcomes. No 3D, gradients, shadows, dramatic explosion/error art, people, mascots, logos, or stack traces.

ASPECT: 16:9 landscape.
