---
illustration_id: 06
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 06-crab-ownership-layers.png
---

# Crab Ownership Layers

Layout: a vertical five-layer contract stack, with a narrow right-hand annotation rail naming what each layer owns. The stack must read from user-visible Git state at top to publication state at bottom.

ZONES:
- Layer 1: `Git commit / tree` with owner note `Versioned names`.
- Layer 2: `Crab pointer` containing three small exact fields: `file-hash`, `size`, `shard-hint (optional)`; owner note `File identity`.
- Layer 3: `Recipe / shard terms`; owner note `Ordered reconstruction`.
- Layer 4: `Xorb byte ranges`; owner note `Immutable content`.
- Layer 5: `Manifest / ref journal`; owner note `Visible repository state`.

FLOW:
- One straight downward dependency spine.
- Pointer fields visually sit inside the pointer document, not as separate services.
- Bottom publication layer has a small CAS latch symbol.

LABELS: Render only these labels, spelled exactly: `Git commit / tree`, `Versioned names`, `Crab pointer`, `file-hash`, `size`, `shard-hint (optional)`, `File identity`, `Recipe / shard terms`, `Ordered reconstruction`, `Xorb byte ranges`, `Immutable content`, `Manifest / ref journal`, `Visible repository state`, `CAS`.

COLORS: off-white background, pale gray grid, deep slate outlines, engineering-blue identity layer, navy reconstruction and storage layers, light-blue annotations, amber CAS latch. Color values are rendering guidance only; never display color names or hex values.

STYLE: formal layered-contract blueprint; flat 2D; aligned rectangles; precise spacing; no cross-layer clutter; readable technical field names. No 3D, gradients, shadows, people, mascots, logos, or extra text.

ASPECT: 16:9 landscape.
