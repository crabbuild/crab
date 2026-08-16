//! Lightweight commit graph index for shallow boundary computation.
//!
//! [`CommitGraphSummary`] is stored on the remote at `{prefix}/commit-graph-summary`
//! and CAS-updated during push. It enables shallow boundary computation without
//! downloading all packs.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

/// Lightweight commit graph index stored on the remote.
///
/// Contains commit OIDs, generation numbers, and parent pointers.
/// CAS-updated before ref publication; fetch traversal remains rooted at refs
/// from the committed manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitGraphSummary {
    /// Monotonically increasing version counter, bumped on each push.
    pub generation: u64,
    /// All commits known to the remote.
    pub commits: Vec<CommitEntry>,
}

impl CommitGraphSummary {
    /// Maximum commits before compaction triggers.
    pub const DEFAULT_MAX_COMMITS: usize = 10_000;
    /// Number of generations retained by the shipped generation-window API.
    pub const DEFAULT_GENERATION_WINDOW: u64 = 1_000;

    /// Append new commits to the summary, deduplicating by OID, and bump the
    /// generation counter. Commits already present in the summary are skipped.
    pub fn append_commits(&mut self, new_commits: &[CommitEntry]) {
        let mut known = HashSet::new();
        self.commits.retain(|entry| known.insert(entry.oid.clone()));
        normalize_stored_generation_numbers(&mut self.commits);

        let base_generations: HashMap<String, u64> = self
            .commits
            .iter()
            .map(|entry| (entry.oid.clone(), entry.gen_number))
            .collect();
        let mut additions = Vec::new();

        for entry in new_commits {
            if known.insert(entry.oid.clone()) {
                additions.push(entry.clone());
            }
        }

        fill_generation_numbers_with_base(&mut additions, &base_generations);
        self.commits.extend(additions);

        self.generation += 1;
    }

    /// Append new commits and compact when the total exceeds `max_commits`.
    ///
    /// Compaction retains at most `max_commits`, preferring the newest
    /// topological generations. Parent links at the retained edge remain in
    /// place so shallow traversal can identify that the summary was truncated.
    pub fn append_commits_with_limit(&mut self, new_commits: &[CommitEntry], max_commits: usize) {
        self.append_commits(new_commits);
        self.compact_to_limit(max_commits);
    }

    /// Retain at most `max_commits`, preferring the newest generations.
    ///
    /// The summary is a bounded acceleration structure. A requested ref or
    /// ancestor outside this window must fall back to a full fetch rather than
    /// being treated as complete history.
    pub fn compact_to_limit(&mut self, max_commits: usize) {
        let mut seen = HashSet::new();
        self.commits.retain(|entry| seen.insert(entry.oid.clone()));
        normalize_stored_generation_numbers(&mut self.commits);
        if self.commits.len() <= max_commits {
            return;
        }

        self.commits.sort_unstable_by(|left, right| {
            right
                .gen_number
                .cmp(&left.gen_number)
                .then_with(|| left.oid.cmp(&right.oid))
        });
        self.commits.truncate(max_commits);
    }

    /// Retain only commits reachable from the most recent `generation_window`
    /// generations via BFS over parent edges.
    ///
    /// This preserves all commits needed for shallow boundary computation at
    /// any depth ≤ `generation_window`.
    pub fn compact(&mut self, generation_window: u64) {
        if self.commits.is_empty() {
            return;
        }
        normalize_stored_generation_numbers(&mut self.commits);

        let max_gen = self.commits.iter().map(|c| c.gen_number).max().unwrap_or(0);
        let cutoff = max_gen.saturating_sub(generation_window);

        // Build the reachable set in a separate scope so the immutable borrow
        // on self.commits is released before we call retain().
        let reachable = {
            let by_oid: HashMap<&str, &CommitEntry> =
                self.commits.iter().map(|c| (c.oid.as_str(), c)).collect();

            // BFS from all commits within the generation window, following parents
            // to capture the full reachable set (including ancestors below cutoff
            // that are parents of retained commits).
            let mut reachable: HashSet<String> = HashSet::new();
            let mut queue: VecDeque<&str> = self
                .commits
                .iter()
                .filter(|c| c.gen_number >= cutoff)
                .map(|c| c.oid.as_str())
                .collect();

            while let Some(oid) = queue.pop_front() {
                if !reachable.insert(oid.to_owned()) {
                    continue;
                }
                if let Some(entry) = by_oid.get(oid) {
                    for parent in &entry.parents {
                        if !reachable.contains(parent.as_str())
                            && by_oid.contains_key(parent.as_str())
                        {
                            queue.push_back(parent.as_str());
                        }
                    }
                }
            }

            reachable
        };

        let mut seen = HashSet::new();
        self.commits
            .retain(|c| reachable.contains(&c.oid) && seen.insert(c.oid.clone()));
    }

