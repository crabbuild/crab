/// Finite aggregate work ceilings for one read operation.
///
/// Every field must be nonzero. Apply with `ReadOptions::with_limits` to validate.
/// Per-object safety limits remain independently enforced by the Git owner.
/// Storage limits include Store retries and, for Crab-built cloud providers,
/// provider retries, listing pages and response-body bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// Logical Git objects visited, including semantic cache hits; default: 10,000.
    pub max_logical_objects: u64,
    /// Storage requests admitted by the read owner; default: 20,000.
    pub max_storage_requests: u64,
    /// Encoded bytes fetched by the read owner; default: 512 MiB.
    pub max_fetched_bytes: u64,
    /// Decoded Git bytes; default: 512 MiB.
    pub max_inflated_bytes: u64,
    /// Aggregate traversal depth work; default: 1,024.
    pub max_depth: u64,
    /// Tree entries visited; default: 100,000.
    pub max_entries: u64,
    /// History commits visited; default: 10,000.
    pub max_history_commits: u64,
    /// Bytes admitted as diff input; default: 32 MiB.
    pub max_diff_input_bytes: u64,
    /// Bytes emitted as diff output; default: 8 MiB.
    pub max_diff_output_bytes: u64,
    /// Lines admitted for attribution; default: 100,000.
    pub max_blame_lines: u64,
    /// Conservative line comparison work; default: 4,000,000.
    pub max_blame_comparison_cells: u64,
    /// Entries visited by archive traversal; default: 100,000.
    pub max_archive_entries: u64,
    /// Bytes admitted by archive traversal; default: 2 GiB.
    pub max_archive_bytes: u64,
    /// Bytes admitted as the operation response; default: 2 GiB.
    pub max_response_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        let limits = crab_remote_git::OperationLimits::default();
        Self {
            max_logical_objects: limits.max_logical_objects,
            max_storage_requests: limits.max_storage_requests,
            max_fetched_bytes: limits.max_fetched_bytes,
            max_inflated_bytes: limits.max_inflated_bytes,
            max_depth: limits.max_depth,
            max_entries: limits.max_entries,
            max_history_commits: limits.max_history_commits,
            max_diff_input_bytes: limits.max_diff_input_bytes,
            max_diff_output_bytes: limits.max_diff_output_bytes,
            max_blame_lines: limits.max_blame_lines,
            max_blame_comparison_cells: limits.max_blame_comparison_cells,
            max_archive_entries: limits.max_archive_entries,
            max_archive_bytes: limits.max_archive_bytes,
            max_response_bytes: limits.max_response_bytes,
        }
    }
}

impl ReadLimits {
    pub(crate) fn into_owner(self) -> crab_remote_git::OperationLimits {
        crab_remote_git::OperationLimits {
            max_logical_objects: self.max_logical_objects,
            max_storage_requests: self.max_storage_requests,
            max_fetched_bytes: self.max_fetched_bytes,
            max_inflated_bytes: self.max_inflated_bytes,
            max_depth: self.max_depth,
            max_entries: self.max_entries,
            max_history_commits: self.max_history_commits,
            max_diff_input_bytes: self.max_diff_input_bytes,
            max_diff_output_bytes: self.max_diff_output_bytes,
            max_blame_lines: self.max_blame_lines,
            max_blame_comparison_cells: self.max_blame_comparison_cells,
            max_archive_entries: self.max_archive_entries,
            max_archive_bytes: self.max_archive_bytes,
            max_response_bytes: self.max_response_bytes,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorKind, ReadOptions};

    #[test]
    fn defaults_match_documented_owner_limits() {
        let expected = ReadLimits {
            max_logical_objects: 10_000,
            max_storage_requests: 20_000,
            max_fetched_bytes: 512 * 1024 * 1024,
            max_inflated_bytes: 512 * 1024 * 1024,
            max_depth: 1_024,
            max_entries: 100_000,
            max_history_commits: 10_000,
            max_diff_input_bytes: 32 * 1024 * 1024,
            max_diff_output_bytes: 8 * 1024 * 1024,
            max_blame_lines: 100_000,
            max_blame_comparison_cells: 4_000_000,
            max_archive_entries: 100_000,
            max_archive_bytes: 2 * 1024 * 1024 * 1024,
            max_response_bytes: 2 * 1024 * 1024 * 1024,
        };
        assert_eq!(ReadLimits::default(), expected);
        assert_eq!(
            expected.into_owner(),
            crab_remote_git::OperationLimits::default()
        );
    }

    #[test]
    fn zero_limits_are_rejected_before_operation_admission() {
        let defaults = ReadLimits::default();
        let invalid = [
            ReadLimits {
                max_logical_objects: 0,
                ..defaults
            },
            ReadLimits {
                max_storage_requests: 0,
                ..defaults
            },
            ReadLimits {
                max_fetched_bytes: 0,
                ..defaults
            },
            ReadLimits {
                max_inflated_bytes: 0,
                ..defaults
            },
            ReadLimits {
                max_depth: 0,
                ..defaults
            },
            ReadLimits {
                max_entries: 0,
                ..defaults
            },
            ReadLimits {
                max_history_commits: 0,
                ..defaults
            },
            ReadLimits {
                max_diff_input_bytes: 0,
                ..defaults
            },
            ReadLimits {
                max_diff_output_bytes: 0,
                ..defaults
            },
            ReadLimits {
                max_blame_lines: 0,
                ..defaults
            },
            ReadLimits {
                max_blame_comparison_cells: 0,
                ..defaults
            },
            ReadLimits {
                max_archive_entries: 0,
                ..defaults
            },
            ReadLimits {
                max_archive_bytes: 0,
                ..defaults
            },
            ReadLimits {
                max_response_bytes: 0,
                ..defaults
            },
        ];
        for limits in invalid {
            let error = ReadOptions::default().with_limits(limits).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{limits:?}");
        }
    }

    #[test]
    fn invalid_durations_are_rejected_before_operation_admission() {
        for duration in [std::time::Duration::ZERO, std::time::Duration::MAX] {
            let error = ReadOptions::default().with_timeout(duration).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }
}
