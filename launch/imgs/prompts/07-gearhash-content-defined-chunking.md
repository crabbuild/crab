---
illustration_id: 07
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 07-gearhash-content-defined-chunking.png
---

# Gearhash Content-Defined Chunking

Use case: scientific-educational infographic.

Primary request: completely redraw the reference image as a minimal three-card comparison. Preserve only its blueprint palette. Discard its composition, long ribbons, guides, arrows, brackets, and side panels.

Layout: one title line, then exactly three equal-width cards in one horizontal row. Cards share identical height, padding, baselines, row positions, cell sizes, border radius, and stroke width. All edges snap to one grid. Nothing overlaps.

CARD TEMPLATE:
- Heading at one fixed baseline.
- `Before` label at one fixed x-position, followed by a compact row of equal rectangular chunk cells.
- `After` label directly below at the same x-position, followed by another compact row using the same cell dimensions.
- One short result label at the bottom.
- No arrows or connector lines anywhere.

CARD 1 — `INSERT`:
- Before row: four blue cells `A`, `B`, `C`, `D`.
- After row: blue `A`, narrow amber `+`, amber `X`, blue `C`, blue `D`.
- Result: `C and D reused`.

CARD 2 — `DELETE`:
- Before row: blue `A`, blue `B`, narrow amber struck-through `−`, blue `C`, blue `D`.
- After row: blue `A`, amber `X`, blue `C`, blue `D`. This row is visibly shorter.
- Result: `C and D reused`.

CARD 3 — `MODIFY`:
- Before row: four blue cells `A`, `B`, `C`, `D`.
- After row: blue `A`, amber `X`, blue `C`, blue `D`. Both rows have equal total length.
- Result: `C and D reused`.

FOOTER: one quiet centered line: `Local edit → local new chunk → later chunks reused`.

LABELS: Render only these labels, spelled exactly: `CONTENT-DEFINED CHUNKING`, `INSERT`, `DELETE`, `MODIFY`, `Before`, `After`, `A`, `B`, `C`, `D`, `+`, `−`, `X`, `C and D reused`, `Local edit → local new chunk → later chunks reused`.

COLORS: solid off-white background, deep slate outlines and text, engineering-blue unchanged chunks, amber edit markers and new chunk `X`. Color values are rendering guidance only; never display color names or hex values.

STYLE: flat 2D minimal blueprint table; perfectly straight horizontal and vertical edges; uniform cells; exact alignment; generous whitespace; large readable text. No perspective, shadows, gradients, icons, curves, grids inside cells, variable box sizes, floating labels, arrows, connector lines, metrics, or extra text.

ASPECT: 16:9 landscape.