    /// Walk `new`'s ancestry in the summary, bounded to at most
    /// `window_commits` visited commits, and return `true` as soon as
    /// `old` is found.
    ///
    /// Used as the fast-forward fallback when
    /// `git merge-base --is-ancestor` can't resolve the question
    /// (shallow / sparse client without the old tip locally). The
    /// walk consults only what's recorded in this summary — if the
    /// true ancestor chain reaches back further than the compaction
    /// window, the walk stops at the summary's edge and returns
    /// `false`. Callers treat a `false` here as "unknown,
    /// conservatively not fast-forward" unless the pusher passed
    /// `force`.
    ///
    /// Returns `true` immediately when `old == new` since a ref
    /// update to the current tip is trivially a fast-forward.
    /// Returns `false` when the summary is empty, `window_commits`
    /// is zero, or `new` is not present in the summary.
    #[must_use]
    pub fn is_ancestor(&self, old: &str, new: &str, window_commits: usize) -> bool {
        if old == new {
            return true;
        }
        if window_commits == 0 {
            return false;
        }

        let parents_by_oid: HashMap<&str, &[String]> = self
            .commits
            .iter()
            .map(|c| (c.oid.as_str(), c.parents.as_slice()))
            .collect();

        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut seen: HashSet<&str> = HashSet::new();
        queue.push_back(new);
        seen.insert(new);

        let mut visited = 0usize;
        while let Some(cur) = queue.pop_front() {
            visited += 1;
            if visited > window_commits {
                return false;
            }
            if cur == old {
                return true;
            }
            if let Some(parents) = parents_by_oid.get(cur) {
                for parent in parents.iter() {
                    let p = parent.as_str();
                    if seen.insert(p) {
                        queue.push_back(p);
                    }
                }
            }
            // If `cur` isn't in the summary, we've hit the edge of
            // the window. Keep walking the rest of the queue; the
            // bound above takes care of termination.
        }

        false
    }
}

/// A single commit in the graph summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitEntry {
    /// Commit OID (hex SHA-1).
    pub oid: String,
    /// Topological generation number (distance from root).
    pub gen_number: u64,
    /// Parent commit OIDs.
    pub parents: Vec<String>,
}

/// Compute topological generation numbers for `entries` in place.
///
/// `gen(commit) = 1 + max(gen(parents))`, with root commits at gen 0.
/// Any parent OIDs not present in `entries` (e.g., parents outside
/// the incremental walk window) are treated as having generation `0`,
/// which makes their children gen `1`.
///
/// Avoids relying on traversal order: some Git walks visit children before
/// parents, so generation assignment must derive order from parent edges.
pub fn fill_generation_numbers(entries: &mut [CommitEntry]) {
    fill_generation_numbers_with_base(entries, &HashMap::new());
}

