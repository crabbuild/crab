---
illustration_id: 05
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 05-git-filter-and-remote-helper.png
---

# Crab Uses Two Git Extension Boundaries

Layout: two horizontal technical lanes. Top lane explains the content filter round trip. Bottom lane explains Git transport through the remote helper.

ZONES:
- Top heading: `Content boundary`.
- Top forward path: `Working file` → `filter=crab clean` → `Crab pointer` → `Git index`.
- Top reverse path: `Git index` → `smudge / hydrate` → `Working file`.
- Bottom heading: `Transport boundary`.
- Bottom path: `git push / fetch` → `crab:// remote` → `git-remote-crab` → `Object store`.
- Center callout spanning both lanes: `Git invokes Crab`.

LABELS: Render only these labels, spelled exactly: `Content boundary`, `Working file`, `filter=crab clean`, `Crab pointer`, `Git index`, `smudge / hydrate`, `Transport boundary`, `git push / fetch`, `crab:// remote`, `git-remote-crab`, `Object store`, `Git invokes Crab`.

COLORS: off-white background, pale gray grid, deep slate Git-owned blocks, engineering-blue Crab boundary blocks, navy object store, light-blue arrows, amber only for the spanning callout. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact two-lane integration diagram; flat 2D; orthogonal arrows; directional arrowheads must be unambiguous; compact but readable. No 3D, gradients, shadows, terminals full of code, people, mascots, logos, or extra labels.

ASPECT: 16:9 landscape.
