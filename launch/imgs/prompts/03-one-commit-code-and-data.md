---
illustration_id: 03
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 03-one-commit-code-and-data.png
---

# One Commit Selects Code and Data

Layout: one Git commit and tree on the left branching to two tracked file entries; the ordinary file terminates in a Git blob, while the large file terminates in a Crab pointer and then a precise reconstruction chain.

ZONES:
- Left: `Git commit` above `Git tree`.
- Upper branch: `src/model.rs` → `Git blob`.
- Lower branch: `data/train.bin` → `Crab pointer`.
- Right reconstruction chain: `File identity` → `Recipe / shard` → `Xorb ranges` → `Exact bytes`.

FLOW:
- Both branches originate from the same Git tree.
- The pointer branch uses a solid identity arrow, not a vague external hyperlink.
- A bracket across both branches carries `One commit`.

LABELS: Render only these labels, spelled exactly: `One commit`, `Git commit`, `Git tree`, `src/model.rs`, `Git blob`, `data/train.bin`, `Crab pointer`, `File identity`, `Recipe / shard`, `Xorb ranges`, `Exact bytes`.

COLORS: off-white background, pale gray grid, deep slate Git structure, engineering-blue pointer and identity path, navy reconstruction blocks, light-blue byte blocks, amber only on `One commit`. Color values are rendering guidance only; never display color names or hex values.

STYLE: rigorous flat 2D data-model blueprint; clean tree edges; no literal source-code text; no generic cloud art; large legible labels and generous whitespace. No 3D, gradients, shadows, people, mascots, logos, or extra text.

ASPECT: 16:9 landscape.
