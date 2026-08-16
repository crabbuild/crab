//! Workflow DAG planner.
//!
//! [`petgraph::DiGraph`] wrapper with path-based
//! edge inference, cycle detection, and deterministic topological
//! sort.
//!
//! The graph is constructed from a stage map by walking every stage's `outs`
//! and `deps`:
//!
//! - Two stages that both declare the same out path are rejected
//!   up front with [`WorkflowError::WorkflowDuplicateOutput`], because
//!   otherwise edge inference and materialization both race.
//! - An edge runs from producer to consumer when the consumer's
//!   [`Dep::Path`] equals a producer's [`Out`] of kind
//!   [`OutKind::File`], OR when the consumer's path descends into
//!   a producer's directory-kind out.
//! - A [`Dep::StageOut`] is a canonical, explicit edge. The
//!   referenced stage must exist AND must declare the named out
//!   path; otherwise [`WorkflowError::WorkflowUndefinedOut`].
//! - Self-loops (a stage consuming its own out) and larger cycles
//!   are rejected with [`WorkflowError::WorkflowCycle`].
//!
//! Topological order is deterministic: Kahn's algorithm with a
//! min-heap on stage name for tie-breaking, so any two runs
//! against the same workflow produce identical schedules.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::path::{Component, Path, PathBuf};

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::{Dep, Out, OutKind, Result, Stage, StageName, WorkflowError};

/// Workflow DAG.
///
/// Wraps a [`DiGraph<StageName, ()>`][DiGraph] (edges are the producer to
/// consumer relation; no per-edge payload is needed
/// today). [`Graph::build`] is the only constructor; it validates
/// the workflow in full before returning, so callers can assume a
/// successfully-built graph is cycle-free and has no duplicate out
/// paths.
#[derive(Debug)]
pub struct Graph {
    inner: DiGraph<StageName, ()>,
    indices: BTreeMap<StageName, NodeIndex>,
    /// Cached toposort, computed once at build time.
    toposorted: Vec<StageName>,
}

