//! Priority queue for background indexing work.
//!
//! Jobs are ordered by priority (lower numeric value = higher priority),
//! then by age (older jobs first). The queue deduplicates by `(repo_id, kind)`
//! — pushing a duplicate upgrades the existing entry's priority if the new
//! one is higher.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;

/// Indexing priority level. Lower numeric value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum IndexPriority {
    /// The repo is currently open and the user is actively viewing it.
    Active = 0,
    /// The repo was recently used but is not the active view.
    Recent = 1,
    /// Background indexing for all known repos.
    Background = 2,
}

impl IndexPriority {
    fn rank(self) -> u8 {
        self as u8
    }
}

impl PartialOrd for IndexPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexPriority {
    /// Lower numeric rank means higher priority.
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// The kind of indexing work to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexKind {
    /// Summarise files (size, type, pointer status).
    FileSummary,
    /// Build a git-log skeleton for the timeline.
    GitLog,
    /// Extract pointer metadata for lazy-checkout views.
    PointerMeta,
}

/// A unit of indexing work.
#[derive(Debug, Clone)]
pub struct IndexJob {
    pub repo_id: String,
    pub kind: IndexKind,
    pub priority: IndexPriority,
    pub queued_at: Instant,
}

impl PartialEq for IndexJob {
    fn eq(&self, other: &Self) -> bool {
        self.repo_id == other.repo_id && self.kind == other.kind
    }
}

impl Eq for IndexJob {}

impl PartialOrd for IndexJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexJob {
    /// BinaryHeap is a max-heap, so we reverse the natural ordering:
    /// - Higher priority (lower rank) should come out first → reverse priority comparison.
    /// - Among equal priorities, older jobs (earlier `queued_at`) come first → reverse time comparison.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .rank()
            .cmp(&self.priority.rank())
            .then_with(|| other.queued_at.cmp(&self.queued_at))
    }
}

/// A priority queue for indexing jobs.
///
/// Deduplicates by `(repo_id, kind)` — if a job with the same key already
/// exists, the higher-priority entry wins.
pub struct PriorityQueue {
    heap: BinaryHeap<IndexJob>,
}

impl PriorityQueue {
    /// Create an empty priority queue.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Add a job to the queue.
    ///
    /// If a job with the same `(repo_id, kind)` already exists, the entry
    /// with higher priority (lower numeric rank) is kept. If priorities are
    /// equal, the older entry is kept.
    pub fn push(&mut self, job: IndexJob) {
        // Check for an existing job with the same key.
        if let Some(existing) = self
            .heap
            .iter()
            .find(|j| j.repo_id == job.repo_id && j.kind == job.kind)
        {
            // Keep the higher-priority (lower rank) entry.
            if job.priority.rank() < existing.priority.rank() {
                self.remove_by_key(&job.repo_id, job.kind);
                self.heap.push(job);
            }
            // Otherwise the existing entry already has equal or better priority.
        } else {
            self.heap.push(job);
        }
    }

    /// Remove and return the highest-priority job.
    pub fn pop(&mut self) -> Option<IndexJob> {
        self.heap.pop()
    }

    /// Number of jobs in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Promote all jobs for a given repo to the specified priority.
    ///
    /// Only upgrades — if a job already has equal or higher priority, it is
    /// left unchanged.
    pub fn promote(&mut self, repo_id: &str, priority: IndexPriority) {
        let jobs: Vec<IndexJob> = self.heap.drain().collect();
        for mut job in jobs {
            if job.repo_id == repo_id && priority.rank() < job.priority.rank() {
                job.priority = priority;
            }
            self.heap.push(job);
        }
    }

    /// Remove a job by `(repo_id, kind)`. Internal helper for dedup.
    fn remove_by_key(&mut self, repo_id: &str, kind: IndexKind) {
        let jobs: Vec<IndexJob> = self
            .heap
            .drain()
            .filter(|j| !(j.repo_id == repo_id && j.kind == kind))
            .collect();
        for job in jobs {
            self.heap.push(job);
        }
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pop_returns_highest_priority_first() {
        let mut q = PriorityQueue::new();
        let now = Instant::now();

        q.push(IndexJob {
            repo_id: "bg-repo".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Background,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "active-repo".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Active,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "recent-repo".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Recent,
            queued_at: now,
        });

        let first = q.pop().expect("should have a job");
        assert_eq!(first.repo_id, "active-repo");

        let second = q.pop().expect("should have a job");
        assert_eq!(second.repo_id, "recent-repo");

        let third = q.pop().expect("should have a job");
        assert_eq!(third.repo_id, "bg-repo");
    }

    #[test]
    fn older_jobs_come_first_at_same_priority() {
        let mut q = PriorityQueue::new();
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(1);

        q.push(IndexJob {
            repo_id: "newer".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Recent,
            queued_at: later,
        });
        q.push(IndexJob {
            repo_id: "older".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Recent,
            queued_at: earlier,
        });

        let first = q.pop().expect("should have a job");
        assert_eq!(first.repo_id, "older");
    }

    #[test]
    fn push_deduplicates_by_repo_id_and_kind() {
        let mut q = PriorityQueue::new();
        let now = Instant::now();

        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Background,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Active,
            queued_at: now,
        });

        assert_eq!(q.len(), 1);
        let job = q.pop().expect("should have a job");
        assert_eq!(job.priority, IndexPriority::Active);
    }

    #[test]
    fn push_keeps_higher_priority_on_duplicate() {
        let mut q = PriorityQueue::new();
        let now = Instant::now();

        // Push with Active first, then Background — should keep Active.
        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Active,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Background,
            queued_at: now,
        });

        assert_eq!(q.len(), 1);
        let job = q.pop().expect("should have a job");
        assert_eq!(job.priority, IndexPriority::Active);
    }

    #[test]
    fn promote_upgrades_matching_jobs() {
        let mut q = PriorityQueue::new();
        let now = Instant::now();

        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Background,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::GitLog,
            priority: IndexPriority::Background,
            queued_at: now,
        });
        q.push(IndexJob {
            repo_id: "repo-b".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Background,
            queued_at: now,
        });

        q.promote("repo-a", IndexPriority::Active);

        // repo-a jobs should come out first.
        let first = q.pop().expect("should have a job");
        assert_eq!(first.repo_id, "repo-a");
        let second = q.pop().expect("should have a job");
        assert_eq!(second.repo_id, "repo-a");
        let third = q.pop().expect("should have a job");
        assert_eq!(third.repo_id, "repo-b");
        assert_eq!(third.priority, IndexPriority::Background);
    }

    #[test]
    fn promote_does_not_downgrade() {
        let mut q = PriorityQueue::new();
        let now = Instant::now();

        q.push(IndexJob {
            repo_id: "repo-a".into(),
            kind: IndexKind::FileSummary,
            priority: IndexPriority::Active,
            queued_at: now,
        });

        q.promote("repo-a", IndexPriority::Recent);

        let job = q.pop().expect("should have a job");
        assert_eq!(job.priority, IndexPriority::Active);
    }

    #[test]
    fn is_empty_and_len() {
        let mut q = PriorityQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        q.push(IndexJob {
            repo_id: "r".into(),
            kind: IndexKind::PointerMeta,
            priority: IndexPriority::Recent,
            queued_at: Instant::now(),
        });

        assert!(!q.is_empty());
        assert_eq!(q.len(), 1);

        q.pop();
        assert!(q.is_empty());
    }
}
