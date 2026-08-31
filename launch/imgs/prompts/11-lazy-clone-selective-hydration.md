---
illustration_id: 11
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 11-lazy-clone-selective-hydration.png
---

# Lazy Clone, Selective Hydration

Layout: three-step worktree sequence. First clone receives Git history and lightweight pointers; second selects only matching paths; third materializes exact files while unselected paths remain pointers.

ZONES:
- Step 1: command `crab clone` above a worktree labeled `Git history + pointers` and badge `Lazy by default`.
- Step 2: command `crab hydrate data/train/**` above a selection funnel labeled `Selected paths only`.
- Step 3: worktree with two solid files labeled `Hydrated files` and several outlined pointer documents labeled `Pointers remain`.
- Bottom data path: selected pointer → `Object store` → `Exact file`.

FLOW:
- Step arrows move left to right.
- Only selected-path arrows descend to object storage and return as files.
- Unselected pointer documents must not receive data arrows.

LABELS: Render only these labels, spelled exactly: `crab clone`, `Git history + pointers`, `Lazy by default`, `crab hydrate data/train/**`, `Selected paths only`, `Hydrated files`, `Pointers remain`, `Object store`, `Exact file`.

COLORS: off-white background, pale gray grid, deep slate worktrees, engineering-blue selected path, navy object store, light-blue hydrated files, amber selection funnel. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact CLI workflow blueprint; flat 2D; simple file and pointer glyphs; clear selective flow; commands must be large and readable. No 3D, gradients, shadows, people, mascots, logos, or extra filesystem paths.

ASPECT: 16:9 landscape.