fn fill_generation_numbers_with_base(
    entries: &mut [CommitEntry],
    base_generations: &HashMap<String, u64>,
) {
    if entries.is_empty() {
        return;
    }

    // Build index: oid → position in entries.
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.oid.as_str(), i))
        .collect();

    // Compute in-degree: how many parents of this commit are in `entries`?
    // Commits whose parents are all outside the window are "roots" for the
    // purpose of topological ordering.
    let mut in_degree: Vec<usize> = entries
        .iter()
        .map(|e| {
            e.parents
                .iter()
                .filter(|p| index.contains_key(p.as_str()))
                .count()
        })
        .collect();

    // Build forward adjacency: parent → list of child indices.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (child_idx, entry) in entries.iter().enumerate() {
        for p in &entry.parents {
            if let Some(&parent_idx) = index.get(p.as_str()) {
                children[parent_idx].push(child_idx);
            }
        }
    }

    // Kahn's algorithm: process nodes in topological order.
    let mut queue: std::collections::VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter_map(|(i, &d)| (d == 0).then_some(i))
        .collect();

    // Initialize generations from parents outside this batch. Known parents
    // retain their stored generation; unknown parents are the edge of the
    // summary and conservatively contribute generation zero.
    let mut gens: Vec<u64> = entries
        .iter()
        .map(|e| {
            if e.parents.is_empty() {
                0
            } else {
                e.parents
                    .iter()
                    .filter(|parent| !index.contains_key(parent.as_str()))
                    .map(|parent| {
                        base_generations
                            .get(parent.as_str())
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(1)
                    })
                    .max()
                    .unwrap_or(0)
            }
        })
        .collect();

    let mut processed = 0usize;
    while let Some(i) = queue.pop_front() {
        processed += 1;
        let this_gen = gens[i];
        // `children` shape is derived from entries; the take dance avoids
        // a borrow-split against gens during iteration.
        let successors = std::mem::take(&mut children[i]);
        for child in successors {
            // Propagate: child gen is max(child, this_gen + 1).
            let candidate = this_gen.saturating_add(1);
            if gens[child] < candidate {
                gens[child] = candidate;
            }
            in_degree[child] -= 1;
            if in_degree[child] == 0 {
                queue.push_back(child);
            }
        }
    }

    // If processed < entries.len(), the graph has a cycle. Git history is
    // a DAG so this shouldn't happen, but fall back to gen 0 for any
    // unprocessed nodes to avoid garbage values.
    if processed < entries.len() {
        tracing::warn!(
            total = entries.len(),
            processed,
            "commit graph has a cycle or corrupt parent links; unprocessed commits get gen 0"
        );
    }

    for (entry, gen_number) in entries.iter_mut().zip(gens) {
        entry.gen_number = gen_number;
    }
}

