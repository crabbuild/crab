---
illustration_id: 07
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 07-gearhash-content-defined-chunking.png
---

# Gearhash Content-Defined Chunking

Use case: precise technical infographic explaining the local resynchronization property of content-defined chunking.

Primary request: completely redraw the reference image as a minimal three-card comparison. The three edit scenarios MUST have visibly different After rows. Preserve only the reference's blueprint palette. Discard its existing composition.

CORE TECHNICAL IDEA: insertion, deletion, and modification can produce a different number and shape of chunks near the edit. Content-defined chunking later resynchronizes with unchanged content, so later chunks can retain their identities. The exact local boundaries depend on the bytes; these are illustrative examples, not universal chunk counts.

LAYOUT:
- One title line: `CONTENT-DEFINED CHUNKING`.
- Exactly three equal-width cards in one horizontal row: `INSERT`, `DELETE`, `MODIFY`.
- Cards have identical height, padding, heading baseline, row-label positions, corner radius, and stroke width.
- Every Before row is identical and occupies the same width: five equal blue cells `A`, `B`, `C`, `D`, `E`.
- Every After row starts at the exact same x-position. Its total width deliberately differs by scenario.
- Use perfectly straight edges and snap everything to one alignment grid.
- No connector lines, crossing arrows, brackets, side panels, or decorative charts.

CARD 1 — `INSERT`:
- Before: five equal blue cells `A`, `B`, `C`, `D`, `E`.
- Operation: `insert bytes`.
- After: blue `A`, then three amber local chunks `I1`, `I2`, `I3`, then blue `D`, blue `E`.
- The After row is visibly LONGER than the Before row.
- Put a small slate label `resync` directly above the boundary before `D`, with one short vertical tick touching that boundary.

CARD 2 — `DELETE`:
- Before: five equal blue cells `A`, `B`, `C`, `D`, `E`.
- Operation: `delete bytes`.
- After: blue `A`, then one amber local chunk `R1`, then blue `D`, blue `E`.
- The After row is visibly SHORTER than the Before row.
- Put a small slate label `resync` directly above the boundary before `D`, with one short vertical tick touching that boundary.

CARD 3 — `MODIFY`:
- Before: five equal blue cells `A`, `B`, `C`, `D`, `E`.
- Operation: `modify bytes`.
- After: blue `A`, then two amber local chunks `M1`, `M2`, then blue `D`, blue `E`.
- The After row has the SAME TOTAL WIDTH as the Before row, but `M1` and `M2` have visibly different widths from the original equal cells.
- Put a small slate label `resync` directly above the boundary before `D`, with one short vertical tick touching that boundary.

VISUAL SEMANTICS:
- Blue means unchanged/reused. `A`, `D`, and `E` are blue in every After row.
- Amber means newly chunked local output. `I1`, `I2`, `I3`, `R1`, `M1`, and `M2` are amber.
- Do not show `B` or `C` in any After row: their old local boundaries no longer survive the edit.
- The repeated blue `D` and `E` after the `resync` marker are the clearest visual proof of later reuse.

FOOTER:
- Main line: `Local outputs differ. Later unchanged chunks can be reused.`
- Smaller note below: `Illustrative — exact boundaries depend on bytes.`

LABELS: Render only these labels, spelled exactly: `CONTENT-DEFINED CHUNKING`, `INSERT`, `DELETE`, `MODIFY`, `Before`, `After`, `A`, `B`, `C`, `D`, `E`, `I1`, `I2`, `I3`, `R1`, `M1`, `M2`, `insert bytes`, `delete bytes`, `modify bytes`, `resync`, `Local outputs differ. Later unchanged chunks can be reused.`, `Illustrative — exact boundaries depend on bytes.`

COLORS: solid off-white background, deep slate outlines and text, engineering-blue unchanged chunks, amber local outputs. Color values are rendering guidance only; never display color names or hex values.

STYLE: flat 2D minimal blueprint table, generous whitespace, large readable text, crisp uniform strokes, exact alignment. No perspective, shadows, gradients, icons, curves, textures, background grids, floating labels, metrics, legends, or extra text.

ASPECT: 16:9 landscape.
