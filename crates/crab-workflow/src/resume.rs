//! Crash-recovery decision logic.
//!
//! Given a journal row plus the current on-disk state, `decide` maps
//! to a [`ResumeAction`] telling the executor how to continue. The
//! decision table mirrors design §"Crash recovery algorithm" —
//! filesystem-drift detection biases conservative: when the journal
//! says a state is reached but the filesystem disagrees, we restart
//! rather than risk silent corruption.
//!
//! Multi-stage resume ([`walk_dag`]) lifts the per-row decision into
//! a DAG-wide plan: for each stage in topological order, either skip
//! (cached success), execute from scratch (no durable row, or a
//! dependency changed), or resume from the last safe state. A
//! cascading restart rule forces downstream stages to execute when
//! an upstream stage's outputs are about to change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use uuid::Uuid;

use crate::{Graph, StageState};

use crate::journal::Journal;
use crate::materialize::SIDECAR_PREFIX;
use crate::stage::StageName;
use crate::{Result, WorkflowError as CrabError};

/// What to do with a stage row recovered from the journal after a
/// crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// The row records no durable work — drop it and start fresh.
    Discard,
    /// Clean partial sidecars and restart from `Resolved`.
    RestartFromResolved,
    /// Skip execution; hash the existing outs.
    ResumeFromProduced,
    /// Skip execute + hash; verify xorbs and publish the entry.
    ResumeFromStaged,
    /// Entry is already written; publish the ref and update the lockfile.
    ResumeFromEntryWritten,
    /// Ref is already published; just update the lockfile.
    ResumeFromRefPublished,
    /// Already terminal — skip the stage unless `--force` re-runs it.
    AlreadyTerminal,
}

/// Filesystem snapshot feeding the resume decision.
///
/// Kept small: the resume path runs per stage_runs row and too many
/// stat() calls balloon cost. `outs_match_journal` is the hashed
/// truth; callers compute it once from the journal payload plus the
/// on-disk files.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsState {
    /// True iff every declared output exists on disk with the hash
    /// the journal recorded.
    pub outs_match_journal: bool,
    /// True iff the stage's recorded child pid is still alive.
    pub child_pid_alive: bool,
    /// True iff the staged xorbs the journal references are still
    /// present in the local cache. Only meaningful at `Staged` or
    /// beyond.
    pub staged_xorbs_present: bool,
}

/// CLI overrides that bias the resume decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct CliFlags {
    /// `--resume-trust-outputs`: trust on-disk files at `Running`
    /// crash even without a journal-recorded hash.
    pub resume_trust_outputs: bool,
    /// `--force`: re-run even terminal-success stages.
    pub force: bool,
}

/// Decide how to recover the given stage state with the observed
/// filesystem and CLI flags.
pub fn decide(state: StageState, fs: FsState, cli: CliFlags) -> ResumeAction {
    match state {
        // Resolving is pre-durable — no work to recover.
        StageState::Resolving => ResumeAction::Discard,

        // Resolved/CacheChecked durably recorded the input hash but
        // no output work has happened yet.
        StageState::Resolved | StageState::CacheChecked => ResumeAction::RestartFromResolved,

        // Running — inspect the child. Dead pid → crash mid-exec;
        // trust on-disk outs only when the user opts in.
        StageState::Running => {
            if fs.child_pid_alive {
                // Not strictly restart; the supervisor can re-attach.
                // Conservatively report RestartFromResolved here and
                // let the orchestrator treat an alive pid specially.
                ResumeAction::RestartFromResolved
            } else if cli.resume_trust_outputs && fs.outs_match_journal {
                ResumeAction::ResumeFromProduced
            } else {
                ResumeAction::RestartFromResolved
            }
        }

        // Produced / Hashed — outs should exist and match the
        // recorded hashes. Any drift is treated as corruption and we
        // restart.
        StageState::Produced | StageState::Hashed => {
            if fs.outs_match_journal {
                if matches!(state, StageState::Produced) {
                    ResumeAction::ResumeFromProduced
                } else {
                    ResumeAction::ResumeFromStaged
                }
            } else {
                ResumeAction::RestartFromResolved
            }
        }

        // Staged — staging segments must still be present.
        StageState::Staged => {
            if fs.staged_xorbs_present {
                ResumeAction::ResumeFromEntryWritten
            } else {
                ResumeAction::RestartFromResolved
            }
        }

        // EntryWritten is the commit point; after it we never restart.
        StageState::EntryWritten => ResumeAction::ResumeFromRefPublished,
        StageState::RefPublished => ResumeAction::ResumeFromRefPublished,
        // The lockfile transition precedes the final commit. A crash here
        // can still have pending cache-hit materialization, so restart through
        // the normal cache probe instead of treating the row as terminal.
        StageState::LockfileUpdated => ResumeAction::RestartFromResolved,

        // Terminal states.
        StageState::Committed | StageState::Failed | StageState::Aborted => {
            if cli.force {
                ResumeAction::RestartFromResolved
            } else {
                ResumeAction::AlreadyTerminal
            }
        }
    }
}

