# Results lab: classification + forecasting

A dependency-free Python 3.10+ example with four real experiment configurations.
Synthetic data, fixed seeds, actual fitting, separate train/validation/test data.
No downloaded datasets or Python packages.

```bash
python3 run-demo.py --destination /path/to/new/results-lab
```

The destination must not exist. The driver creates a local Git repository,
queues linear and interaction-feature models at two regularization strengths,
runs two workers, then freezes an immutable comparison. Open the created
repository in Crab → **Data Workflows → Results**, select all four runs and set
the linear/L2 0.001 run as baseline. Open `reports/results-lab.html` for the
self-contained interactive report; it works offline.

| View | What to explore |
| --- | --- |
| Learning | Train/validation log loss, eight curves, linked epoch zoom/cursor |
| Classification diagnostics | Precision/recall, ROC, calibration with binomial intervals |
| Cohorts | Precision, recall, accuracy; three customer cohorts; 95% Wilson intervals |
| Decision lab | Move the threshold slider, choose a cohort, brush a score range; confusion matrices update per run |
| Forecast | Demand, fitted forecast, residual bands and residuals; choose validation or test period |

The four stages are `prepare → train → evaluate`, plus an independent
`prepare → forecast` branch. Detailed chart declarations live in the top-level `plots` list; stage `plots`
entries declare the generated paths. Model JSON files are declared artifacts. Effective
parameters, metrics, templates, source data, logs and stage lineage are captured
through the normal experiment/Results path.

To run the sweep yourself from a committed copy:

```bash
crab run --validate
crab exp queue -S train.capacity=linear,interactions -S train.l2=0.001,0.1
crab exp start --jobs 2
crab results list --json
```

The driver also writes run IDs, the report ID, raw normalized charts and frozen
run documents under `reports/`. `--crab /path/to/crab` selects a specific build.
A remote is optional for local use. Configure one before publishing reports.

## Use your own visualization

`templates/*.json` are ordinary Vega-Lite 5 specifications. Crab supplies
`<DVC_METRIC_DATA>`, selected x/y, revision/grouping, labels and other supported
DVC anchors. All source-row fields remain available to authored templates:
`cohort`, `lower`, `upper`, `panel`, `actual`, `split`, and so on. Select one y
field when each row represents one observation; selecting several y fields
creates one long row per measure. JSON/YAML preserve auxiliary value types;
CSV/TSV normalize numbers and empty cells, leaving other cells as text.

Compose layers, facets and concatenated views; add bound parameters, filters,
calculations and selections. Keep the data anchor at a shared parent rather
than duplicating it in every child. These examples use documented Vega-Lite
[composition](https://vega.github.io/vega-lite/docs/spec.html),
[parameters](https://vega.github.io/vega-lite/docs/parameter.html), and
[selections](https://vega.github.io/vega-lite/docs/selection.html).

Charts use the bundled offline renderer. External data/URLs and supplied
JavaScript are refused. Results retains its existing run/series/point/byte
budgets; statistical/compound views are refused when reducing rows would change
their meaning. Aggregate upstream or split a large diagnostic into smaller
plots. Automatic synchronized cursors apply to compatible simple curves;
compound templates own their declared interactions.

## Interpretation

This is a visualization and workflow example, not a model recommendation.
Customer labels come from a nonlinear stochastic generator. Forecasts fit a
trend, optionally with weekly seasonality. Forecast bands use 1.96 times the
validation residual RMS; they illustrate residual uncertainty and do not promise
95% test coverage. Wilson intervals show binomial sampling uncertainty, not
between-training-run uncertainty. Decision-lab controls explore retained test
predictions; they do not retrain or change frozen metrics. Confusion matrices fill unobserved class pairs with explicit zero counts;
all-empty selections show no marks. A captured benchmark manifest contains hashes of both datasets and the evaluator
source, plus the two held-out split definitions.
