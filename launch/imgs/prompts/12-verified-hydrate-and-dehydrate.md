---
illustration_id: 12
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 12-verified-hydrate-and-dehydrate.png
---

# Verified Hydrate and Dehydrate

Layout: two horizontal lanes with opposing directions. The upper hydration lane reconstructs and verifies before an atomic file appears. The lower dehydration lane permits pointer replacement only after clean-and-verified checks.

ZONES:
- Upper heading: `Hydrate`.
- Upper flow: `Crab pointer` → `Ordered recipe` → `Coalesced xorb ranges` → `Cache / origin` → `BLAKE3 whole-file verify` → `Atomic materialization`.
- Lower heading: `Dehydrate`.
- Lower flow: `Working file` → gate `Clean + verified` → `Crab pointer`.
- Lower rejected branch: `Modified` → `Keep working file`.

FLOW:
- Hydration must pass verification before atomic materialization.
- Dehydration gate sends only clean verified content to the pointer outcome.
- Modified content takes the keep-file branch.

LABELS: Render only these labels, spelled exactly: `Hydrate`, `Crab pointer`, `Ordered recipe`, `Coalesced xorb ranges`, `Cache / origin`, `BLAKE3 whole-file verify`, `Atomic materialization`, `Dehydrate`, `Working file`, `Clean + verified`, `Modified`, `Keep working file`.

COLORS: off-white background, pale gray grid, deep slate input states, engineering-blue verified flow, navy recipe/range blocks, light-blue atomic output, amber rejection branch. Color values are rendering guidance only; never display color names or hex values.

STYLE: correctness-focused lifecycle blueprint; flat 2D; explicit gates and direction arrows; verification must visually precede materialization. No 3D, gradients, shadows, people, mascots, logos, or hash strings.

ASPECT: 16:9 landscape.