impl Graph {
    /// Build a DAG from parsed stages.
    ///
    /// Returns:
    /// - [`WorkflowError::WorkflowDuplicateOutput`] if two stages
    ///   declare the same normalized out path.
    /// - [`WorkflowError::WorkflowUndefinedOut`] if a
    ///   [`Dep::StageOut`] references a missing stage, or an out
    ///   path the producer does not declare.
    /// - [`WorkflowError::WorkflowCycle`] if the inferred DAG has a
    ///   cycle (including self-loops). The `stages` field lists
    ///   the members of one discovered cycle in order so the user
    ///   can read it back as a loop.
    pub fn build(stages: &BTreeMap<StageName, Stage>) -> Result<Self> {
        let mut inner = DiGraph::<StageName, ()>::new();
        let mut indices: BTreeMap<StageName, NodeIndex> = BTreeMap::new();

        // Materialize every node first so edge insertion below
        // can index by name in either order.
        for name in stages.keys() {
            let idx = inner.add_node(name.clone());
            indices.insert(name.clone(), idx);
        }

        // Producers indexed by normalized out path. Duplicate
        // declarations are a hard error: the edge-inference rule
        // below would pick an arbitrary producer, and at runtime
        // two stages writing the same path race.
        let mut file_producers: BTreeMap<PathBuf, StageName> = BTreeMap::new();
        let mut dir_producers: BTreeMap<PathBuf, StageName> = BTreeMap::new();

        for (stage_name, stage) in stages {
            for out in &stage.outs {
                let normalized = normalize_path(&out.path);
                // Reject duplicates across both maps: a file
                // produced by stage A and a directory covering
                // the same path produced by stage B would also
                // collide at materialization.
                if let Some(prev) = file_producers.get(&normalized) {
                    return Err(WorkflowError::WorkflowDuplicateOutput {
                        first: prev.as_str().to_owned(),
                        second: stage_name.as_str().to_owned(),
                        path: normalized,
                    });
                }
                if let Some(prev) = dir_producers.get(&normalized) {
                    return Err(WorkflowError::WorkflowDuplicateOutput {
                        first: prev.as_str().to_owned(),
                        second: stage_name.as_str().to_owned(),
                        path: normalized,
                    });
                }
                match out.kind {
                    OutKind::File | OutKind::Stdout => {
                        file_producers.insert(normalized, stage_name.clone());
                    }
                    OutKind::Directory => {
                        dir_producers.insert(normalized, stage_name.clone());
                    }
                }
            }
        }

        // Walk every dep and either (a) resolve explicit
        // Dep::StageOut to its producer, or (b) infer an edge from
        // path matching against producer outs.
        //
        // Uses a set to dedup producer to consumer pairs so
        // petgraph's multigraph semantics don't produce double
        // edges when a stage declares the same dep twice.
        let mut edges: BTreeSet<(StageName, StageName)> = BTreeSet::new();

        for (consumer_name, stage) in stages {
            for dep in &stage.deps {
                match dep {
                    Dep::StageOut {
                        stage: producer_name,
                        out,
                    } => {
                        let producer_stage = stages.get(producer_name).ok_or_else(|| {
                            WorkflowError::WorkflowUndefinedOut {
                                consumer: consumer_name.as_str().to_owned(),
                                out: format!("{}:{}", producer_name.as_str(), out.display()),
                            }
                        })?;
                        let normalized = normalize_path(out);
                        if !producer_declares(producer_stage, &normalized) {
                            return Err(WorkflowError::WorkflowUndefinedOut {
                                consumer: consumer_name.as_str().to_owned(),
                                out: format!("{}:{}", producer_name.as_str(), normalized.display()),
                            });
                        }
                        edges.insert((producer_name.clone(), consumer_name.clone()));
                    }
                    Dep::Path(path) => {
                        let normalized = normalize_path(path);
                        if let Some(producer) = file_producers.get(&normalized) {
                            edges.insert((producer.clone(), consumer_name.clone()));
                        } else if let Some(producer) =
                            find_directory_producer(&dir_producers, &normalized)
                        {
                            edges.insert((producer.clone(), consumer_name.clone()));
                        }
                        // Otherwise the dep is an external input
                        // (working-tree file, committed-blob, etc);
                        // no intra-DAG edge.
                    }
                    // Remote / external deps never contribute
                    // intra-DAG edges. They participate in the
                    // stage hash but not in scheduling.
                    Dep::CrabRef { .. }
                    | Dep::GitRef { .. }
                    | Dep::Url { .. }
                    | Dep::OciImage { .. } => {}
                }
            }
        }

        for (producer, consumer) in edges {
            // Safe to unwrap indices: both nodes were created in
            // the first pass above.
            let src = indices
                .get(&producer)
                .ok_or_else(|| WorkflowError::GraphInvariant {
                    message: format!("graph builder lost producer index for '{producer}'"),
                })?;
            let dst = indices
                .get(&consumer)
                .ok_or_else(|| WorkflowError::GraphInvariant {
                    message: format!("graph builder lost consumer index for '{consumer}'"),
                })?;
            inner.add_edge(*src, *dst, ());
        }

        let toposorted = kahn_toposort(&inner, &indices)?;

        Ok(Self {
            inner,
            indices,
            toposorted,
        })
    }

    /// Stages in topological order with stage-name tiebreak.
    ///
    /// Deterministic across runs: equal-priority nodes are drawn
    /// from a min-heap keyed on [`StageName`], which is lexicographic
    /// ASCII by construction.
    pub fn toposort(&self) -> Vec<StageName> {
        self.toposorted.clone()
    }

