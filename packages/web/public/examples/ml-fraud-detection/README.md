# Crab ML fraud-detection example

This dependency-free Python 3 example exercises a four-stage Crab workflow:

```text
transactions.csv -> raw.csv -> features.csv -> fraud-model.pkl -> metrics + plot
```

Run it from this directory in a Git repository with the `crab` CLI installed:

```bash
git init
crab run --validate
crab workflow dag
crab run --parallelism 2 --json
crab workflow status --json
crab metrics show
crab plots show --json
python3 src/smoke_model.py
python3 src/check_quality_gate.py
crab run --parallelism 2 --json
```

The final run should report a cache hit for all four stages. No Python packages
outside the standard library are required. A Crab remote is optional for these
local commands; configure one before using remote cache, experiment sharing, or
clean-client examples from the accompanying guides.

The model uses Python pickle to match the artifact examples. Load pickle files
only when their source and content identity are trusted.
