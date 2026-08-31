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
- One centered operation line directly below the Before row. It explicitly names what happens to chunk `B`.
- `After` label directly below the operation line at the same x-position, followed by another compact row using the same four cell dimensions.
- One short result label at the bottom.
- The arrow glyph inside the operation text is the only arrow. No connector lines anywhere.

CARD 1 — `INSERT`:
- Before row: four blue cells `A`, `B`, `C`, `D`.
- Operation line: `B + bytes → X`.
- After row: blue `A`, amber `X`, blue `C`, blue `D`.
- Result: `C and D reused`.

CARD 2 — `DELETE`:
- Before row: four blue cells `A`, `B`, `C`, `D`.
- Operation line: `B − bytes → X`.
- After row: blue `A`, amber `X`, blue `C`, blue `D`.
- Result: `C and D reused`.

CARD 3 — `MODIFY`:
- Before row: four blue cells `A`, `B`, `C`, `D`.
- Operation line: `edit B → X`.
- After row: blue `A`, amber `X`, blue `C`, blue `D`. Both rows have equal total length.
- Result: `C and D reused`.

FOOTER: one quiet centered line: `B changes locally. C and D are reused.`.

LABELS: Render only these labels, spelled exactly: `CONTENT-DEFINED CHUNKING`, `INSERT`, `DELETE`, `MODIFY`, `Before`, `After`, `A`, `B`, `C`, `D`, `X`, `B + bytes → X`, `B − bytes → X`, `edit B → X`, `C and D reused`, `B changes locally. C and D are reused.`.

COLORS: solid off-white background, deep slate outlines and text, engineering-blue unchanged chunks, amber edit markers and new chunk `X`. Color values are rendering guidance only; never display color names or hex values.

STYLE: flat 2D minimal blueprint table; perfectly straight horizontal and vertical edges; exactly four equal cells in every Before and After row; exact alignment; generous whitespace; large readable text. No perspective, shadows, gradients, icons, curves, grids inside cells, variable box sizes, floating labels, connector lines, metrics, or extra text.

ASPECT: 16:9 landscape.
