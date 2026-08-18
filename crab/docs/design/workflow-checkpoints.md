# Workflow checkpoint contract

Crab treats a checkpoint as an acknowledged experiment lineage point, not as
an alias for persist. A persisted output survives pre-execution cleanup; a
checkpoint additionally identifies an immutable output snapshot with a parent,
sequence, metrics, and resume state.

checkpoint: true is valid only on cached local file or directory outputs. The
field is part of the stage hash (schema crab.stage.v3). crab run and crab
repro fail closed with workflow_checkpoint_requires_exp_run; ordinary
execution never turns the field into persist: true. crab exp run is the owner
of checkpoint lineage.

The stage-facing control entry point is the hidden crab workflow checkpoint
command. An experiment supervisor supplies a private control directory, run
and stage identities, and a one-shot token through inherited environment
variables. The command writes an atomically renamed, canonical JSON request
and waits for a keyed-Blake3 authenticated acknowledgement. Identity and
tokens are not accepted as command arguments, are excluded from hashes and
logs, and control files are removed after acknowledgement or timeout.

The versioned CheckpointRecord contains the experiment and stage, sequence,
parent, stage hash, output identities, metrics, terminal flag, and resumable
flag. A lineage can be selected by checkpoint ID or sequence; an ambiguous
selector is an error. Parent links and monotonic sequence numbers are checked
before an acknowledgement is accepted. GC protects every record reachable
from a retained experiment reference.

`crab exp run --resume <experiment>` creates a new experiment worktree but
first copies and validates the source checkpoint objects and lineage. The
latest resumable point is then applied to the new worktree. New records append
to that forked lineage, retaining the source lineage identity so parent links
and immutable objects remain continuous across the resume boundary.

The current implementation includes the validated field, hash participation,
record contract, lineage checks, authenticated stage control boundary, local
supervision, selectors, reset/resume handling, and checkpoint object
validation during push/pull. A release claim still requires retained
clean-clone E2E evidence for remote publication, `exp show/apply/reset`,
resume, metrics, and GC reachability; without that evidence the operations
remain implementation-level rather than release-qualified.
