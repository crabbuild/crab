use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use crab_metadata::commit_graph::CommitGraphSummary;
use crab_metadata::manifests::{Manifest, manifest_reachable_objects};

use crate::hidden_refs;

/// A single object request from a fetch batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchWant {
    pub sha: String,
    pub ref_name: String,
}

impl FetchWant {
    /// Creates a fetch-want request from a raw object id and ref target.
    #[must_use]
    pub fn new(sha: impl Into<String>, ref_name: impl Into<String>) -> Self {
        Self {
            sha: sha.into(),
            ref_name: ref_name.into(),
        }
    }
}

/// Upload-pack admission policy for raw object wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchAdmissionPolicy {
    pub allow_any_sha_in_want: bool,
    pub allow_tip_sha_in_want: bool,
    pub allow_reachable_sha_in_want: bool,
    pub transfer_hide_refs: Vec<String>,
}

impl Default for FetchAdmissionPolicy {
    fn default() -> Self {
        Self {
            allow_any_sha_in_want: false,
            allow_tip_sha_in_want: true,
            allow_reachable_sha_in_want: false,
            transfer_hide_refs: Vec::new(),
        }
    }
}

/// Reason a fetch want is denied by read-side admission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchAdmissionReject {
    NotReachable { sha: String },
    NotAtTip { sha: String },
    NotAllowed { sha: String, reason: String },
}

/// Validates raw fetch wants against the manifest and upload-pack policy.
///
/// Hidden refs are removed from the advertised tip and reachable sets before
/// object admission is evaluated.
#[must_use]
pub fn validate_fetch_wants_with_manifest(
    wants: &[FetchWant],
    manifest: &Manifest,
    summary: Option<&CommitGraphSummary>,
    policy: &FetchAdmissionPolicy,
) -> Vec<(FetchWant, std::result::Result<(), FetchAdmissionReject>)> {
    let hidden_refs = hidden_refs::compile(&policy.transfer_hide_refs);
    let visible_manifest = visible_manifest(manifest, &hidden_refs, policy);
    let tip_set: HashSet<&str> = visible_manifest.refs.values().map(String::as_str).collect();
    let mut reachable_set: Option<HashSet<String>> = None;

    wants
        .iter()
        .map(|want| {
            if hidden_refs.is_match(&want.ref_name) {
                return (
                    want.clone(),
                    Err(FetchAdmissionReject::NotAllowed {
                        sha: want.sha.clone(),
                        reason: "hidden-ref target".to_owned(),
                    }),
                );
            }

            if policy.allow_any_sha_in_want {
                return (want.clone(), Ok(()));
            }

            if policy.allow_tip_sha_in_want && tip_set.contains(want.sha.as_str()) {
                return (want.clone(), Ok(()));
            }

            if policy.allow_reachable_sha_in_want {
                let set = reachable_set
                    .get_or_insert_with(|| manifest_reachable_objects(&visible_manifest, summary));
                if set.contains(&want.sha) {
                    return (want.clone(), Ok(()));
                }
                return (
                    want.clone(),
                    Err(FetchAdmissionReject::NotReachable {
                        sha: want.sha.clone(),
                    }),
                );
            }

            (
                want.clone(),
                Err(FetchAdmissionReject::NotAtTip {
                    sha: want.sha.clone(),
                }),
            )
        })
        .collect()
}

fn visible_manifest<'a>(
    manifest: &'a Manifest,
    hidden_refs: &globset::GlobSet,
    policy: &FetchAdmissionPolicy,
) -> Cow<'a, Manifest> {
    if policy.transfer_hide_refs.is_empty() {
        return Cow::Borrowed(manifest);
    }

    let visible_refs: BTreeMap<String, String> = manifest
        .refs
        .iter()
        .filter(|(name, _)| !hidden_refs.is_match(name.as_str()))
        .map(|(name, sha)| (name.clone(), sha.clone()))
        .collect();
    let mut manifest = manifest.clone();
    manifest.refs = visible_refs;
    Cow::Owned(manifest)
}

