# Templating Reference

Crab supports `${expr}` variable substitution in `crab.yaml`. This
lets you reference parameters, variables, and environment values directly
in stage definitions without duplicating values.

## Table of Contents

- [Syntax](#syntax)
- [Resolution order](#resolution-order)
- [vars: block](#vars-block)
- [Params files](#params-files)
- [Environment variables](#environment-variables)
- [Escaping](#escaping)
- [Array index access](#array-index-access)
- [Interaction with stage hashing](#interaction-with-stage-hashing)
- [Error messages](#error-messages)

## Syntax

Substitution expressions use `${expr}` where `expr` is a dot-separated
path into the value tree:

```yaml
stages:
  train:
    cmd: "python train.py --lr ${model.lr} --epochs ${model.epochs}"
    deps:
      - ${paths.data_dir}/features.parquet
    outs:
      - ${paths.output_dir}/model.pkl
```

Supported expression forms:

| Form | Example | Resolves to |
|------|---------|-------------|
| Top-level key | `${codedir}` | Value of `codedir` |
| Nested path | `${model.lr}` | `model` → `lr` |
| Deep nesting | `${a.b.c.d}` | Arbitrary depth |
| Array index | `${data.files[0]}` | First element of `data.files` |
| Environment | `${env.HOME}` | `$HOME` env var |

Expressions can appear in any string-valued field: `cmd`, dep paths, out
paths, `timeout`, and stage-output references.

## Resolution order

When resolving `${expr}`, crab searches sources in this order:

1. **Environment variables** — only for expressions with the `${env.VAR}` prefix
2. **Params files** — `params.yaml`, `params.json`, `params.py`, or custom files
3. **`vars:` block** — inline key-value pairs and file references

If the same key exists in both `vars:` and a params file, the **params
value wins**. This is intentional: params are tracked inputs that affect
the stage hash, while vars are structural values that don't.

## vars: block

The top-level `vars:` key defines variables directly in `crab.yaml`.
Use vars for structural values (paths, script names) that don't affect
whether a stage needs to re-run.

### Inline maps

```yaml
vars:
  - codedir: src
  - datadir: data/processed
  - model_name: resnet50
```

Each list entry is a single key-value pair.

### File references

```yaml
vars:
  - config/paths.yaml
```

Loads all keys from the referenced file and makes them available for
substitution. The file can be YAML, JSON, or TOML.

### Selective imports

```yaml
vars:
  - config/settings.yaml:paths,defaults
```

Loads only the `paths` and `defaults` top-level keys from the file.
Other keys in the file are ignored.

### Combined

```yaml
vars:
  - codedir: src
  - config/paths.yaml
  - config/team.yaml:owner,cost_center
```

All three forms can be mixed in a single `vars:` block. Later entries
override earlier ones on key conflicts.

## Params files

Params files are declared at the top level and are available for
templating and bare stage-level param refs. Supported formats: YAML,
JSON, TOML, and Python literal assignments.

```yaml
params:
  - params.yaml
  - config/hyperparams.json
  - settings.toml
  - params.py
```

Given `params.yaml`:

```yaml
model:
  lr: 0.001
  epochs: 50
  arch: resnet
data:
  batch_size: 32
  augment: true
```

You can reference any nested value:

```yaml
stages:
  train:
    cmd: "python train.py --lr ${model.lr} --arch ${model.arch}"
    params:
      - model.lr
      - model.epochs
      - model.arch
```

The `params:` list on a stage declares which param keys are tracked
dependencies. Bare refs like `model.lr` resolve against the top-level
params files, or `params.yaml` if no top-level list is declared. The
`${...}` syntax resolves the value for use in strings.

Stage params can also use DVC-style file-scoped refs:

```yaml
stages:
  train:
    cmd: "python train.py --lr ${model.lr}"
    params:
      - model.lr
      - custom.yaml:
          - model.dropout
          - data.batch_size
      - sweep.json:
```

The `custom.yaml` entry tracks only the listed keys. The `sweep.json:`
entry tracks every scalar value in that file. File-scoped refs are
written to `crab.lock` and status output as `file:key`, for example
`custom.yaml:model.dropout`.

Python params files are parsed without executing repository code. Crab
tracks literal top-level assignments, class constants, and `self.*`
assignments inside `__init__`, including scalars, lists, dicts, and
`dict(key=value)` values for stage hashing and status. Dynamic Python
expressions are ignored, so a tracked dynamic key is reported as missing.
Experiment overrides rewrite YAML, JSON, TOML, and Python literal params
files. For custom params files, prefix the key with the file path:
`crab exp run --set-param custom.yaml:model.dropout=0.2` or
`crab exp queue -S custom.yaml:model.dropout=0.2`. Python
write-back updates the same safe literal subset Crab tracks, including
class params such as `params.py:TrainConfig.layers=12`; dynamic Python
expressions remain read-only because executing repository code would make
experiment setup non-hermetic.

Override keys also accept DVC/Hydra-style operation prefixes:

| Form | Meaning |
|------|---------|
| `key=value` | Update an existing key |
| `+key=value` | Add a new key; fails if it already exists |
| `++key=value` | Add the key or update it if it already exists |
| `~key` | Remove an existing key |

The same prefixes work after a file scope, for example
`custom.yaml:+model.dropout=0.2` and `custom.yaml:~model.dropout`.

## Command dictionary unpacking

Inside shell-form `cmd:` strings, a mapping expression expands into CLI
arguments:

```yaml
params:
  - params.yaml
stages:
  train:
    cmd: "python train.py ${train_args}"
```

Given:

```yaml
train_args:
  lr: 0.01
  model: resnet
  nested:
    dropout: 0.2
  tags: [baseline, gpu]
```

Crab resolves the command to:

```bash
python train.py --lr 0.01 --model 'resnet' --nested.dropout 0.2 --tags 'baseline' 'gpu'
```

This unpacking is command-only. Other fields still require scalar
substitution, so `${train_args}` in `deps:`, `outs:`, `params:`, or
`wdir:` remains a configuration error.

## Environment variables

Access environment variables with the `env.` prefix:

```yaml
stages:
  deploy:
    cmd: "deploy.sh --env ${env.DEPLOY_TARGET}"
    deps:
      - dist/
```

Environment variable access is opt-in. Only variables explicitly
referenced via `${env.VAR}` are read. The full process environment is
NOT exposed by default.

Common uses:
- `${env.HOME}` — user home directory
- `${env.CI}` — detect CI environment
- `${env.CUDA_VISIBLE_DEVICES}` — GPU selection

If the referenced environment variable is not set, resolution fails with
an undefined-reference error (see [Error messages](#error-messages)).

## Escaping

To produce a literal `${...}` in the output (without substitution),
escape with a backslash:

```yaml
stages:
  report:
    cmd: "echo 'Cost: \\${total}' > report.txt"
```

The resolved command will contain the literal string `${total}` — no
substitution is attempted.

Escaping rules:
- `\${...}` → literal `${...}` (backslash consumed)
- `${...}` → substituted value
- A bare `$` not followed by `{` is left as-is (no special meaning)

## Array index access

Access list elements by zero-based index using bracket notation:

```yaml
# params.yaml
data:
  files:
    - train.csv
    - val.csv
    - test.csv
  splits: [0.7, 0.15, 0.15]
```

```yaml
stages:
  validate:
    cmd: "python validate.py --input ${data.files[1]} --split ${data.splits[1]}"
```

Resolves to: `python validate.py --input val.csv --split 0.15`

Index out of bounds produces an undefined-reference error.

## Interaction with stage hashing

This is the key distinction between vars and params:

| Source | Tracked in stage hash? | Purpose |
|--------|:----------------------:|---------|
| `vars:` | No | Structural values (paths, names) |
| Stage `params:` | Yes | Tunable inputs (hyperparameters) |
| `env.*` | Only if listed in stage `env:` | Runtime configuration |

After template resolution, the stage hash sees only the final resolved
string values. The `${...}` syntax itself never appears in the lockfile
or hash computation.

**Practical consequence:** Changing a `vars:` value does NOT invalidate
the cache (the stage won't re-run). Changing a `params:` value DOES
invalidate the cache.

Example:

```yaml
vars:
  - script_dir: src/ml    # changing this does NOT trigger re-run

stages:
  train:
    cmd: "python ${script_dir}/train.py --lr ${model.lr}"
    params:
      - model.lr           # changing this DOES trigger re-run
```

## Error messages

### Undefined reference

```
Error: WorkflowTemplateUndefined
  key: "model.learning_rate"
  field: "cmd"
  stage: "train"

  The expression ${model.learning_rate} in stage "train" field "cmd"
  could not be resolved. No matching key found in vars, params, or env.

  Available keys under "model": lr, epochs, arch
```

The error names the missing key, the field that referenced it, and the
stage. When possible, it suggests similar keys that do exist.

### Undefined environment variable

```
Error: WorkflowTemplateUndefined
  key: "env.DEPLOY_TARGET"
  field: "cmd"
  stage: "deploy"

  The environment variable DEPLOY_TARGET is not set.
  Set it before running, or use a condition to skip this stage:
    condition:
      env: DEPLOY_TARGET
```

### Index out of bounds

```
Error: WorkflowTemplateUndefined
  key: "data.files[5]"
  field: "deps"
  stage: "validate"

  Array index 5 is out of bounds. "data.files" has 3 elements (indices 0-2).
```

### Validation mode

Use `--validate` to check all substitutions without executing:

```bash
crab run --validate
```

Reports all undefined references at once (doesn't stop at the first
error), so you can fix them in a single pass.