/// Per-stage action emitted by [`walk_dag`] for a whole DAG.
///
/// Distinct from [`ResumeAction`]: the single-stage decision only
/// sees one journal row and can't see that an upstream stage is
/// about to re-execute, which forces this one to re-execute too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAction {
    /// Run the stage from its beginning — either because no
    /// durable journal row exists, because a dependency is about
    /// to change, or because a previous attempt terminated in
    /// `Failed` / `Aborted`.
    Execute,
    /// Skip the stage. `cached` is true when the skip is backed
    /// by a terminal `Committed` row (the cache hit path); false
    /// is reserved for future variants such as `persist: true`
    /// pass-throughs.
    Skip { cached: bool },
    /// Resume mid-lifecycle from the last safe state recorded in
    /// the journal, using the inner [`ResumeAction`] for the
    /// per-state dispatch.
    Resume(ResumeAction),
}

/// DAG-wide resume plan, ordered in toposort so callers can drive
/// it linearly.
#[derive(Debug, Clone)]
pub struct DagResumeReport {
    /// Per-stage action. Keys match the DAG's stage set exactly.
    pub actions: BTreeMap<StageName, StageAction>,
    /// Toposorted stage order, carried alongside `actions` so
    /// callers don't re-toposort. BTreeMap iteration would give
    /// lexicographic order, which is wrong for scheduling.
    pub order: Vec<StageName>,
}

impl DagResumeReport {
    /// Borrow the action for `stage`, if any.
    pub fn action_for(&self, stage: &StageName) -> Option<StageAction> {
        self.actions.get(stage).copied()
    }
}

