---
illustration_id: 09
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 09-immutable-first-visibility-last.png
---

# Immutable First, Visibility Last

Layout: a strict left-to-right push publication pipeline with a bold gate immediately before the final visible ref update. Earlier stages are immutable uploads; the last stage is a CAS publication event.

ZONES:
- Stage 1: `Stage / prepare`.
- Stage 2: `Upload xorbs`.
- Stage 3: `Upload shards + metadata`.
- Stage 4: `Upload Git packs`.
- Stage 5: `Validate dependency closure`.
- Gate: `All immutable data durable`.
- Final stage: `Manifest CAS / ref commit`.
- Above stages 2–5, a bracket labeled `Immutable first`.
- Above final stage, a bracket labeled `Visibility last`.

FLOW:
- One unbroken ordered arrow through every stage.
- No arrow may bypass dependency validation.
- Final stage visually flips a ref from old to new only after the gate.

LABELS: Render only these labels, spelled exactly: `Stage / prepare`, `Upload xorbs`, `Upload shards + metadata`, `Upload Git packs`, `Validate dependency closure`, `All immutable data durable`, `Manifest CAS / ref commit`, `Immutable first`, `Visibility last`, `Old ref`, `New ref`.

COLORS: off-white background, pale gray grid, deep slate stage outlines, engineering-blue completed immutable stages, navy gate, light-blue durable objects, amber only on the visibility/CAS transition. Color values are rendering guidance only; never display color names or hex values.

STYLE: precise transaction/publication blueprint; flat 2D; numbered stages are permitted but no extra prose; strong causal order and gate semantics. No 3D, gradients, shadows, people, mascots, logos, or decorative cloud icons.

ASPECT: 16:9 landscape.
