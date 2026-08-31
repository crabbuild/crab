---
illustration_id: 02
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 02-crab-open-source-module-map.png
---

# Crab Open-Source Module Map

Layout: layered module architecture with three executable entry points on top, shared ownership crates in the middle, and object storage at the base. Use aligned rectangular modules, not a generic dependency cloud.

ZONES:
- Top entry points: `crab CLI`, `git-remote-crab`, `filter-process`.
- Middle ownership modules in two tidy rows: `crab-git`, `crab-staging`, `crab-metadata`, `crab-storage`, `crab-xet`, `crab-read`, `crab-coordination`.
- Base: `Object storage`.
- Small side badges: `Rust 2024`, `Apache 2.0`.

FLOW:
- Entry points connect downward only to relevant shared modules.
- Shared modules connect downward to the single object-storage boundary.
- Emphasize that product composition is at the top and reusable contracts are below.

LABELS: Render only these labels, spelled exactly: `Product entry points`, `crab CLI`, `git-remote-crab`, `filter-process`, `Shared Rust contracts`, `crab-git`, `crab-staging`, `crab-metadata`, `crab-storage`, `crab-xet`, `crab-read`, `crab-coordination`, `Object storage`, `Rust 2024`, `Apache 2.0`.

COLORS: off-white background, pale gray grid, deep slate labels, engineering-blue entry points, navy shared modules, light-blue storage layer, amber badges. Color values are rendering guidance only; never display color names or hex values.

STYLE: exact flat 2D software architecture blueprint; consistent line weight; orthogonal connectors; strong hierarchy; minimal readable text; no icons that imply unlisted services. No 3D, gradients, shadows, decorative code, mascots, or vendor logos.

ASPECT: 16:9 landscape, spacious and balanced.
