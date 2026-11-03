# Workflows View

The Workflows view provides a visual DAG editor for defining and
running multi-stage data pipelines. Workflows are defined in YAML and
executed as directed acyclic graphs with dependency tracking.

## Accessing the Workflows View

- Click the **Workflow** icon in the activity bar
- Workflows are defined in `pipeline.yaml` (or `crab.yaml`) at the
  repository root

## Concepts

### Workflow

A workflow is a collection of stages connected by dependencies. Each
stage runs a command and produces outputs that downstream stages can
consume.

### Stage

A stage is a single unit of work:
- Has a name, command, and optional dependencies
- Produces outputs (files or directories)
- Can depend on outputs from other stages
- Runs only when its dependencies are satisfied

### DAG (Directed Acyclic Graph)

Stages form a DAG — each stage depends on zero or more upstream
stages. The workflow engine resolves the execution order automatically
and can run independent stages in parallel.

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  [pipeline.yaml ▾]  [+ New Stage]  [▶ Run]  [■ Stop]  [Save]    │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│         ┌──────────┐                                            │
│         │  ingest  │                                            │
│         └────┬─────┘                                            │
│              │                                                  │
│      ┌───────┴───────┐                                          │
│      │               │                                          │
│  ┌───▼────┐    ┌─────▼─────┐                                    │
│  │ clean  │    │ transform │                                    │
│  └───┬────┘    └─────┬─────┘                                    │
│      │               │                                          │
│      └───────┬───────┘                                          │
│              │                                                  │
│         ┌────▼─────┐                                            │
│         │  train   │                                            │
│         └────┬─────┘                                            │
│              │                                                  │
│         ┌────▼─────┐                                            │
│         │ evaluate │                                            │
│         └──────────┘                                            │
│                                                                 │
├─────────────────────────────────────────────────────────────────┤
│  Properties Panel (selected stage)                              │
│  Name: train                                                    │
│  Command: python train.py --epochs 50                           │
│  Deps: clean:output, transform:features                         │
│  Outputs: model.safetensors, metrics.json                       │
└─────────────────────────────────────────────────────────────────┘
```

## Features

### Visual DAG Editor

The workflow is rendered as an interactive node graph using
`@xyflow/react`:
- **Nodes** — each stage is a draggable node
- **Edges** — dependency connections between stages
- **Auto-layout** — topological sort arranges nodes top-to-bottom
- **Minimap** — overview for large workflows
- **Zoom/Pan** — navigate complex graphs

### Stage Nodes

Each node shows:
- Stage name
- Run status (idle, running, completed, failed)
- Color-coded border by status
- Click to select and edit properties

### Stage Properties Panel

When a stage is selected, the bottom panel shows:
- **Name** — stage identifier
- **Command** — shell command to execute
- **Dependencies** — upstream stage outputs this stage consumes
- **Outputs** — files/directories this stage produces
- **Environment** — environment variables
- **Working directory** — execution context

### Creating Stages

1. Click **+ New Stage** in the toolbar
2. Enter stage name and command
3. Optionally add dependencies (select from existing stage outputs)
4. The node appears in the graph

### Connecting Stages

- Drag from a stage's output port to another stage's input port
- Or edit dependencies in the Properties Panel
- The editor validates that connections don't create cycles

### Running Workflows

1. Click **▶ Run** to execute the entire workflow
2. Or right-click a stage → **Run from here** (runs this stage and
   all downstream)
3. Progress shows per-stage status:
   - ⏳ Waiting (dependencies not met)
   - 🔄 Running
   - ✅ Completed
   - ❌ Failed (with error output)

### Status Dashboard

A collapsible panel shows:
- Overall workflow status
- Per-stage timing
- Output file sizes
- Error logs for failed stages

### Experiment Panel

For ML workflows, the experiment panel tracks:
- Hyperparameters per run
- Metrics (loss, accuracy, etc.)
- Comparison across runs
- Link to experiment tracking service

### Workflow Tabs

Multiple workflow files can be open simultaneously:
- Tab bar at the top shows open workflows
- Switch between `pipeline.yaml`, `ci.yaml`, etc.

### Migration Wizard

For repositories migrating from other pipeline tools (DVC, MLflow):
- Detects existing pipeline definitions
- Offers guided migration to Crab workflow format
- Preserves stage structure and dependencies

## YAML Format

```yaml
stages:
  ingest:
    cmd: python scripts/ingest.py
    outputs:
      - data/raw/

  clean:
    cmd: python scripts/clean.py
    deps:
      - ingest:data/raw/
    outputs:
      - data/clean/

  train:
    cmd: python scripts/train.py --epochs 50
    deps:
      - clean:data/clean/
    outputs:
      - models/latest.safetensors
      - metrics/train.json
```

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+S` | Save workflow |
| `Cmd+Shift+R` | Run workflow |
| `Delete` | Remove selected stage |
| `Cmd+Z` | Undo |
| `Cmd+Shift+Z` | Redo |
| `Cmd+A` | Select all stages |
| `Escape` | Deselect |

## Integration

- Workflow outputs are tracked as Crab large files
- Experiment runs are linked to commits in the Timeline
- The Dashboard shows recent workflow run status
- Push operations include workflow output files
