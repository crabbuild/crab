# Data Workflows

Open **Data Workflows** in the activity bar to manage repository pipelines.
Crab discovers `crab.yaml` and `*.workflow.yaml` files recursively, excluding
hidden and build directories. A file named `pipeline.yaml` or `a.yml` is not
automatically discovered; use `pipeline.workflow.yaml` or `a.workflow.yaml`.

## Workflow sidebar

The left pane lists workflow paths and stage counts. Filter the list to find a
workflow, select it to edit, or refresh to pick up files changed outside Crab.
An unreadable file stays in the list with an error indicator; other workflows
remain accessible. Drag the divider beside the list to resize it, or focus the
divider and use the arrow keys. The width is remembered between visits.

- **New workflow** creates a discoverable YAML file with an example stage.
- Hover or focus a row for **Rename** and **Delete**. Rename is available for
  local working directories and keeps the file in its current folder, so
  relative output paths retain their meaning. Renaming a named workflow changes
  its stage namespace; its next run recomputes cached state. Existing lockfiles
  are not renamed.
- **Delete** asks for confirmation in the sidebar and removes the definition.
  Produced files remain. The confirmation warns when another workflow consumes
  its outputs.
- **Depends on** and **Used by** link directly to related workflow files.

Pending stage edits are saved before changing, creating, renaming, deleting,
or refreshing workflows. If saving fails, edits remain visible with an error.
Workflow selection and mutation are disabled during execution.

## Canvas and editing

Stages have readable names, two-line command previews, input/output counts,
and run status. The layout places dependencies above their consumers, separates
parallel branches, and centers merge rows. **Arrange** restores that layout;
**Fit workflow to view** frames the graph without enlarging small pipelines.
**Find stage** zooms to readable detail and opens its inspector. Newly added
nodes avoid existing manual placements. Drag nodes to arrange them manually.
Positions are saved per repository and
workflow; the larger node geometry starts a new layout namespace.

Select a stage to edit its command, dependencies, outputs, parameters, and
execution settings in the inspector. **+ Stage** adds a stage. Drag between
ports to add an explicit stage-output dependency within the same workflow.
The source must already declare an output; a connection does not create a file
or change its command. Use **Edit YAML** for advanced policies or template axes.
Commands must produce declared output files, or provide stdout for the engine's
stdout capture contract.

Changes autosave after a short delay; **Save** and `Cmd/Ctrl+S` save immediately.
Undo/redo history belongs to the current workflow and resets when switching
files. Larger graphs also show a minimap.

## Dependencies between workflow files

Crab connects **stages through their output paths**, rather than declaring that
one YAML file depends on another YAML file. For example:

```yaml
# prepare.workflow.yaml
stages:
  prepare:
    cmd: python prepare.py
    outs:
      - features.csv
```

```yaml
# train.workflow.yaml
stages:
  fit:
    cmd: python train.py
    deps:
      - features.csv
    outs:
      - model.bin
```

Running `train.workflow.yaml` includes the upstream `prepare` stage. The desktop
runner uses recursive discovery and file-qualified targets so unrelated workflows
are not selected merely because their stage names match.

In a stage's **Dependencies** section, **Use a workflow output** lists outputs
from other workflows that can be addressed from this workflow's directory.
Choosing one adds a normal path dependency. The sidebar shows the relationship
immediately; removing the dependency removes that relationship.

Without `wdir`, paths in nested workflow files are relative to the containing
directory. An explicit `wdir` is relative to the repository root; dependencies
and outputs resolve within it. Use `wdir: .` to exchange repository-relative
artifacts from workflows in different folders. An explicit `stage_out.out`
includes the producer's working directory, independently of the consumer's.
The editor follows the engine's exact file and longest matching directory-output
rules. It does not collapse `..` paths or invent cross-directory dependencies.
Without an explicit `wdir`, keep shared named workflows in the same folder when
they exchange outputs.
`stage_out` is for explicit references within a workflow; there is no general
`depends_on: other.yml` file-level syntax.

See [split workflows and lockfiles](https://crab.build/docs/cli/workflow/split-workflows-lockfiles)
for namespaces, recursive discovery, and command-line usage.

## Complex workflows

Foreach and matrix definitions appear as source template nodes with execution
counts. The engine expands their dependencies and output paths; the canvas
collapses those edges back onto the editable templates. Template status combines
all executions in the current run. Named `workflows:` groups retain their stage
membership when edited or saved.

Use the inspector to change a template's shared command and stage settings.
Use **Edit YAML** to change iteration items, matrix axes, variables, retry
policies, resources, metrics, plots, hooks, or output metadata. The editor opens
the working-tree definition. Refresh workflows after changing its source.
Visual saves preserve these fields, validate with the runtime parser, and replace
the file atomically. A rejected edit leaves the previous definition intact.

```yaml
defaults:
  retry:
    max_attempts: 3
    on_exit_codes: [1]
stages:
  prepare:
    foreach: [train, test]
    do:
      cmd: python prepare.py --split ${item}
      outs: ["data/${item}.csv"]
  fit:
    matrix:
      model: [linear, forest]
    cmd: python train.py --model ${item.model}
    deps: [data/train.csv, data/test.csv]
    outs: ["models/${item.model}.bin"]
```

The commands must write their declared files. Retries only apply to configured
exit codes, signals, or timeouts; `max_attempts` includes the initial attempt.
Connecting a template's individual outputs requires their expanded artifact
paths in YAML rather than a stage-output reference to the template name.

## Running and inspecting

**Run All** runs the selected workflow with its upstream stages. A selected stage
can also be run from the inspector or context menu. Template runs select all
expanded executions belonging to that template. **Cancel** requests shutdown;
**Stopping…** remains visible until the native process owner confirms cleanup.
The next run can then acquire the execution lock. Conditional and frozen skips
are counted separately from completed stages.
The shared **Output** dock shows command output, stage progress (including
upstream stages in other files), and errors below Editor, Status, and Experiments.
Use **Output** in the top bar to show it, the close button to dismiss it, and
its upper divider to resize it. Both dividers support arrow-key resizing.
The panel remembers its height; dismissing it keeps the current run logs.
A new run opens it automatically, but streaming output does not reopen a dock
you dismissed during that run. Scrolling up keeps earlier logs in view; scroll
back to the bottom to follow new output. Switching files clears the previous
run display; unrelated repository output stays separate.

**Run All** and stage runs execute locally and use local stage cache. Merely
having a Crab/S3 remote configured does not connect to it. Workflows that
explicitly download remote inputs still need those providers. Use the CLI for
remote cache replay (`crab run --cache-only`) or publication
(`crab run --cache-push`); failures of those requested remote operations remain
visible.

The **Status** tab checks stage freshness; **Experiments** manages parameterized
runs. **Import DVC workflow** in the sidebar imports DVC pipeline definitions.
