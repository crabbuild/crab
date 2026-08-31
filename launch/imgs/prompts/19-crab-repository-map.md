---
illustration_id: 19
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 19-crab-repository-map.png
---

# Crab Open-Source Repository Map

Layout: monorepo tree on the left and a concise verified project-facts panel on the right. Each top-level repository surface gets one responsibility note.

ZONES:
- Left root: `CrabBuild/`.
- Tree entries: `crab/` with note `CLI + product wiring`; `crates/` with note `Shared Rust contracts`; `packages/web/` with note `Site + docs`; `diagram/` with note `Architecture assets`; `.github/workflows/` with note `CI + release evidence`.
- Right facts panel: `Rust 2024`, `Apache 2.0`, `20 workspace members`, `19 shared crates + crab`.
- Bottom label: `Open source, inspectable end to end`.

FLOW:
- No dependency arrows. This is a repository ownership tree.
- Use consistent folder nodes and a thin brace connecting the facts panel to the whole monorepo.

LABELS: Render only these labels, spelled exactly: `CrabBuild/`, `crab/`, `CLI + product wiring`, `crates/`, `Shared Rust contracts`, `packages/web/`, `Site + docs`, `diagram/`, `Architecture assets`, `.github/workflows/`, `CI + release evidence`, `Rust 2024`, `Apache 2.0`, `20 workspace members`, `19 shared crates + crab`, `Open source, inspectable end to end`.

COLORS: off-white background, pale gray grid, deep slate tree lines, engineering-blue folder nodes, navy root and facts panel, light-blue responsibility notes, amber license/language badges. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact repository-tree blueprint; flat 2D; preserve path punctuation; no fake files or packages. No 3D, gradients, shadows, people, mascots, vendor logos, or operating-system icons.

ASPECT: 16:9 landscape.
