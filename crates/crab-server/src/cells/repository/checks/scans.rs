use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::*;

const ROOT: &str = "collaboration/v1/check-scans";
pub(crate) const CHECK_PREFIX: &str = "Code scanning / ";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scan {
    pub tools: [u8; 32],
    pub complete: bool,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Finding {
    pub rule: Option<[u8; 32]>,
    pub fingerprint: Option<Fingerprint>,
    pub annotated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fingerprint {
    pub method: Method,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Method {
    PrimaryLocation,
    Full,
}

impl Scan {
    pub(super) fn validate(&self, input: &CreateCheckRunInput) -> crate::cells::Result<()> {
        if !input.name.starts_with(CHECK_PREFIX)
            || input.report.status != 2
            || !input.report.output.steps.is_empty()
            || self.findings.len() > MAX_CHECK_ANNOTATIONS
            || self
                .findings
                .iter()
                .any(|finding| finding.fingerprint.is_some() && finding.rule.is_none())
            || self
                .findings
                .iter()
                .filter(|finding| finding.annotated)
                .count()
                != input.report.output.annotations.len()
        {
            return Err(crate::cells::Error::Invalid(
                "Invalid code scanning check metadata",
            ));
        }
        Ok(())
    }
}

fn report_key(number: u64) -> String {
    format!("{ROOT}/reports/{number:016x}")
}

fn latest_key(head: &CheckRunRecord, oid: &str) -> String {
    let mut digest = blake3::Hasher::new();
    hash_text(&mut digest, &head.author.issuer);
    hash_text(&mut digest, &head.author.subject);
    hash_text(&mut digest, &head.name.to_lowercase());
    format!("{ROOT}/latest/{}/{oid}", digest.finalize().to_hex())
}

pub(super) fn save(
    context: &Transaction<'_>,
    run: &CheckRunRecord,
    scan: &Scan,
) -> crate::cells::Result<()> {
    // The report and latest pointer share the immutable check and its receipt.
    context.put_document(&report_key(run.number), scan)?;
    context.put_document(&latest_key(run, &run.oid), &run.number)
}

pub(super) fn load(context: &Transaction<'_>, number: u64) -> crate::cells::Result<Option<Scan>> {
    context.document(&report_key(number))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Comparison {
    Ready {
        introduced: usize,
        existing: usize,
        unclassified: usize,
        annotations: Vec<FindingState>,
    },
    Missing,
    Incomplete,
    ToolsChanged,
    Unreachable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingState {
    Introduced,
    Existing,
    Unclassified,
}

pub(super) fn compare(
    context: &Transaction<'_>,
    head: &CheckRunDetail,
    baseline: &str,
) -> crate::cells::Result<Option<Comparison>> {
    let Some(scan) = head.scan.as_ref() else {
        return Ok(None);
    };
    if !scan.complete {
        return Ok(Some(Comparison::Incomplete));
    }
    let Some(number) = context.document::<u64>(&latest_key(&head.run, baseline))? else {
        return Ok(Some(Comparison::Missing));
    };
    let base = load(context, number)?.ok_or(crate::cells::Error::Invalid(
        "Code scanning baseline has no report",
    ))?;
    Ok(Some(compare_findings(scan, &base)))
}

fn compare_findings(head: &Scan, base: &Scan) -> Comparison {
    #[derive(PartialEq, Default)]
    enum Coverage {
        #[default]
        Empty,
        Uniform(Method),
        Incomplete,
    }
    if !head.complete || !base.complete {
        return Comparison::Incomplete;
    }
    if head.tools != base.tools {
        return Comparison::ToolsChanged;
    }
    let mut base_counts = BTreeMap::new();
    let mut coverage = BTreeMap::new();
    let mut unknown_rule = false;
    for finding in &base.findings {
        let Some(rule) = finding.rule else {
            unknown_rule = true;
            continue;
        };
        let rule_coverage = coverage.entry(rule).or_default();
        if let Some(key) = finding.fingerprint {
            *base_counts.entry(key).or_insert(0) += 1;
            if *rule_coverage == Coverage::Empty {
                *rule_coverage = Coverage::Uniform(key.method);
            } else if *rule_coverage != Coverage::Uniform(key.method) {
                *rule_coverage = Coverage::Incomplete;
            }
        } else {
            *rule_coverage = Coverage::Incomplete;
        }
    }
    let mut introduced = 0;
    let mut existing = 0;
    let mut unclassified = 0;
    let mut annotations = Vec::new();
    let mut head_counts = BTreeMap::new();
    for key in head
        .findings
        .iter()
        .filter_map(|finding| finding.fingerprint)
    {
        *head_counts.entry(key).or_insert(0) += 1;
    }
    for finding in &head.findings {
        let state = match finding.fingerprint {
            Some(key) => {
                let count = base_counts.get(&key).copied().unwrap_or(0);
                if count >= head_counts.get(&key).copied().unwrap_or(1) {
                    FindingState::Existing
                } else if count == 0
                    && !unknown_rule
                    && finding.rule.as_ref().is_some_and(|rule| {
                        coverage
                            .get(rule)
                            .is_none_or(|value| *value == Coverage::Uniform(key.method))
                    })
                {
                    FindingState::Introduced
                } else {
                    FindingState::Unclassified
                }
            }
            None => FindingState::Unclassified,
        };
        match state {
            FindingState::Introduced => introduced += 1,
            FindingState::Existing => existing += 1,
            FindingState::Unclassified => unclassified += 1,
        }
        if finding.annotated {
            annotations.push(state);
        }
    }
    Comparison::Ready {
        introduced,
        existing,
        unclassified,
        annotations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(keys: &[Option<u8>]) -> Scan {
        Scan {
            tools: [1; 32],
            complete: true,
            findings: keys
                .iter()
                .map(|key| Finding {
                    rule: Some([1; 32]),
                    fingerprint: key.map(|value| Fingerprint {
                        method: Method::PrimaryLocation,
                        digest: [value; 32],
                    }),
                    annotated: true,
                })
                .collect(),
        }
    }

    #[test]
    fn ambiguous_duplicates_cannot_identify_which_finding_is_new() {
        let base = scan(&[Some(1), Some(2)]);
        let head = scan(&[Some(1), Some(1), Some(2), Some(3)]);
        assert_eq!(
            compare_findings(&head, &base),
            Comparison::Ready {
                introduced: 1,
                existing: 1,
                unclassified: 2,
                annotations: vec![
                    FindingState::Unclassified,
                    FindingState::Unclassified,
                    FindingState::Existing,
                    FindingState::Introduced
                ],
            }
        );
    }

    #[test]
    fn partial_baseline_coverage_cannot_prove_an_unmatched_finding_is_new() {
        let base = scan(&[Some(1), None]);
        let head = scan(&[Some(1), Some(2)]);
        assert_eq!(
            compare_findings(&head, &base),
            Comparison::Ready {
                introduced: 0,
                existing: 1,
                unclassified: 1,
                annotations: vec![FindingState::Existing, FindingState::Unclassified],
            }
        );
    }

    #[test]
    fn incomplete_evidence_for_another_rule_preserves_classification() {
        let mut base = scan(&[None]);
        base.findings[0].rule = Some([2; 32]);
        let head = scan(&[Some(1)]);
        assert_eq!(
            compare_findings(&head, &base),
            Comparison::Ready {
                introduced: 1,
                existing: 0,
                unclassified: 0,
                annotations: vec![FindingState::Introduced],
            }
        );
    }
}
