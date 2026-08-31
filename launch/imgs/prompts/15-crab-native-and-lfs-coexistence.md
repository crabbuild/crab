---
illustration_id: 15
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 15-crab-native-and-lfs-coexistence.png
---

# Crab Native and LFS Coexistence

Layout: one Git repository at left branches through two explicit `.gitattributes` filter lanes, then rejoins at one object-store boundary. Treat the formats as parallel and distinct.

ZONES:
- Left: `One Git repository` and `Git commits`.
- Upper lane: `filter=crab` → `Crab pointer` → `Chunks + xorbs` → `BLAKE3`.
- Lower lane: `filter=lfs` → `LFS pointer` → `Whole-file object` → `SHA-256`.
- Right: `Object store` containing two clearly separated namespaces.
- Bottom callout: `No LFS server required`.

FLOW:
- Both pointer files live in Git commits.
- Each filter lane connects only to its own object representation and hash family.
- Both storage representations terminate at the same provider-neutral object-store boundary without merging formats.

LABELS: Render only these labels, spelled exactly: `One Git repository`, `Git commits`, `filter=crab`, `Crab pointer`, `Chunks + xorbs`, `BLAKE3`, `filter=lfs`, `LFS pointer`, `Whole-file object`, `SHA-256`, `Object store`, `No LFS server required`.

COLORS: off-white background, pale gray grid, deep slate Git boundary, engineering-blue Crab lane, navy LFS lane, light-blue storage namespaces, amber no-server callout. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact compatibility architecture blueprint; flat 2D; symmetric lanes; preserve algorithm punctuation exactly; no implication that Crab and LFS object formats are identical. No 3D, gradients, shadows, people, mascots, logos, or vendor marks.

ASPECT: 16:9 landscape.