/// Plan recovery across an entire workflow DAG.
///
/// Walks `graph` in topological order. For each stage:
///
/// - No journal row → [`StageAction::Execute`].
/// - Row in terminal `Committed` → [`StageAction::Skip`] (cache hit).
/// - Row in terminal `Failed` / `Aborted` → [`StageAction::Execute`]
///   (retry from scratch), unless `cli.force` is set in which case
///   it still re-executes (force is an explicit "rerun even on
///   success" and does not narrow the failure path).
/// - Non-terminal row → call [`decide`] with the filesystem state
///   returned by `fs_state_getter` and wrap in
///   [`StageAction::Resume`]. If `decide` returns `AlreadyTerminal`
///   — which happens for force-on-committed — treat it as
///   `Execute`.
///
/// Then apply the cascading-restart rule: any consumer of a stage
/// whose action is `Execute` or `Resume(RestartFromResolved)` has
/// its own plan downgraded to `Execute`, because the upstream
/// stage is about to produce new outputs (or the same outputs
/// from fresh computation) that invalidate anything cached
/// downstream. Cascade propagates transitively — a descendant of
/// a cascading ancestor cascades in turn.
pub fn walk_dag(
    graph: &Graph,
    journal: &Journal,
    run_id: Uuid,
    cli: CliFlags,
    fs_state_getter: &dyn Fn(&StageName) -> FsState,
) -> Result<DagResumeReport> {
    let order = graph.toposort();

    // First pass: per-stage decision from its own journal row +
    // filesystem snapshot, ignoring upstream effects.
    let mut actions: BTreeMap<StageName, StageAction> = BTreeMap::new();
    for stage in &order {
        let row = journal.latest_stage_row(run_id, stage.as_str())?;
        let action = match row {
            None => StageAction::Execute,
            Some(r) if r.state == StageState::Committed => {
                if cli.force {
                    StageAction::Execute
                } else {
                    StageAction::Skip { cached: true }
                }
            }
            Some(r) if matches!(r.state, StageState::Failed | StageState::Aborted) => {
                // `force` and default both retry from scratch on a
                // failed attempt — the knob is for overriding
                // success, not for altering the failure path.
                StageAction::Execute
            }
            Some(r) => {
                let fs = fs_state_getter(stage);
                match decide(r.state, fs, cli) {
                    // `AlreadyTerminal` on a non-terminal state
                    // means `decide` got an inconsistent view;
                    // `Discard` means "no durable work". Either
                    // way the safest response is to re-run from
                    // the top rather than surface the raw variant
                    // to a DAG scheduler.
                    ResumeAction::AlreadyTerminal | ResumeAction::Discard => StageAction::Execute,
                    other => StageAction::Resume(other),
                }
            }
        };
        actions.insert(stage.clone(), action);
    }

    // Second pass: cascade. A stage that will `Execute` or
    // `Resume(RestartFromResolved)` will (re)produce its outs;
    // anything downstream that cached against the old outs must
    // also re-execute. Walk in topo order: when we upgrade a
    // stage to Execute, its downstream decisions (still ahead in
    // the order) see the upgrade and propagate further.
    let cascade_stages: BTreeSet<StageName> = order
        .iter()
        .filter(|s| {
            matches!(
                actions.get(*s),
                Some(StageAction::Execute | StageAction::Resume(ResumeAction::RestartFromResolved))
            )
        })
        .cloned()
        .collect();

    if !cascade_stages.is_empty() {
        // Transitively close over consumers. BFS from each
        // cascade source would revisit nodes; toposort-ordered
        // scan guarantees we see each node's ancestors first so
        // one linear sweep is sufficient.
        let mut forced: BTreeSet<StageName> = cascade_stages;
        for stage in &order {
            // If any producer of `stage` is forced, so is `stage`.
            let has_forced_producer = graph
                .producers_of(stage)
                .into_iter()
                .any(|p| forced.contains(&p));
            if has_forced_producer {
                forced.insert(stage.clone());
                actions.insert(stage.clone(), StageAction::Execute);
            }
        }
    }

    Ok(DagResumeReport { actions, order })
}

///
/// Walks `workdir` recursively for files whose names contain
/// [`SIDECAR_PREFIX`] followed by a UUID that is not in
/// `active_run_ids`. Returns the number of sidecars removed.
pub fn sweep_orphan_sidecars(workdir: &Path, active_run_ids: &[Uuid]) -> Result<usize> {
    let mut removed = 0_usize;
    sweep_inner(workdir, active_run_ids, &mut removed)?;
    Ok(removed)
}

fn sweep_inner(dir: &Path, active: &[Uuid], removed: &mut usize) -> Result<()> {
    let iter = match std::fs::read_dir(dir) {
        Ok(i) => i,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CrabError::Io(e)),
    };

    for entry in iter {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;

        let name_is_sidecar = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(SIDECAR_PREFIX))
            .unwrap_or(false);

        if name_is_sidecar {
            if is_active(&path, active) {
                continue;
            }
            if file_type.is_dir() {
                std::fs::remove_dir_all(&path).map_err(CrabError::Io)?;
            } else {
                std::fs::remove_file(&path).map_err(CrabError::Io)?;
            }
            *removed += 1;
        } else if file_type.is_dir() {
            sweep_inner(&path, active, removed)?;
        }
    }
    Ok(())
}

