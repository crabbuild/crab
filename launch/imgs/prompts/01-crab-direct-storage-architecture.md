---
illustration_id: 01
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 01-crab-direct-storage-architecture.png
---

# Crab Direct-Storage Architecture

Layout: precise left-to-right systems architecture on a subtle engineering grid. Two parallel lanes leave the same developer workstation and converge conceptually at one Git commit.

ZONES:
- Left: developer workstation containing `git` and `crab CLI`.
- Center top, Git lane: `git-remote-crab` leading to `Git history` and a small `Crab pointer` document.
- Center bottom, data lane: `filter-process` leading to `Xorbs` and `Shards`.
- Right: one large `Object store` boundary containing Git packs, xorbs, shards, and repo metadata.
- Bottom callout: a crossed-out server rack icon with the exact label `No Crab data server`.

FLOW:
- `git` → `git-remote-crab` → `Git history` → `Object store`.
- `crab CLI` → `filter-process` → `Xorbs` and `Shards` → `Object store`.
- `Crab pointer` remains inside Git history and points toward the reconstruction metadata.

LABELS: Render only these labels, spelled exactly: `Developer`, `git`, `crab CLI`, `git-remote-crab`, `filter-process`, `Git history`, `Crab pointer`, `Xorbs`, `Shards`, `Object store`, `Direct storage access`, `No Crab data server`.

COLORS: off-white background, pale gray grid, deep slate structure, engineering-blue primary flows, navy storage boundary, light-blue data blocks, amber only for the no-server callout. Color values are rendering guidance only; never display color names or hex values.

STYLE: professional dark-ink blueprint infographic; flat 2D; thin consistent strokes; square corners; straight or 90-degree connectors; generous whitespace; high technical precision; large readable English labels. No 3D, gradients, shadows, mascots, decorative clouds, people, logos, or extra text.

ASPECT: 16:9 landscape, presentation-safe margins, all labels fully visible.
