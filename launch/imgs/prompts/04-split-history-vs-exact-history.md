---
illustration_id: 04
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 04-split-history-vs-exact-history.png
---

# Split History vs Exact History

Layout: equal two-column comparison separated by a precise vertical rule. Left shows independent code and data timelines with an ambiguous join. Right shows a Git commit selecting an exact file identity and verified reconstruction.

ZONES:
- Left heading: `Split histories`.
- Left top: `Git code history` with commits A, B, C.
- Left bottom: `External data history` with versions 7, 8, 9.
- Between them: dotted ambiguous connector labeled `Which data version?` and a warning tag `latest`.
- Right heading: `Commit-bound history`.
- Right: `Git commit C` → `Crab pointer` → `File identity` → `Hydration` → `Exact bytes`.
- Right proof callout: `BLAKE3 verified`.

LABELS: Render only these labels, spelled exactly: `Split histories`, `Git code history`, `External data history`, `Which data version?`, `latest`, `Commit-bound history`, `Git commit C`, `Crab pointer`, `File identity`, `Hydration`, `Exact bytes`, `BLAKE3 verified`.

COLORS: off-white background, pale gray grid, muted slate on the split-history side, amber ambiguity warning, engineering blue and navy on the commit-bound side, light-blue verified output. Color values are rendering guidance only; never display color names or hex values.

STYLE: sober comparison blueprint; exact timelines and arrows; visibly unresolved dotted relationship on the left, deterministic solid chain on the right. No 3D, gradients, shadows, people, mascots, decorative clouds, logos, or extra prose.

ASPECT: 16:9 landscape, symmetric columns.
