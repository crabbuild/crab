---
illustration_id: 07
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 07-gearhash-content-defined-chunking.png
---

# Gearhash Content-Defined Chunking

Layout: before-and-after byte ribbon comparison. A small insertion appears near the start of the lower ribbon. Chunk boundaries shift locally, then visibly resynchronize so later chunks match the upper ribbon.

ZONES:
- Top ribbon heading: `Original file`.
- Bottom ribbon heading: `After small insertion`.
- Both ribbons divided into chunks with boundary ticks and matching chunk IDs after resynchronization.
- Small inserted segment labeled `insert`.
- Curved alignment guides show matching later chunks.
- Right metric panel: `Gearhash CDC`, `64 KiB target`, `128 KiB cap`, `Local resynchronization`.

LABELS: Render only these labels, spelled exactly: `Original file`, `After small insertion`, `insert`, `Gearhash CDC`, `64 KiB target`, `128 KiB cap`, `Local resynchronization`.

COLORS: off-white background, pale gray grid, deep slate byte ribbons, engineering-blue stable chunk boundaries, light-blue matching chunks, amber inserted bytes and locally changed chunks, navy metric panel. Color values are rendering guidance only; never display color names or hex values.

STYLE: technical algorithm infographic, not decorative blocks; flat 2D; precise equal-height byte ribbons; boundary ticks and matching guides must make resynchronization immediately clear. No 3D, gradients, shadows, binary filler text, people, mascots, logos, or unlisted size values.

ASPECT: 16:9 landscape.