    /// Direct downstream stages of `stage`.
    ///
    /// Returns an empty vec if `stage` is not in the graph. Output
    /// is sorted by stage name for determinism.
    pub fn consumers_of(&self, stage: &StageName) -> Vec<StageName> {
        let Some(&idx) = self.indices.get(stage) else {
            return Vec::new();
        };
        let mut out: Vec<StageName> = self
            .inner
            .edges_directed(idx, Direction::Outgoing)
            .map(|edge| self.inner[edge.target()].clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Direct upstream stages of `stage`: the stages whose outs
    /// this stage depends on.
    ///
    /// Returns an empty vec if `stage` is not in the graph. Output
    /// is sorted by stage name for determinism.
    pub fn producers_of(&self, stage: &StageName) -> Vec<StageName> {
        let Some(&idx) = self.indices.get(stage) else {
            return Vec::new();
        };
        let mut out: Vec<StageName> = self
            .inner
            .edges_directed(idx, Direction::Incoming)
            .map(|edge| self.inner[edge.source()].clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Iterate every stage in topological order.
    pub fn stages(&self) -> impl Iterator<Item = &StageName> {
        self.toposorted.iter()
    }

    /// Number of nodes in the DAG. Useful for diagnostics; not
    /// part of any structured schema.
    pub fn len(&self) -> usize {
        self.inner.node_count()
    }

    /// Whether the DAG has no stages.
    pub fn is_empty(&self) -> bool {
        self.inner.node_count() == 0
    }
}

/// Kahn's algorithm with deterministic tie-breaking on stage name.
///
/// Uses a min-heap over [`StageName`] so equal-priority nodes
/// always come out in the same order regardless of iteration
/// order elsewhere. If the graph is cyclic, returns
/// [`WorkflowError::WorkflowCycle`] naming one discovered cycle.
fn kahn_toposort(
    graph: &DiGraph<StageName, ()>,
    indices: &BTreeMap<StageName, NodeIndex>,
) -> Result<Vec<StageName>> {
    // `BinaryHeap` is a max-heap, so wrap names in `Reverse` to
    // get min-heap behavior.
    use std::cmp::Reverse;

    let mut in_degree: Vec<usize> = graph
        .node_indices()
        .map(|idx| graph.edges_directed(idx, Direction::Incoming).count())
        .collect();

    let mut ready: BinaryHeap<Reverse<StageName>> = BinaryHeap::new();
    for (name, &idx) in indices {
        if in_degree[idx.index()] == 0 {
            ready.push(Reverse(name.clone()));
        }
    }

    let mut out = Vec::with_capacity(graph.node_count());
    while let Some(Reverse(name)) = ready.pop() {
        // Safe: `indices` is bijective with `graph`'s nodes.
        let Some(&idx) = indices.get(&name) else {
            return Err(WorkflowError::GraphInvariant {
                message: format!("toposort lost index for stage '{name}'"),
            });
        };
        out.push(name);
        for edge in graph.edges_directed(idx, Direction::Outgoing) {
            let dst = edge.target();
            in_degree[dst.index()] -= 1;
            if in_degree[dst.index()] == 0 {
                ready.push(Reverse(graph[dst].clone()));
            }
        }
    }

    if out.len() != graph.node_count() {
        // Some node was not drained; by Kahn's theorem the
        // remainder contains a cycle. Extract one representative
        // cycle so the error message is actionable.
        let cycle = find_cycle(graph, indices, &out);
        return Err(WorkflowError::WorkflowCycle {
            stages: cycle.into_iter().map(|n| n.as_str().to_owned()).collect(),
        });
    }

    Ok(out)
}

/// Depth-first search for one cycle among the nodes Kahn's
/// algorithm failed to drain.
///
/// The returned vector lists the cycle members in order with the
/// entry node repeated at the end, for example `[a, b, c, a]`, so the
/// user can read it off as the cycle it is.
fn find_cycle(
    graph: &DiGraph<StageName, ()>,
    indices: &BTreeMap<StageName, NodeIndex>,
    drained: &[StageName],
) -> Vec<StageName> {
    let drained: BTreeSet<&StageName> = drained.iter().collect();
    // Start from a node that wasn't drained. Walk an arbitrary
    // outgoing edge; mark every visit. The first time we revisit
    // a node already on the current path, we've closed a cycle.
    let Some(start_name) = indices.keys().find(|n| !drained.contains(n)).cloned() else {
        // Unreachable given the caller's invariant (`drained.len()
        // < graph.node_count()`); return an empty cycle rather
        // than manufacturing a placeholder name.
        return Vec::new();
    };

    let mut stack: Vec<StageName> = Vec::new();
    let mut on_path: BTreeSet<StageName> = BTreeSet::new();
    let mut cursor = start_name;

    loop {
        if on_path.contains(&cursor) {
            // Walk back through `stack` until we find the
            // matching entry; return the closed loop.
            let mut cycle: Vec<StageName> = Vec::new();
            let mut started = false;
            for name in &stack {
                if name == &cursor {
                    started = true;
                }
                if started {
                    cycle.push(name.clone());
                }
            }
            cycle.push(cursor);
            return cycle;
        }
        on_path.insert(cursor.clone());
        stack.push(cursor.clone());

        let Some(&idx) = indices.get(&cursor) else {
            // Graph/indices invariant lost; return what we
            // walked so far for diagnostic value.
            return stack;
        };

        // Deterministic choice of next edge: lowest-named
        // successor that is itself not yet drained.
        let mut next: Option<StageName> = None;
        for edge in graph.edges_directed(idx, Direction::Outgoing) {
            let candidate = graph[edge.target()].clone();
            if drained.contains(&candidate) {
                continue;
            }
            next = Some(match next {
                Some(prev) if prev < candidate => prev,
                _ => candidate,
            });
        }
        match next {
            Some(name) => cursor = name,
            None => return stack, // dead end; caller should treat as degraded diagnostic.
        }
    }
}

/// Normalize a path for comparison: drop leading `./` components
/// and collapse repeated separators, without touching the
/// filesystem (no canonicalization, no symlink resolution). A
/// dep at `./data/clean.csv` must match a producer out declared
/// as `data/clean.csv`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(s) => out.push(s),
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                // These are rejected by `Out::validate` upstream;
                // if they somehow reach us, preserve them so
                // matching fails loudly instead of collapsing
                // into something bogus.
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// Whether `stage` declares an out at exactly `normalized_path`
/// (of either kind).
fn producer_declares(stage: &Stage, normalized_path: &Path) -> bool {
    stage
        .outs
        .iter()
        .any(|out: &Out| normalize_path(&out.path) == normalized_path)
}

/// Find the producer whose directory-kind out covers
/// `consumer_path`: either the out's path equals the consumer
/// path (exact dir match) or the consumer path descends into the
/// producer's directory.
fn find_directory_producer(
    dir_producers: &BTreeMap<PathBuf, StageName>,
    consumer_path: &Path,
) -> Option<StageName> {
    // Longest-prefix wins: a consumer at `data/sub/file.csv` with
    // producers at `data/` and `data/sub/` should attach to the
    // more specific one.
    let mut best: Option<(&PathBuf, &StageName)> = None;
    for (dir_path, producer) in dir_producers {
        if consumer_path == dir_path.as_path() || path_starts_with(consumer_path, dir_path) {
            match best {
                Some((prev, _)) if prev.components().count() >= dir_path.components().count() => {}
                _ => best = Some((dir_path, producer)),
            }
        }
    }
    best.map(|(_, p)| p.clone())
}

/// Same-as-Path::starts_with but comparing components to avoid
/// substring matches like `data` matching `data_backup`.
fn path_starts_with(candidate: &Path, prefix: &Path) -> bool {
    let mut c = candidate.components();
    for pc in prefix.components() {
        match c.next() {
            Some(cc) if cc == pc => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cmd, EnvSpec, Resources};

    /// Build a minimal stage with the given name, deps, and outs.
    fn make_stage(name: &str, deps: Vec<Dep>, outs: Vec<Out>) -> (StageName, Stage) {
        let stage_name = StageName::parse(name).expect("valid name in test fixture");
        let stage = Stage {
            name: stage_name.clone(),
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
        (stage_name, stage)
    }

    fn make_workflow(stages: Vec<(StageName, Stage)>) -> BTreeMap<StageName, Stage> {
        let mut map = BTreeMap::new();
        for (name, stage) in stages {
            map.insert(name, stage);
        }
        map
    }

    fn file_out(path: &str) -> Out {
        Out::new(PathBuf::from(path), OutKind::File)
    }

    fn dir_out(path: &str) -> Out {
        Out::new(PathBuf::from(path), OutKind::Directory)
    }

    fn path_dep(path: &str) -> Dep {
        Dep::Path(PathBuf::from(path))
    }

    #[test]
    fn linear_chain_toposorts_in_order() {
        let wf = make_workflow(vec![
            make_stage("a", Vec::new(), vec![file_out("a.out")]),
            make_stage("b", vec![path_dep("a.out")], vec![file_out("b.out")]),
            make_stage("c", vec![path_dep("b.out")], vec![file_out("c.out")]),
        ]);
        let graph = Graph::build(&wf).expect("linear DAG builds");
        let order: Vec<String> = graph
            .toposort()
            .into_iter()
            .map(|n| n.as_str().to_owned())
            .collect();
        assert_eq!(order, vec!["a".to_owned(), "b".into(), "c".into()]);
    }

    #[test]
    fn diamond_dag_puts_root_first_and_sink_last() {
        // a to b, a to c, b to d, c to d.
        let wf = make_workflow(vec![
            make_stage("a", Vec::new(), vec![file_out("a.out")]),
            make_stage("b", vec![path_dep("a.out")], vec![file_out("b.out")]),
            make_stage("c", vec![path_dep("a.out")], vec![file_out("c.out")]),
            make_stage(
                "d",
                vec![path_dep("b.out"), path_dep("c.out")],
                vec![file_out("d.out")],
            ),
        ]);
        let graph = Graph::build(&wf).expect("diamond DAG builds");
        let order = graph.toposort();
        let names: Vec<&str> = order.iter().map(|n| n.as_str()).collect();
        assert_eq!(names.first().copied(), Some("a"));
        assert_eq!(names.last().copied(), Some("d"));
        // Middle layer is alphabetical for determinism.
        assert_eq!(names[1..3], ["b", "c"]);
    }

    #[test]
    fn simple_cycle_is_rejected() {
        // a to b, b to a.
        let wf = make_workflow(vec![
            make_stage("a", vec![path_dep("b.out")], vec![file_out("a.out")]),
            make_stage("b", vec![path_dep("a.out")], vec![file_out("b.out")]),
        ]);
        let err = Graph::build(&wf).expect_err("cycle should fail");
        match err {
            WorkflowError::WorkflowCycle { stages } => {
                let set: BTreeSet<String> = stages.into_iter().collect();
                assert!(set.contains("a"));
                assert!(set.contains("b"));
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn self_loop_is_rejected() {
        let wf = make_workflow(vec![make_stage(
            "a",
            vec![path_dep("a.out")],
            vec![file_out("a.out")],
        )]);
        let err = Graph::build(&wf).expect_err("self-loop should fail");
        match err {
            WorkflowError::WorkflowCycle { stages } => {
                assert!(
                    stages.contains(&"a".to_owned()),
                    "cycle missing 'a': {stages:?}"
                );
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn duplicate_out_path_across_stages_is_rejected() {
        let wf = make_workflow(vec![
            make_stage("a", Vec::new(), vec![file_out("shared.out")]),
            make_stage("b", Vec::new(), vec![file_out("shared.out")]),
        ]);
        let err = Graph::build(&wf).expect_err("duplicate out should fail");
        match err {
            WorkflowError::WorkflowDuplicateOutput {
                first,
                second,
                path,
            } => {
                // BTreeMap iteration fixes order by stage name;
                // 'a' is inserted first, 'b' second.
                assert_eq!(first, "a");
                assert_eq!(second, "b");
                assert_eq!(path, PathBuf::from("shared.out"));
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn stage_out_dep_pointing_at_missing_stage_is_rejected() {
        let missing = StageName::parse("ghost").unwrap();
        let wf = make_workflow(vec![make_stage(
            "consumer",
            vec![Dep::StageOut {
                stage: missing,
                out: PathBuf::from("x.bin"),
            }],
            Vec::new(),
        )]);
        let err = Graph::build(&wf).expect_err("missing producer should fail");
        assert!(
            matches!(err, WorkflowError::WorkflowUndefinedOut { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn stage_out_dep_pointing_at_undeclared_out_is_rejected() {
        let producer = StageName::parse("producer").unwrap();
        let wf = make_workflow(vec![
            make_stage("producer", Vec::new(), vec![file_out("real.out")]),
            make_stage(
                "consumer",
                vec![Dep::StageOut {
                    stage: producer,
                    out: PathBuf::from("phantom.out"),
                }],
                Vec::new(),
            ),
        ]);
        let err = Graph::build(&wf).expect_err("undeclared out should fail");
        match err {
            WorkflowError::WorkflowUndefinedOut { consumer, out } => {
                assert_eq!(consumer, "consumer");
                assert!(
                    out.contains("phantom.out"),
                    "out string missing path: {out}"
                );
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn path_matching_infers_file_edge() {
        let wf = make_workflow(vec![
            make_stage("producer", Vec::new(), vec![file_out("data/clean.csv")]),
            make_stage(
                "consumer",
                vec![path_dep("data/clean.csv")],
                vec![file_out("summary.html")],
            ),
        ]);
        let graph = Graph::build(&wf).expect("file edge inference");
        let consumer = StageName::parse("consumer").unwrap();
        let producers = graph.producers_of(&consumer);
        assert_eq!(
            producers
                .into_iter()
                .map(|n| n.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["producer".to_owned()]
        );
    }

    #[test]
    fn directory_out_matches_descendant_path_dep() {
        let wf = make_workflow(vec![
            make_stage("producer", Vec::new(), vec![dir_out("data")]),
            make_stage(
                "consumer",
                vec![path_dep("data/clean.csv")],
                vec![file_out("summary.html")],
            ),
        ]);
        let graph = Graph::build(&wf).expect("directory edge inference");
        let consumer = StageName::parse("consumer").unwrap();
        let producers = graph.producers_of(&consumer);
        assert_eq!(
            producers
                .into_iter()
                .map(|n| n.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["producer".to_owned()]
        );
    }

    #[test]
    fn dep_with_leading_dot_normalizes_for_matching() {
        let wf = make_workflow(vec![
            make_stage("a", Vec::new(), vec![file_out("data/x.bin")]),
            // Dep uses `./data/x.bin`; normalizer strips `./`.
            make_stage("b", vec![path_dep("./data/x.bin")], Vec::new()),
        ]);
        let graph = Graph::build(&wf).expect("normalized edge inference");
        let b = StageName::parse("b").unwrap();
        let producers = graph.producers_of(&b);
        assert_eq!(
            producers
                .into_iter()
                .map(|n| n.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["a".to_owned()]
        );
    }

    #[test]
    fn disconnected_stages_both_appear_in_toposort() {
        let wf = make_workflow(vec![
            make_stage("alpha", Vec::new(), vec![file_out("alpha.out")]),
            make_stage("bravo", Vec::new(), vec![file_out("bravo.out")]),
        ]);
        let graph = Graph::build(&wf).expect("disconnected DAG builds");
        let order = graph.toposort();
        let names: Vec<&str> = order.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo"]);
    }

    #[test]
    fn external_deps_do_not_create_edges() {
        let url_dep = Dep::Url {
            url: "https://example.com/blob".into(),
            digest: Some("b3:0".into()),
        };
        let wf = make_workflow(vec![make_stage("alpha", vec![url_dep], Vec::new())]);
        let graph = Graph::build(&wf).expect("external-only DAG builds");
        let alpha = StageName::parse("alpha").unwrap();
        assert!(graph.producers_of(&alpha).is_empty());
    }

    #[test]
    fn consumers_and_producers_are_deterministic() {
        // a to b, a to c. Both calls should yield sorted names.
        let wf = make_workflow(vec![
            make_stage("a", Vec::new(), vec![file_out("a.out")]),
            make_stage("b", vec![path_dep("a.out")], Vec::new()),
            make_stage("c", vec![path_dep("a.out")], Vec::new()),
        ]);
        let graph = Graph::build(&wf).expect("fanout DAG builds");
        let a = StageName::parse("a").unwrap();
        let c = graph.consumers_of(&a);
        let names: Vec<&str> = c.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    // Generators build random DAGs constrained to be acyclic by
    // construction: stages are emitted in a fixed left-to-right
    // order, and each stage's deps may only point at earlier
    // stages. Injected back-edges (for the cycle-rejection
    // property) violate that invariant on purpose.

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]

        /// Toposort respects every declared edge: for every
        /// `(producer, consumer)` pair in the graph, the producer
        /// must appear before the consumer in the returned order.
        #[test]
        fn prop_random_dag_toposort_respects_edges(spec in arb_dag(2..12usize)) {
            let wf = spec.to_stages();
            let graph = Graph::build(&wf).expect("acyclic DAG builds");
            let order = graph.toposort();
            let positions: BTreeMap<StageName, usize> = order
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), i))
                .collect();
            for (producer_idx, consumer_idx) in &spec.edges {
                let producer = spec.name(*producer_idx);
                let consumer = spec.name(*consumer_idx);
                let p_pos = positions.get(&producer).copied().unwrap_or(usize::MAX);
                let c_pos = positions.get(&consumer).copied().unwrap_or(usize::MAX);
                proptest::prop_assert!(
                    p_pos < c_pos,
                    "edge {} to {} but positions were {p_pos}/{c_pos}",
                    producer.as_str(),
                    consumer.as_str()
                );
            }
            // Sanity: every node appears exactly once.
            proptest::prop_assert_eq!(order.len(), spec.node_count);
        }

        /// Injecting a back-edge that mirrors an existing
        /// forward edge turns an acyclic DAG into a cyclic one;
        /// the builder must reject.
        #[test]
        fn prop_injected_backedge_is_rejected(
            spec in arb_dag(3..10usize),
            pick in 0u16..1024,
        ) {
            // Need at least one forward edge to mirror into a
            // back-edge. Skip otherwise; no cycle possible.
            proptest::prop_assume!(!spec.edges.is_empty());
            let (producer, consumer) = spec.edges[(pick as usize) % spec.edges.len()];
            let mut with_cycle = spec.clone();
            // Add `consumer to producer` on top of the existing
            // `producer to consumer`, guaranteeing a 2-cycle.
            with_cycle.edges.push((consumer, producer));
            let wf = with_cycle.to_stages();
            let err = Graph::build(&wf).expect_err("cycle must be rejected");
            proptest::prop_assert!(
                matches!(err, WorkflowError::WorkflowCycle { .. }),
                "expected WorkflowCycle, got {err}"
            );
        }
    }

    /// Compact DAG description: stages are numbered 0..node_count,
    /// and each edge is a `(producer_idx, consumer_idx)` pair with
    /// `producer_idx < consumer_idx`. Stage names are synthesized
    /// deterministically so proptest shrinking stays meaningful.
    #[derive(Debug, Clone)]
    struct DagSpec {
        node_count: usize,
        edges: Vec<(usize, usize)>,
    }

    impl DagSpec {
        fn name(&self, idx: usize) -> StageName {
            StageName::parse(&format!("s{idx:03}")).expect("synthesized name is valid")
        }

        fn to_stages(&self) -> BTreeMap<StageName, Stage> {
            let mut stages = Vec::with_capacity(self.node_count);
            // Collect inbound edges per consumer so each stage
            // can be built with its full dep list.
            let mut inbound: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (p, c) in &self.edges {
                inbound.entry(*c).or_default().push(*p);
            }
            for i in 0..self.node_count {
                let name = self.name(i);
                let out_path = PathBuf::from(format!("out/s{i:03}.bin"));
                let deps: Vec<Dep> = inbound
                    .get(&i)
                    .map(|producers| {
                        producers
                            .iter()
                            .map(|p| Dep::Path(PathBuf::from(format!("out/s{p:03}.bin"))))
                            .collect()
                    })
                    .unwrap_or_default();
                let stage = Stage {
                    name: name.clone(),
                    cmd: Cmd::Shell("true".into()),
                    deps,
                    outs: vec![Out::new(out_path, OutKind::File)],
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
                stages.push((name, stage));
            }
            make_workflow(stages)
        }
    }

    fn arb_dag(
        node_range: std::ops::Range<usize>,
    ) -> impl proptest::prelude::Strategy<Value = DagSpec> {
        use proptest::prelude::*;

        (node_range)
            .prop_flat_map(|node_count| {
                // For each pair `(producer, consumer)` with
                // `producer < consumer`, flip a bit to include it
                // in the edge set. Keeps the generator
                // shrink-friendly and guarantees acyclicity.
                let total_pairs = node_count * node_count.saturating_sub(1) / 2;
                (
                    Just(node_count),
                    proptest::collection::vec(any::<bool>(), total_pairs),
                )
            })
            .prop_map(|(node_count, bits)| {
                let mut edges = Vec::new();
                let mut cursor = 0;
                for consumer in 1..node_count {
                    for producer in 0..consumer {
                        if bits.get(cursor).copied().unwrap_or(false) {
                            edges.push((producer, consumer));
                        }
                        cursor += 1;
                    }
                }
                DagSpec { node_count, edges }
            })
    }
}
