//! Pointer-map pairing for diff planning.

use std::collections::BTreeMap;

use crab_types::pointer::Pointer;

use crate::types::FileStatus;

/// Pair files from two ref maps by path.
///
/// Unchanged files with identical file hashes are skipped. Returned entries
/// are sorted by path.
pub fn pair_files(
    old_map: &BTreeMap<String, Pointer>,
    new_map: &BTreeMap<String, Pointer>,
) -> Vec<(String, FileStatus, Option<Pointer>, Option<Pointer>)> {
    let mut result = Vec::new();
    let mut old_iter = old_map.iter().peekable();
    let mut new_iter = new_map.iter().peekable();

    loop {
        match (old_iter.peek(), new_iter.peek()) {
            (Some((old_path, _)), Some((new_path, _))) => match old_path.cmp(new_path) {
                std::cmp::Ordering::Less => {
                    let Some((path, ptr)) = old_iter.next() else {
                        break;
                    };
                    result.push((path.clone(), FileStatus::Deleted, Some(ptr.clone()), None));
                }
                std::cmp::Ordering::Greater => {
                    let Some((path, ptr)) = new_iter.next() else {
                        break;
                    };
                    result.push((path.clone(), FileStatus::Added, None, Some(ptr.clone())));
                }
                std::cmp::Ordering::Equal => {
                    let (Some((path, old_ptr)), Some((_path, new_ptr))) =
                        (old_iter.next(), new_iter.next())
                    else {
                        break;
                    };

                    if old_ptr.file_hash != new_ptr.file_hash {
                        result.push((
                            path.clone(),
                            FileStatus::Modified,
                            Some(old_ptr.clone()),
                            Some(new_ptr.clone()),
                        ));
                    }
                }
            },
            (Some(_), None) => {
                let Some((path, ptr)) = old_iter.next() else {
                    break;
                };
                result.push((path.clone(), FileStatus::Deleted, Some(ptr.clone()), None));
            }
            (None, Some(_)) => {
                let Some((path, ptr)) = new_iter.next() else {
                    break;
                };
                result.push((path.clone(), FileStatus::Added, None, Some(ptr.clone())));
            }
            (None, None) => break,
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pointer(hash_byte: u8, size: u64) -> Pointer {
        Pointer {
            file_hash: [hash_byte; 32],
            size,
            shard_hint: None,
        }
    }

    #[test]
    fn empty_maps_produce_no_pairs() {
        let old = BTreeMap::new();
        let new = BTreeMap::new();
        let result = pair_files(&old, &new);
        assert!(result.is_empty());
    }

    #[test]
    fn added_files_have_new_pointer_only() {
        let old = BTreeMap::new();
        let mut new = BTreeMap::new();
        new.insert("a.bin".to_owned(), make_pointer(0x01, 100));
        new.insert("b.bin".to_owned(), make_pointer(0x02, 200));

        let result = pair_files(&old, &new);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "a.bin");
        assert_eq!(result[0].1, FileStatus::Added);
        assert!(result[0].2.is_none());
        assert!(result[0].3.is_some());
        assert_eq!(result[1].0, "b.bin");
        assert_eq!(result[1].1, FileStatus::Added);
    }

    #[test]
    fn deleted_files_have_old_pointer_only() {
        let mut old = BTreeMap::new();
        old.insert("a.bin".to_owned(), make_pointer(0x01, 100));
        let new = BTreeMap::new();

        let result = pair_files(&old, &new);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "a.bin");
        assert_eq!(result[0].1, FileStatus::Deleted);
        assert!(result[0].2.is_some());
        assert!(result[0].3.is_none());
    }

    #[test]
    fn changed_file_hash_is_modified() {
        let mut old = BTreeMap::new();
        old.insert("model.bin".to_owned(), make_pointer(0x01, 100));
        let mut new = BTreeMap::new();
        new.insert("model.bin".to_owned(), make_pointer(0x02, 200));

        let result = pair_files(&old, &new);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "model.bin");
        assert_eq!(result[0].1, FileStatus::Modified);
        assert!(result[0].2.is_some());
        assert!(result[0].3.is_some());
    }

    #[test]
    fn unchanged_file_hash_is_skipped() {
        let ptr = make_pointer(0x01, 100);
        let mut old = BTreeMap::new();
        old.insert("same.bin".to_owned(), ptr.clone());
        let mut new = BTreeMap::new();
        new.insert("same.bin".to_owned(), ptr);

        let result = pair_files(&old, &new);
        assert!(result.is_empty());
    }

    #[test]
    fn mixed_operations_are_sorted_by_path() {
        let mut old = BTreeMap::new();
        old.insert("deleted.bin".to_owned(), make_pointer(0x01, 100));
        old.insert("modified.bin".to_owned(), make_pointer(0x02, 200));
        old.insert("unchanged.bin".to_owned(), make_pointer(0x03, 300));

        let mut new = BTreeMap::new();
        new.insert("added.bin".to_owned(), make_pointer(0x04, 400));
        new.insert("modified.bin".to_owned(), make_pointer(0x05, 500));
        new.insert("unchanged.bin".to_owned(), make_pointer(0x03, 300));

        let result = pair_files(&old, &new);
        let observed: Vec<(&str, FileStatus)> = result
            .iter()
            .map(|(path, status, _, _)| (path.as_str(), *status))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("added.bin", FileStatus::Added),
                ("deleted.bin", FileStatus::Deleted),
                ("modified.bin", FileStatus::Modified),
            ]
        );
    }

    #[test]
    fn result_paths_are_sorted() {
        let mut old = BTreeMap::new();
        old.insert("z.bin".to_owned(), make_pointer(0x01, 100));
        old.insert("a.bin".to_owned(), make_pointer(0x02, 200));

        let mut new = BTreeMap::new();
        new.insert("m.bin".to_owned(), make_pointer(0x03, 300));

        let result = pair_files(&old, &new);
        let paths: Vec<&str> = result.iter().map(|(p, _, _, _)| p.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }
}
