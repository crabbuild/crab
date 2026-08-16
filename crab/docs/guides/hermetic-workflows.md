# Hermetic Workflows

Workflow sandbox enforcement for declared dependencies and outputs.

Status: `hermetic: true` in `crab.yaml` and inline `crab run --hermetic` execute
through the macOS `sandbox-exec` backend when available. Unsupported platforms
fail before launching the stage command. Hermetic cache entries include the
sandbox policy version so policy changes do not reuse stale results.

## Current Behavior

```yaml
stages:
  train:
    cmd: "python train.py"
    deps:
      - data/train.csv
    outs:
      - model.bin
    hermetic: true
```

Declaring `hermetic: true` allows reads from declared `deps`, writes to
declared `outs`, and writes to the per-stage sandbox temp directory. Reads or
writes outside that policy fail the stage with a structured hermetic violation
that includes the stage and offending path.

Inline single-stage runs use the same enforcement:

```bash
crab run --name prep --deps data/raw.csv --outs data/clean.csv --hermetic -- \
  python scripts/prep.py
```

## Current Scope

The first enforcing backend is macOS `sandbox-exec`. Other platforms fail
closed until a backend is implemented. Network access and undeclared repository
filesystem access are denied by default; system reads needed to launch normal
commands are allowed.
