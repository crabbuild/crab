use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

// Large-input comparison must match occurrences in order: report builders pair
// unchanged entries in sequence. Set membership loses both pairing and counts.
pub(crate) fn greedy_ordered_matches<T, K: Eq + Hash>(
    old: &[T],
    new: &[T],
    key: impl Fn(&T) -> K,
) -> Vec<(usize, usize)> {
    let mut positions: HashMap<K, VecDeque<usize>> = HashMap::new();
    for (idx, entry) in new.iter().enumerate() {
        positions.entry(key(entry)).or_default().push_back(idx);
    }

    let mut matches = Vec::new();
    let mut next_allowed = 0usize;
    for (old_idx, entry) in old.iter().enumerate() {
        let Some(queue) = positions.get_mut(&key(entry)) else {
            continue;
        };
        while queue.front().is_some_and(|&index| index < next_allowed) {
            queue.pop_front();
        }
        if let Some(new_idx) = queue.pop_front() {
            matches.push((old_idx, new_idx));
            next_allowed = new_idx.saturating_add(1);
        }
    }
    matches
}
