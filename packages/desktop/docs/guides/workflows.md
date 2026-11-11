# Data Workflows

Open **Data Workflows** in the activity bar to manage repository pipelines.
Crab discovers `crab.yaml` and `*.workflow.yaml` files recursively, excluding
hidden and build directories. A file named `pipeline.yaml` or `a.yml` is not
automatically discovered; use `pipeline.workflow.yaml` or `a.workflow.yaml`.

## Workflow sidebar

The left pane lists workflow paths and stage counts. Filter the list to find a
workflow, select it to edit, or refresh to pick up files changed outside Crab.
An unreadable file stays in the list with an error indicator; other workflows
remain accessible.

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
Drag nodes to arrange them manually. Positions are saved per repository and
workflow; the larger node geometry starts a new layout namespace.

Select a stage to edit its command, dependencies, outputs, parameters, and
execution settings in the inspector. **+ Stage** adds a stage. Drag between
ports to add an explicit stage-output dependency within the same workflow.
Commands must produce declared output files, or provide stdout for the engine's
stdout capture contract.

Changes autosave after a short delay; **Save** and `Cmd/Ctrl+S` save immediately.
Undo/redo history belongs to the current workflow and resets when switching
files. Output stays collapsed until a run starts, leaving more space for the
canvas. Larger graphs also show a minimap.

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

Paths in nested workflow files are relative to the containing directory. The
editor follows the engine's exact file and longest matching directory-output
rules. It does not collapse `..` paths or invent cross-directory dependencies.
Keep shared named workflows in the same folder when they exchange outputs.
`stage_out` is for explicit references within a workflow; there is no general
`depends_on: other.yml` file-level syntax.

See [split workflows and lockfiles](https://crab.build/docs/cli/workflow/split-workflows-lockfiles)
for namespaces, recursive discovery, and command-line usage.

## Running and inspecting

**Run All** runs the selected workflow with its upstream stages. A selected stage
can also be run from the inspector or context menu. **Cancel** stops the run.
The output pane opens automatically and shows command output, stage progress
(including upstream stages in other files), and errors. Switching files clears
the previous run display; unrelated repository output stays separate.

The **Status** tab checks stage freshness; **Experiments** manages parameterized
runs. **Import DVC workflow** in the sidebar imports DVC pipeline definitions.
