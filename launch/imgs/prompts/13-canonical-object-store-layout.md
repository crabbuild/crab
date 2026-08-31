---
illustration_id: 13
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 13-canonical-object-store-layout.png
---

# Canonical Object-Store Layout

Layout: one object-store boundary split into two clean columns. The global column explicitly separates content-addressed objects from the shared mutable index. The repository column gives each path its own exact ownership badge. Render paths as a concise tree.

ZONES:
- Outer boundary: `Object store`.
- Left heading: `Global shared namespace`.
- Left tree root `.crab/` with `xorbs/{first-two}/{hash}` and `shards/{first-two}/{hash}` grouped under `Content-addressed`; place `chunk_index_db/` separately with badge `Shared index`.
- Right heading: `Repo-local namespace`.
- Right tree root `{repo}/` with `manifest` badge `CAS-mutable`; `packs/` badge `Immutable packs`; `file_index_db/` badge `Repo index`; `locks/` badge `Coordination`.

FLOW:
- No runtime-flow arrows; this is a namespace and ownership map.
- Hash marks appear only beside xorbs and shards. The chunk and file indexes are database/index state, not content-addressed objects. A CAS mark appears only beside manifest. Locks are coordination state.

LABELS: Render only these labels, spelled exactly: `Object store`, `Global shared namespace`, `.crab/`, `xorbs/{first-two}/{hash}`, `shards/{first-two}/{hash}`, `Content-addressed`, `chunk_index_db/`, `Shared index`, `Repo-local namespace`, `{repo}/`, `manifest`, `CAS-mutable`, `packs/`, `Immutable packs`, `file_index_db/`, `Repo index`, `locks/`, `Coordination`.

COLORS: off-white background, pale gray grid, deep slate tree lines, engineering-blue content-addressed paths, navy outer boundary, light-blue index and repo-local paths, amber only for CAS and coordination marks. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact namespace tree blueprint; flat 2D; monospaced-looking but highly readable path labels; preserve braces and punctuation exactly. No 3D, gradients, shadows, bucket photography, people, mascots, vendor logos, or extra paths.

ASPECT: 16:9 landscape.