fn is_active(sidecar: &Path, active: &[Uuid]) -> bool {
    let name = match sidecar.file_name().and_then(|n| n.to_str()) {
        Some(s) => s,
        None => return false,
    };
    let Some(pos) = name.rfind(SIDECAR_PREFIX) else {
        return false;
    };
    let suffix = &name[pos + SIDECAR_PREFIX.len()..];
    match Uuid::parse_str(suffix) {
        Ok(id) => active.contains(&id),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolving_is_discarded() {
        assert_eq!(
            decide(
                StageState::Resolving,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::Discard
        );
    }

    #[test]
    fn resolved_restarts() {
        assert_eq!(
            decide(
                StageState::Resolved,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::RestartFromResolved
        );
        assert_eq!(
            decide(
                StageState::CacheChecked,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::RestartFromResolved
        );
    }

    #[test]
    fn running_with_alive_pid_restarts_by_default() {
        let fs = FsState {
            child_pid_alive: true,
            ..FsState::default()
        };
        assert_eq!(
            decide(StageState::Running, fs, CliFlags::default()),
            ResumeAction::RestartFromResolved
        );
    }

    #[test]
    fn running_with_dead_pid_and_trust_flag_resumes() {
        let fs = FsState {
            child_pid_alive: false,
            outs_match_journal: true,
            ..FsState::default()
        };
        let cli = CliFlags {
            resume_trust_outputs: true,
            ..CliFlags::default()
        };
        assert_eq!(
            decide(StageState::Running, fs, cli),
            ResumeAction::ResumeFromProduced
        );
    }

    #[test]
    fn produced_matching_fs_resumes() {
        let fs = FsState {
            outs_match_journal: true,
            ..FsState::default()
        };
        assert_eq!(
            decide(StageState::Produced, fs, CliFlags::default()),
            ResumeAction::ResumeFromProduced
        );
    }

    #[test]
    fn produced_drifted_restarts() {
        let fs = FsState {
            outs_match_journal: false,
            ..FsState::default()
        };
        assert_eq!(
            decide(StageState::Produced, fs, CliFlags::default()),
            ResumeAction::RestartFromResolved
        );
    }

    #[test]
    fn staged_with_xorbs_resumes_to_entry_written() {
        let fs = FsState {
            staged_xorbs_present: true,
            ..FsState::default()
        };
        assert_eq!(
            decide(StageState::Staged, fs, CliFlags::default()),
            ResumeAction::ResumeFromEntryWritten
        );
    }

    #[test]
    fn staged_without_xorbs_restarts() {
        let fs = FsState {
            staged_xorbs_present: false,
            ..FsState::default()
        };
        assert_eq!(
            decide(StageState::Staged, fs, CliFlags::default()),
            ResumeAction::RestartFromResolved
        );
    }

    #[test]
    fn entry_written_and_ref_published_resume_publication_steps() {
        assert_eq!(
            decide(
                StageState::EntryWritten,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::ResumeFromRefPublished
        );
        assert_eq!(
            decide(
                StageState::RefPublished,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::ResumeFromRefPublished
        );
    }

    #[test]
    fn lockfile_updated_restarts_before_materialized_commit() {
        assert_eq!(
            decide(
                StageState::LockfileUpdated,
                FsState::default(),
                CliFlags::default()
            ),
            ResumeAction::RestartFromResolved
        );
    }

    #[test]
    fn terminal_states_are_skipped_without_force() {
        for state in [
            StageState::Committed,
            StageState::Failed,
            StageState::Aborted,
        ] {
            assert_eq!(
                decide(state, FsState::default(), CliFlags::default()),
                ResumeAction::AlreadyTerminal
            );
        }
    }

    #[test]
    fn force_restarts_terminal_states() {
        let cli = CliFlags {
            force: true,
            ..CliFlags::default()
        };
        for state in [
            StageState::Committed,
            StageState::Failed,
            StageState::Aborted,
        ] {
            assert_eq!(
                decide(state, FsState::default(), cli),
                ResumeAction::RestartFromResolved
            );
        }
    }

    #[test]
    fn sweep_removes_orphan_sidecars() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path();

        let active = Uuid::now_v7();
        let orphan = Uuid::now_v7();

        let active_path = workdir.join(format!("out.txt.crab.tmp.{active}"));
        let orphan_path = workdir.join(format!("out.txt.crab.tmp.{orphan}"));
        fs::write(&active_path, b"a").unwrap();
        fs::write(&orphan_path, b"o").unwrap();

        let removed = sweep_orphan_sidecars(workdir, &[active]).unwrap();
        assert_eq!(removed, 1);
        assert!(active_path.exists(), "active sidecar should survive");
        assert!(!orphan_path.exists(), "orphan sidecar should be removed");
    }

    #[test]
    fn sweep_recurses_into_subdirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("nested");
        fs::create_dir_all(&sub).unwrap();
        let orphan = Uuid::now_v7();
        let path = sub.join(format!("x.crab.tmp.{orphan}"));
        fs::write(&path, b"o").unwrap();

        let removed = sweep_orphan_sidecars(tmp.path(), &[]).unwrap();
        assert_eq!(removed, 1);
        assert!(!path.exists());
    }

    #[test]
    fn sweep_ignores_non_sidecar_files() {
        let tmp = TempDir::new().unwrap();
        let normal = tmp.path().join("regular.txt");
        fs::write(&normal, b"x").unwrap();
        let removed = sweep_orphan_sidecars(tmp.path(), &[]).unwrap();
        assert_eq!(removed, 0);
        assert!(normal.exists());
    }

    #[test]
    fn sweep_handles_missing_workdir() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let removed = sweep_orphan_sidecars(&missing, &[]).unwrap();
        assert_eq!(removed, 0);
    }

    // --- walk_dag: multi-stage resume ---

    use crate::journal::Journal;
    use crate::stage::{Cmd, Dep, EnvSpec, Out, OutKind, Resources, Stage, StageName};
    use crate::{Defaults, Workflow};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn mk_stage(name: &str, deps: Vec<Dep>, outs: Vec<Out>) -> (StageName, Stage) {
        let n = StageName::parse(name).unwrap();
        let s = Stage {
            name: n.clone(),
            cmd: Cmd::Shell("true".into()),
            deps,
            outs,
            env: EnvSpec::Inherit,
            retry: None,
            timeout: None,
            wdir: None,
            persist: false,
            nondeterministic: false,
            hermetic: false,
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            side_effects: false,
            on_cache_hit: None,
            resources: Resources::default(),
            frozen: false,
            desc: None,
            meta: None,
            condition: None,
        };
        (n, s)
    }

    fn linear_abc() -> Workflow {
        let mut map = BTreeMap::new();
        for (n, s) in [
            mk_stage(
                "a",
                Vec::new(),
                vec![Out::new(PathBuf::from("a.out"), OutKind::File)],
            ),
            mk_stage(
                "b",
                vec![Dep::Path(PathBuf::from("a.out"))],
                vec![Out::new(PathBuf::from("b.out"), OutKind::File)],
            ),
            mk_stage(
                "c",
                vec![Dep::Path(PathBuf::from("b.out"))],
                vec![Out::new(PathBuf::from("c.out"), OutKind::File)],
            ),
        ] {
            map.insert(n, s);
        }
        Workflow {
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            plot_configs: Vec::new(),
            artifacts: crate::ArtifactMetadata::default(),
            defaults: Defaults::default(),
            stages: map,
            workflow_membership: BTreeMap::new(),
        }
    }

    fn open_journal_tmp() -> (TempDir, Journal) {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(&tmp.path().join("journal.db")).unwrap();
        (tmp, j)
    }

    /// Drive a stage row through the legal transition chain to
    /// `target`, so a test can seed "stage X is at state Y".
    fn drive_to(j: &Journal, run: Uuid, stage: &str, target: StageState) {
        let chain = [
            StageState::Resolved,
            StageState::CacheChecked,
            StageState::Running,
            StageState::Produced,
            StageState::Hashed,
            StageState::Staged,
            StageState::EntryWritten,
            StageState::RefPublished,
            StageState::LockfileUpdated,
            StageState::Committed,
        ];
        j.insert_stage_start(run, stage).unwrap();
        for step in chain {
            j.transition(run, stage, 1, step, "{}").unwrap();
            if step == target {
                return;
            }
        }
    }

    #[test]
    fn walk_dag_all_uncommitted_yields_execute() {
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();

        let no_fs = |_: &StageName| FsState::default();
        let report = walk_dag(&graph, &j, run, CliFlags::default(), &no_fs).unwrap();

        for name in ["a", "b", "c"] {
            let s = StageName::parse(name).unwrap();
            assert_eq!(report.action_for(&s), Some(StageAction::Execute));
        }
        // Order is the graph's toposort.
        assert_eq!(
            report.order.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn walk_dag_all_committed_yields_skip() {
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();
        for name in ["a", "b", "c"] {
            drive_to(&j, run, name, StageState::Committed);
        }

        let no_fs = |_: &StageName| FsState::default();
        let report = walk_dag(&graph, &j, run, CliFlags::default(), &no_fs).unwrap();

        for name in ["a", "b", "c"] {
            let s = StageName::parse(name).unwrap();
            assert_eq!(
                report.action_for(&s),
                Some(StageAction::Skip { cached: true })
            );
        }
    }

    #[test]
    fn walk_dag_cascades_restart_from_mid_stage_crash() {
        // Seeds the SIGKILL-mid-stage-2 scenario: A committed, B
        // at Running with a dead pid (no on-disk outs), C never
        // ran. Expected: A skip, B restart-from-resolved, C
        // execute. The cascade rule forces C even though C has no
        // journal row.
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();
        drive_to(&j, run, "a", StageState::Committed);
        drive_to(&j, run, "b", StageState::Running);
        // C: no row at all.

        let fs_getter = |stage: &StageName| match stage.as_str() {
            // B's pid is dead and no on-disk outs survived the kill.
            "b" => FsState {
                child_pid_alive: false,
                outs_match_journal: false,
                staged_xorbs_present: false,
            },
            _ => FsState::default(),
        };
        let report = walk_dag(&graph, &j, run, CliFlags::default(), &fs_getter).unwrap();

        let a = StageName::parse("a").unwrap();
        let b = StageName::parse("b").unwrap();
        let c = StageName::parse("c").unwrap();
        assert_eq!(
            report.action_for(&a),
            Some(StageAction::Skip { cached: true })
        );
        assert_eq!(
            report.action_for(&b),
            Some(StageAction::Resume(ResumeAction::RestartFromResolved))
        );
        assert_eq!(report.action_for(&c), Some(StageAction::Execute));
    }

    #[test]
    fn walk_dag_resumes_from_staged_without_cascading_to_consumer() {
        // When B sits at Hashed with matching on-disk outs, the
        // single-stage decider picks ResumeFromStaged. That is a
        // "mid-publication" resume, not a recompute — C hasn't
        // observed B's outs yet, but they're fixed, so cascading
        // to C is wrong. The cascade rule excludes non-Restart
        // resumes on purpose.
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();
        drive_to(&j, run, "a", StageState::Committed);
        drive_to(&j, run, "b", StageState::Hashed);

        let fs_getter = |stage: &StageName| match stage.as_str() {
            "b" => FsState {
                outs_match_journal: true,
                ..FsState::default()
            },
            _ => FsState::default(),
        };
        let report = walk_dag(&graph, &j, run, CliFlags::default(), &fs_getter).unwrap();

        let a = StageName::parse("a").unwrap();
        let b = StageName::parse("b").unwrap();
        let c = StageName::parse("c").unwrap();
        assert_eq!(
            report.action_for(&a),
            Some(StageAction::Skip { cached: true })
        );
        assert_eq!(
            report.action_for(&b),
            Some(StageAction::Resume(ResumeAction::ResumeFromStaged))
        );
        assert_eq!(report.action_for(&c), Some(StageAction::Execute));
    }

    #[test]
    fn walk_dag_terminal_failed_retries_and_cascades() {
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();
        drive_to(&j, run, "a", StageState::Committed);
        // B reached Staged then transitioned to Failed.
        drive_to(&j, run, "b", StageState::Staged);
        j.transition(run, "b", 1, StageState::Failed, "{}").unwrap();

        let report = walk_dag(&graph, &j, run, CliFlags::default(), &|_: &StageName| {
            FsState::default()
        })
        .unwrap();

        let b = StageName::parse("b").unwrap();
        let c = StageName::parse("c").unwrap();
        assert_eq!(report.action_for(&b), Some(StageAction::Execute));
        assert_eq!(report.action_for(&c), Some(StageAction::Execute));
    }

    #[test]
    fn walk_dag_force_reruns_committed_and_cascades() {
        let wf = linear_abc();
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, j) = open_journal_tmp();
        let run = Uuid::now_v7();
        j.insert_run_start(run, "test", "host").unwrap();
        for name in ["a", "b", "c"] {
            drive_to(&j, run, name, StageState::Committed);
        }
        let cli = CliFlags {
            force: true,
            ..CliFlags::default()
        };
        let report = walk_dag(&graph, &j, run, cli, &|_: &StageName| FsState::default()).unwrap();
        for name in ["a", "b", "c"] {
            let s = StageName::parse(name).unwrap();
            assert_eq!(report.action_for(&s), Some(StageAction::Execute));
        }
    }

    #[test]
    fn walk_dag_diamond_cascades_from_one_branch() {
        // a → b, a → c, b → d, c → d.
        // a committed, b Running (dead pid), c committed.
        // Expected: a skip, b restart, c skip, d execute (because
        // its producer b is about to re-execute).
        let mut map = BTreeMap::new();
        for (n, s) in [
            mk_stage(
                "a",
                Vec::new(),
                vec![Out::new(PathBuf::from("a.out"), OutKind::File)],
            ),
            mk_stage(
                "b",
                vec![Dep::Path(PathBuf::from("a.out"))],
                vec![Out::new(PathBuf::from("b.out"), OutKind::File)],
            ),
            mk_stage(
                "c",
                vec![Dep::Path(PathBuf::from("a.out"))],
                vec![Out::new(PathBuf::from("c.out"), OutKind::File)],
            ),
            mk_stage(
                "d",
                vec![
                    Dep::Path(PathBuf::from("b.out")),
                    Dep::Path(PathBuf::from("c.out")),
                ],
                vec![Out::new(PathBuf::from("d.out"), OutKind::File)],
            ),
        ] {
            map.insert(n, s);
        }
        let wf = Workflow {
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            plot_configs: Vec::new(),
            artifacts: crate::ArtifactMetadata::default(),
            defaults: Defaults::default(),
            stages: map,
            workflow_membership: BTreeMap::new(),
        };
        let graph = Graph::build(&wf.stages).unwrap();
        let (_tmp, journal) = open_journal_tmp();
        let run = Uuid::now_v7();
        journal.insert_run_start(run, "test", "host").unwrap();
        drive_to(&journal, run, "a", StageState::Committed);
        drive_to(&journal, run, "b", StageState::Running);
        drive_to(&journal, run, "c", StageState::Committed);
        drive_to(&journal, run, "d", StageState::Committed);

        let fs_getter = |stage: &StageName| match stage.as_str() {
            "b" => FsState {
                child_pid_alive: false,
                outs_match_journal: false,
                staged_xorbs_present: false,
            },
            _ => FsState::default(),
        };
        let report = walk_dag(&graph, &journal, run, CliFlags::default(), &fs_getter).unwrap();

        let producer_a = StageName::parse("a").unwrap();
        let crashed_b = StageName::parse("b").unwrap();
        let sibling_c = StageName::parse("c").unwrap();
        let sink_d = StageName::parse("d").unwrap();
        assert_eq!(
            report.action_for(&producer_a),
            Some(StageAction::Skip { cached: true })
        );
        assert_eq!(
            report.action_for(&crashed_b),
            Some(StageAction::Resume(ResumeAction::RestartFromResolved))
        );
        // C was untouched and its producer (A) is skipping — no cascade here.
        assert_eq!(
            report.action_for(&sibling_c),
            Some(StageAction::Skip { cached: true })
        );
        // D depends on B, which is about to recompute.
        assert_eq!(report.action_for(&sink_d), Some(StageAction::Execute));
    }
}
