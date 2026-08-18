//! Integration tests for [`crab::workflow::resume::walk_dag`].
//!
//! These tests exercise the multi-stage resume planner against a
//! real on-disk SQLite journal so the public API stays ergonomic
//! from an out-of-crate caller's perspective. The classic scenario
//! — SIGKILL mid-stage-2 of a 3-stage linear DAG — is the first
//! assertion: resume must run only stage 2 from the last safe
//! state plus stage 3 fresh, never touching stage 1.
//!
//! The SIGKILL is simulated at the journal level rather than by
//! fork+kill: a journal where B is recorded at `Running`, its pid
//! gone, and its out files absent is indistinguishable from the
//! post-SIGKILL state, and far cheaper to seed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use uuid::Uuid;

use crab::workflow::journal::Journal;
use crab::workflow::resume::{
    CliFlags, DagResumeReport, FsState, ResumeAction, StageAction, walk_dag,
};
use crab::workflow::stage::{Cmd, Dep, EnvSpec, Out, OutKind, Resources, Stage, StageName};
use crab_workflow::{Defaults, Graph, StageState, Workflow};

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

fn linear_three_stage() -> Workflow {
    let mut stages = BTreeMap::new();
    for (n, s) in [
        mk_stage(
            "fetch",
            Vec::new(),
            vec![Out::new(PathBuf::from("data.csv"), OutKind::File)],
        ),
        mk_stage(
            "train",
            vec![Dep::Path(PathBuf::from("data.csv"))],
            vec![Out::new(PathBuf::from("model.bin"), OutKind::File)],
        ),
        mk_stage(
            "evaluate",
            vec![Dep::Path(PathBuf::from("model.bin"))],
            vec![Out::new(PathBuf::from("metrics.json"), OutKind::File)],
        ),
    ] {
        stages.insert(n, s);
    }
    Workflow {
        params: Vec::new(),
        metrics: Vec::new(),
        plots: Vec::new(),
        plot_configs: Vec::new(),
        artifacts: crab_workflow::ArtifactMetadata::default(),
        defaults: Defaults::default(),
        stages,
        workflow_membership: BTreeMap::new(),
    }
}

/// Walk a stage row through legal transitions up to `target`.
/// Terminal states cannot be re-entered so the chain stops at the
/// requested state.
fn seed_to(j: &Journal, run: Uuid, stage: &str, target: StageState) {
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

fn action_of(report: &DagResumeReport, name: &str) -> StageAction {
    let s = StageName::parse(name).unwrap();
    report
        .action_for(&s)
        .unwrap_or_else(|| panic!("no action for stage {name}"))
}

/// fetch → train → evaluate; SIGKILL hit mid-`train`. Resume must
/// skip `fetch`, restart `train` from the last safe pre-Running
/// state, and execute `evaluate` fresh. This is the scenario from
/// the design doc's crash-recovery worked example.
#[test]
fn sigkill_mid_stage_two_resumes_only_that_stage_and_successor() {
    let wf = linear_three_stage();
    let graph = Graph::build(&wf.stages).unwrap();

    let tmp = TempDir::new().unwrap();
    let journal_path = tmp.path().join("journal.db");
    let j = Journal::open(&journal_path).unwrap();
    let run = Uuid::now_v7();
    j.insert_run_start(run, "test", "host").unwrap();

    // fetch committed, train mid-run when SIGKILL hit.
    seed_to(&j, run, "fetch", StageState::Committed);
    seed_to(&j, run, "train", StageState::Running);
    // evaluate never started.

    // Drop `train`'s scratch outs — a SIGKILL leaves a dead pid
    // and usually no matching on-disk files. The fs getter
    // reports that truth for `train` and defaults for everything
    // else.
    let fs_getter = |stage: &StageName| match stage.as_str() {
        "train" => FsState {
            child_pid_alive: false,
            outs_match_journal: false,
            staged_xorbs_present: false,
        },
        _ => FsState::default(),
    };

    let report = walk_dag(&graph, &j, run, CliFlags::default(), &fs_getter).unwrap();

    // Order is deterministic via Graph::toposort — producer
    // before consumer on a linear chain.
    let names: Vec<&str> = report.order.iter().map(|n| n.as_str()).collect();
    assert_eq!(names, vec!["fetch", "train", "evaluate"]);

    assert_eq!(
        action_of(&report, "fetch"),
        StageAction::Skip { cached: true },
        "fetch committed → skip"
    );
    assert_eq!(
        action_of(&report, "train"),
        StageAction::Resume(ResumeAction::RestartFromResolved),
        "train mid-Running with dead pid → restart from Resolved"
    );
    assert_eq!(
        action_of(&report, "evaluate"),
        StageAction::Execute,
        "evaluate cascades to Execute because its producer train is restarting"
    );
}

/// Same DAG, `train` crashed at `Staged` with its staged xorbs
/// still present. Resume continues the publication path rather
/// than recomputing from scratch, so `evaluate` is safe to leave
/// alone — the outs are about to be published, not regenerated.
#[test]
fn crash_at_staged_does_not_cascade_to_consumer() {
    let wf = linear_three_stage();
    let graph = Graph::build(&wf.stages).unwrap();

    let tmp = TempDir::new().unwrap();
    let j = Journal::open(&tmp.path().join("journal.db")).unwrap();
    let run = Uuid::now_v7();
    j.insert_run_start(run, "test", "host").unwrap();

    seed_to(&j, run, "fetch", StageState::Committed);
    seed_to(&j, run, "train", StageState::Staged);
    // evaluate never started.

    let fs_getter = |stage: &StageName| match stage.as_str() {
        "train" => FsState {
            staged_xorbs_present: true,
            ..FsState::default()
        },
        _ => FsState::default(),
    };

    let report = walk_dag(&graph, &j, run, CliFlags::default(), &fs_getter).unwrap();

    assert_eq!(
        action_of(&report, "train"),
        StageAction::Resume(ResumeAction::ResumeFromEntryWritten),
        "staged train resumes forward, no recompute"
    );
    // `evaluate` has no journal row; since train is not recomputing,
    // no cascade applies and evaluate runs fresh (not because of
    // cascade — because it has nothing committed).
    assert_eq!(action_of(&report, "evaluate"), StageAction::Execute);
}

/// All-committed terminal case: re-running the same workflow
/// after a successful prior run produces an all-skip plan.
#[test]
fn fully_cached_run_yields_all_skip() {
    let wf = linear_three_stage();
    let graph = Graph::build(&wf.stages).unwrap();

    let tmp = TempDir::new().unwrap();
    let j = Journal::open(&tmp.path().join("journal.db")).unwrap();
    let run = Uuid::now_v7();
    j.insert_run_start(run, "test", "host").unwrap();

    for name in ["fetch", "train", "evaluate"] {
        seed_to(&j, run, name, StageState::Committed);
    }

    let report = walk_dag(&graph, &j, run, CliFlags::default(), &|_: &StageName| {
        FsState::default()
    })
    .unwrap();

    for name in ["fetch", "train", "evaluate"] {
        assert_eq!(
            action_of(&report, name),
            StageAction::Skip { cached: true },
            "stage {name} should be a cached skip"
        );
    }
}