#[cfg(test)]
mod tests {
    use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};

    use super::*;

    fn manifest_with_refs(pairs: &[(&str, &str)]) -> Manifest {
        let refs = pairs
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect();
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    #[test]
    fn tip_sha_is_accepted_by_default_policy() {
        let tip = "a".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let wants = vec![FetchWant::new(&tip, "refs/heads/main")];

        let result =
            validate_fetch_wants_with_manifest(&wants, &manifest, None, &Default::default());

        assert_eq!(result.len(), 1);
        assert!(result[0].1.is_ok());
    }

    #[test]
    fn non_tip_sha_is_rejected_when_only_tips_are_allowed() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let wants = vec![FetchWant::new(&non_tip, "refs/heads/feature")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: false,
            allow_tip_sha_in_want: true,
            allow_reachable_sha_in_want: false,
            transfer_hide_refs: Vec::new(),
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, None, &policy);

        assert_eq!(
            result[0].1,
            Err(FetchAdmissionReject::NotAtTip { sha: non_tip })
        );
    }

    #[test]
    fn allow_any_sha_accepts_non_tip_wants() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let wants = vec![FetchWant::new(&non_tip, "refs/heads/feature")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: true,
            allow_tip_sha_in_want: false,
            allow_reachable_sha_in_want: false,
            transfer_hide_refs: Vec::new(),
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, None, &policy);

        assert!(result[0].1.is_ok());
    }

    #[test]
    fn reachable_sha_is_accepted_when_summary_proves_reachability() {
        let ancestor = "a".repeat(40);
        let tip = "b".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![
                CommitEntry {
                    oid: ancestor.clone(),
                    gen_number: 0,
                    parents: vec![],
                },
                CommitEntry {
                    oid: tip.clone(),
                    gen_number: 1,
                    parents: vec![ancestor.clone()],
                },
            ],
        };
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let wants = vec![FetchWant::new(&ancestor, "refs/heads/main")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: false,
            allow_tip_sha_in_want: true,
            allow_reachable_sha_in_want: true,
            transfer_hide_refs: Vec::new(),
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, Some(&summary), &policy);

        assert!(result[0].1.is_ok());
    }

    #[test]
    fn unreachable_sha_is_rejected_when_reachable_policy_is_required() {
        let tip = "a".repeat(40);
        let unknown = "f".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![CommitEntry {
                oid: tip.clone(),
                gen_number: 0,
                parents: vec![],
            }],
        };
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let wants = vec![FetchWant::new(&unknown, "refs/heads/feature")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: false,
            allow_tip_sha_in_want: true,
            allow_reachable_sha_in_want: true,
            transfer_hide_refs: Vec::new(),
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, Some(&summary), &policy);

        assert_eq!(
            result[0].1,
            Err(FetchAdmissionReject::NotReachable { sha: unknown })
        );
    }

    #[test]
    fn hidden_ref_target_is_rejected_before_allow_any_policy() {
        let hidden = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/secret", &hidden)]);
        let wants = vec![FetchWant::new(&hidden, "refs/heads/secret")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: true,
            allow_tip_sha_in_want: true,
            allow_reachable_sha_in_want: true,
            transfer_hide_refs: vec!["refs/heads/secret".to_owned()],
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, None, &policy);

        assert_eq!(
            result[0].1,
            Err(FetchAdmissionReject::NotAllowed {
                sha: hidden,
                reason: "hidden-ref target".to_owned()
            })
        );
    }

    #[test]
    fn hidden_refs_are_removed_from_reachable_set() {
        let hidden_ancestor = "a".repeat(40);
        let hidden_tip = "b".repeat(40);
        let visible_tip = "c".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![
                CommitEntry {
                    oid: hidden_ancestor.clone(),
                    gen_number: 0,
                    parents: vec![],
                },
                CommitEntry {
                    oid: hidden_tip.clone(),
                    gen_number: 1,
                    parents: vec![hidden_ancestor.clone()],
                },
                CommitEntry {
                    oid: visible_tip.clone(),
                    gen_number: 1,
                    parents: vec![],
                },
            ],
        };
        let manifest = manifest_with_refs(&[
            ("refs/heads/main", &visible_tip),
            ("refs/heads/secret", &hidden_tip),
        ]);
        let wants = vec![FetchWant::new(&hidden_ancestor, "refs/heads/main")];
        let policy = FetchAdmissionPolicy {
            allow_any_sha_in_want: false,
            allow_tip_sha_in_want: false,
            allow_reachable_sha_in_want: true,
            transfer_hide_refs: vec!["refs/heads/secret".to_owned()],
        };

        let result = validate_fetch_wants_with_manifest(&wants, &manifest, Some(&summary), &policy);

        assert_eq!(
            result[0].1,
            Err(FetchAdmissionReject::NotReachable {
                sha: hidden_ancestor
            })
        );
    }
}