fn normalize_stored_generation_numbers(entries: &mut [CommitEntry]) {
    let known: HashSet<&str> = entries.iter().map(|entry| entry.oid.as_str()).collect();
    let mut edge_generations = HashMap::new();
    for entry in entries.iter() {
        for parent in &entry.parents {
            if !known.contains(parent.as_str()) {
                edge_generations
                    .entry(parent.clone())
                    .and_modify(|generation: &mut u64| {
                        *generation = (*generation).max(entry.gen_number.saturating_sub(1));
                    })
                    .or_insert_with(|| entry.gen_number.saturating_sub(1));
            }
        }
    }
    fill_generation_numbers_with_base(entries, &edge_generations);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn serialize_deserialize_round_trip() {
        let summary = CommitGraphSummary {
            generation: 7,
            commits: vec![
                CommitEntry {
                    oid: "abc123".to_string(),
                    gen_number: 3,
                    parents: vec!["def456".to_string()],
                },
                CommitEntry {
                    oid: "def456".to_string(),
                    gen_number: 2,
                    parents: vec!["000000".to_string()],
                },
            ],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: CommitGraphSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.generation, 7);
        assert_eq!(parsed.commits.len(), 2);
        assert_eq!(parsed.commits[0].oid, "abc123");
        assert_eq!(parsed.commits[0].gen_number, 3);
        assert_eq!(parsed.commits[0].parents, vec!["def456"]);
        assert_eq!(parsed.commits[1].oid, "def456");
        assert_eq!(parsed.commits[1].parents, vec!["000000"]);
    }

    #[test]
    fn empty_summary_round_trip() {
        let summary = CommitGraphSummary {
            generation: 0,
            commits: vec![],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: CommitGraphSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.generation, 0);
        assert!(parsed.commits.is_empty());
    }

    #[test]
    fn commit_with_multiple_parents() {
        let entry = CommitEntry {
            oid: "merge01".to_string(),
            gen_number: 10,
            parents: vec!["parent_a".to_string(), "parent_b".to_string()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: CommitEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.parents.len(), 2);
        assert_eq!(parsed.parents[0], "parent_a");
        assert_eq!(parsed.parents[1], "parent_b");
    }

    #[test]
    fn root_commit_has_no_parents() {
        let entry = CommitEntry {
            oid: "root00".to_string(),
            gen_number: 0,
            parents: vec![],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: CommitEntry = serde_json::from_str(&json).unwrap();

        assert!(parsed.parents.is_empty());
        assert_eq!(parsed.gen_number, 0);
    }

    #[test]
    fn default_summary_is_empty_generation_zero() {
        let summary = CommitGraphSummary::default();
        assert_eq!(summary.generation, 0);
        assert!(summary.commits.is_empty());
    }

    #[test]
    fn append_commits_adds_new_and_bumps_generation() {
        let mut summary = CommitGraphSummary::default();
        let commits = vec![
            CommitEntry {
                oid: "aaa".to_string(),
                gen_number: 1,
                parents: vec![],
            },
            CommitEntry {
                oid: "bbb".to_string(),
                gen_number: 2,
                parents: vec!["aaa".to_string()],
            },
        ];

        summary.append_commits(&commits);

        assert_eq!(summary.generation, 1);
        assert_eq!(summary.commits.len(), 2);
        assert_eq!(summary.commits[0].oid, "aaa");
        assert_eq!(summary.commits[1].oid, "bbb");
    }

    #[test]
    fn append_commits_deduplicates_existing_oids() {
        let mut summary = CommitGraphSummary {
            generation: 5,
            commits: vec![CommitEntry {
                oid: "aaa".to_string(),
                gen_number: 1,
                parents: vec![],
            }],
        };

        let new_commits = vec![
            CommitEntry {
                oid: "aaa".to_string(),
                gen_number: 1,
                parents: vec![],
            },
            CommitEntry {
                oid: "bbb".to_string(),
                gen_number: 2,
                parents: vec!["aaa".to_string()],
            },
        ];

        summary.append_commits(&new_commits);

        assert_eq!(summary.generation, 6);
        assert_eq!(summary.commits.len(), 2);
        assert_eq!(summary.commits[0].oid, "aaa");
        assert_eq!(summary.commits[1].oid, "bbb");
    }

    #[test]
    fn append_commits_deduplicates_overlapping_multi_ref_histories() {
        let mut summary = CommitGraphSummary {
            generation: 1,
            commits: vec![CommitEntry {
                oid: "root".to_owned(),
                gen_number: 0,
                parents: vec![],
            }],
        };
        let branch_history = vec![
            CommitEntry {
                oid: "tip".to_owned(),
                gen_number: 0,
                parents: vec!["parent".to_owned()],
            },
            CommitEntry {
                oid: "parent".to_owned(),
                gen_number: 0,
                parents: vec!["root".to_owned()],
            },
        ];
        let mut overlapping_histories = branch_history.clone();
        overlapping_histories.extend(branch_history);

        summary.append_commits(&overlapping_histories);

        let unique: HashSet<&str> = summary
            .commits
            .iter()
            .map(|entry| entry.oid.as_str())
            .collect();
        assert_eq!(summary.commits.len(), 3);
        assert_eq!(unique.len(), summary.commits.len());
    }

    #[test]
    fn append_commits_preserves_incremental_generation_numbers() {
        let mut summary = CommitGraphSummary {
            generation: 7,
            commits: vec![
                CommitEntry {
                    oid: "existing-parent".to_owned(),
                    gen_number: 1_600,
                    parents: vec!["compacted-parent".to_owned()],
                },
                CommitEntry {
                    oid: "existing-tip".to_owned(),
                    gen_number: 1,
                    parents: vec!["existing-parent".to_owned()],
                },
            ],
        };
        let new_commits = vec![
            CommitEntry {
                oid: "new-tip".to_owned(),
                gen_number: 1,
                parents: vec!["new-parent".to_owned()],
            },
            CommitEntry {
                oid: "new-parent".to_owned(),
                gen_number: 0,
                parents: vec!["existing-tip".to_owned()],
            },
        ];

        summary.append_commits(&new_commits);

        let by_oid: HashMap<&str, &CommitEntry> = summary
            .commits
            .iter()
            .map(|entry| (entry.oid.as_str(), entry))
            .collect();
        assert_eq!(by_oid["existing-tip"].gen_number, 1_601);
        assert_eq!(by_oid["new-parent"].gen_number, 1_602);
        assert_eq!(by_oid["new-tip"].gen_number, 1_603);
    }

    #[test]
    fn append_limit_bounds_history_over_ten_thousand_commits() {
        let mut summary = CommitGraphSummary::default();
        let commits = (0..=CommitGraphSummary::DEFAULT_MAX_COMMITS)
            .map(|index| CommitEntry {
                oid: format!("commit-{index:05}"),
                gen_number: index as u64,
                parents: (index > 0)
                    .then(|| format!("commit-{:05}", index - 1))
                    .into_iter()
                    .collect(),
            })
            .collect::<Vec<_>>();

        summary.append_commits_with_limit(&commits, CommitGraphSummary::DEFAULT_MAX_COMMITS);

        let unique: HashSet<&str> = summary
            .commits
            .iter()
            .map(|entry| entry.oid.as_str())
            .collect();
        assert_eq!(
            summary.commits.len(),
            CommitGraphSummary::DEFAULT_MAX_COMMITS
        );
        assert_eq!(unique.len(), summary.commits.len());
        assert!(
            summary
                .commits
                .iter()
                .any(|entry| entry.oid == "commit-10000")
        );
        assert!(
            !summary
                .commits
                .iter()
                .any(|entry| entry.oid == "commit-00000")
        );
    }

    #[test]
    fn append_empty_commits_still_bumps_generation() {
        let mut summary = CommitGraphSummary {
            generation: 3,
            commits: vec![],
        };

        summary.append_commits(&[]);

        assert_eq!(summary.generation, 4);
        assert!(summary.commits.is_empty());
    }

    #[test]
    fn fill_generation_numbers_linear_history() {
        // A → B → C → D (D is tip). BFS yields D, C, B, A.
        let mut entries = vec![
            CommitEntry {
                oid: "D".to_string(),
                gen_number: 0,
                parents: vec!["C".to_string()],
            },
            CommitEntry {
                oid: "C".to_string(),
                gen_number: 0,
                parents: vec!["B".to_string()],
            },
            CommitEntry {
                oid: "B".to_string(),
                gen_number: 0,
                parents: vec!["A".to_string()],
            },
            CommitEntry {
                oid: "A".to_string(),
                gen_number: 0,
                parents: vec![],
            },
        ];

        fill_generation_numbers(&mut entries);

        let by_oid: HashMap<&str, &CommitEntry> =
            entries.iter().map(|e| (e.oid.as_str(), e)).collect();
        assert_eq!(by_oid["A"].gen_number, 0);
        assert_eq!(by_oid["B"].gen_number, 1);
        assert_eq!(by_oid["C"].gen_number, 2);
        assert_eq!(by_oid["D"].gen_number, 3);
    }

    #[test]
    fn fill_generation_numbers_merge_history() {
        //   A
        //  / \
        // B   C
        //  \ /
        //   D (merge)
        let mut entries = vec![
            CommitEntry {
                oid: "D".to_string(),
                gen_number: 0,
                parents: vec!["B".to_string(), "C".to_string()],
            },
            CommitEntry {
                oid: "C".to_string(),
                gen_number: 0,
                parents: vec!["A".to_string()],
            },
            CommitEntry {
                oid: "B".to_string(),
                gen_number: 0,
                parents: vec!["A".to_string()],
            },
            CommitEntry {
                oid: "A".to_string(),
                gen_number: 0,
                parents: vec![],
            },
        ];

        fill_generation_numbers(&mut entries);

        let by_oid: HashMap<&str, &CommitEntry> =
            entries.iter().map(|e| (e.oid.as_str(), e)).collect();
        assert_eq!(by_oid["A"].gen_number, 0);
        assert_eq!(by_oid["B"].gen_number, 1);
        assert_eq!(by_oid["C"].gen_number, 1);
        assert_eq!(by_oid["D"].gen_number, 2);
    }

    #[test]
    fn fill_generation_numbers_parents_outside_window() {
        // Only B and C are in the window; A is outside (e.g., already pushed).
        // C's parent A is not in the entries list.
        let mut entries = vec![
            CommitEntry {
                oid: "C".to_string(),
                gen_number: 0,
                parents: vec!["B".to_string()],
            },
            CommitEntry {
                oid: "B".to_string(),
                gen_number: 0,
                parents: vec!["A".to_string()], // A not in window
            },
        ];

        fill_generation_numbers(&mut entries);

        // B's in-window parents is empty; it becomes a topo root with gen 1
        // (because it has at least one parent, just outside the window).
        // C then gets gen 2.
        let by_oid: HashMap<&str, &CommitEntry> =
            entries.iter().map(|e| (e.oid.as_str(), e)).collect();
        assert_eq!(by_oid["B"].gen_number, 1);
        assert_eq!(by_oid["C"].gen_number, 2);
    }

    #[test]
    fn fill_generation_numbers_empty_is_noop() {
        let mut entries: Vec<CommitEntry> = Vec::new();
        fill_generation_numbers(&mut entries);
        assert!(entries.is_empty());
    }

    // --- is_ancestor tests ---

    /// Helper: build a summary from (oid, parents) pairs.
    fn summary_from(entries: Vec<(&str, Vec<&str>)>) -> CommitGraphSummary {
        CommitGraphSummary {
            generation: 1,
            commits: entries
                .into_iter()
                .map(|(oid, parents)| CommitEntry {
                    oid: oid.to_owned(),
                    gen_number: 0,
                    parents: parents.into_iter().map(str::to_owned).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn is_ancestor_same_sha_is_trivially_true() {
        let s = summary_from(vec![]);
        assert!(s.is_ancestor("abc", "abc", 1000));
    }

    #[test]
    fn is_ancestor_zero_window_returns_false() {
        let s = summary_from(vec![("B", vec!["A"]), ("A", vec![])]);
        assert!(!s.is_ancestor("A", "B", 0));
    }

    #[test]
    fn is_ancestor_linear_history_finds_ancestor() {
        // A → B → C (C is tip)
        let s = summary_from(vec![("C", vec!["B"]), ("B", vec!["A"]), ("A", vec![])]);
        assert!(s.is_ancestor("A", "C", 1000));
        assert!(s.is_ancestor("B", "C", 1000));
    }

    #[test]
    fn is_ancestor_rejects_when_new_not_descendant() {
        // Divergent: A → B, A → C. B and C are siblings.
        let s = summary_from(vec![("B", vec!["A"]), ("C", vec!["A"]), ("A", vec![])]);
        assert!(!s.is_ancestor("B", "C", 1000));
    }

    #[test]
    fn is_ancestor_walks_merge_commit_parents() {
        // A → B, A → C, B+C → D
        let s = summary_from(vec![
            ("D", vec!["B", "C"]),
            ("B", vec!["A"]),
            ("C", vec!["A"]),
            ("A", vec![]),
        ]);
        assert!(s.is_ancestor("B", "D", 1000));
        assert!(s.is_ancestor("C", "D", 1000));
        assert!(s.is_ancestor("A", "D", 1000));
    }

    #[test]
    fn is_ancestor_window_exhausted_returns_false() {
        // A → B → C → D → E. Old = A, new = E, window = 2 → walks E,
        // D but exceeds the bound before finding A. Expected false
        // (conservative "unknown").
        let s = summary_from(vec![
            ("E", vec!["D"]),
            ("D", vec!["C"]),
            ("C", vec!["B"]),
            ("B", vec!["A"]),
            ("A", vec![]),
        ]);
        assert!(!s.is_ancestor("A", "E", 2));
    }

    #[test]
    fn is_ancestor_unknown_new_returns_false() {
        // `new` not in summary: first pop visits `new` which has no
        // parents recorded. Walk exhausts without finding `old`.
        let s = summary_from(vec![("A", vec![])]);
        assert!(!s.is_ancestor("A", "UNKNOWN", 1000));
    }

    #[test]
    fn is_ancestor_walks_up_to_the_window_bound() {
        // A → B → C. Window = 3 is exactly enough to visit C, B, A.
        let s = summary_from(vec![("C", vec!["B"]), ("B", vec!["A"]), ("A", vec![])]);
        assert!(s.is_ancestor("A", "C", 3));
    }
}
