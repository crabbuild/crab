use super::*;
use crate::cells::repository::MergeMethod;

const KEY: &str = "repository/v1/merge-methods";
const MAX_QUEUE_BUILDS: u8 = 100;
const MAX_QUEUE_BRANCHES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergeMethods {
    pub version: u64,
    pub enabled: Vec<MergeMethod>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_merge: bool,
    #[serde(default, skip_serializing_if = "zero")]
    pub auto_merge_generation: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge_queue: bool,
    #[serde(default, skip_serializing_if = "zero")]
    pub merge_queue_generation: u64,
    #[serde(
        default = "default_queue_method",
        skip_serializing_if = "uses_default_queue_method"
    )]
    pub merge_queue_method: MergeMethod,
    #[serde(default, skip_serializing_if = "zero")]
    pub merge_queue_method_revision: u64,
    #[serde(default = "single_build", skip_serializing_if = "is_single_build")]
    pub merge_queue_builds: u8,
    #[serde(
        default = "default_timeout",
        skip_serializing_if = "uses_default_timeout"
    )]
    pub merge_queue_timeout_minutes: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_queue_branches: Vec<BranchQueue>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchQueue {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u16>,
}

impl BranchQueue {
    fn valid(&self) -> bool {
        !self.branch.starts_with("refs/")
            && self.branch.len() <= 255
            && crab_git::validate_push_refname(&format!("refs/heads/{}", self.branch)).is_ok()
            && (self.method.is_some() || self.builds.is_some() || self.timeout_minutes.is_some())
            && self.method.is_none_or(MergeMethod::supports_queue)
            && self.builds.is_none_or(valid_builds)
            && self.timeout_minutes.is_none_or(valid_timeout)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceMergeMethodsInput {
    pub expected_version: u64,
    pub enabled: Vec<MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_method: Option<MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_builds: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_timeout_minutes: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_branches: Option<Vec<BranchQueue>>,
}

impl ReplaceMergeMethodsInput {
    pub(crate) fn valid(&self) -> bool {
        self.expected_version < MAX_NUMBER
            && !self.enabled.is_empty()
            && self.enabled.len() <= MergeMethod::ALL.len()
            && self
                .merge_queue_method
                .is_none_or(MergeMethod::supports_queue)
            && self.merge_queue_builds.is_none_or(valid_builds)
            && self.merge_queue_timeout_minutes.is_none_or(valid_timeout)
            && self.merge_queue_branches.as_ref().is_none_or(|branches| {
                let mut names = std::collections::HashSet::new();
                branches.len() <= MAX_QUEUE_BRANCHES
                    && branches
                        .iter()
                        .all(|entry| entry.valid() && names.insert(&entry.branch))
            })
            && self
                .enabled
                .iter()
                .enumerate()
                .all(|(i, method)| !self.enabled[..i].contains(method))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum ReplaceMergeMethodsOutcome {
    Saved(MergeMethods),
    Conflict,
    Archived,
}

pub(in crate::cells::repository) fn load(
    context: &Transaction<'_>,
) -> crate::cells::Result<MergeMethods> {
    Ok(context.document(KEY)?.unwrap_or_else(|| MergeMethods {
        version: 0,
        enabled: MergeMethod::ALL.to_vec(),
        auto_merge: false,
        auto_merge_generation: 0,
        merge_queue: false,
        merge_queue_generation: 0,
        merge_queue_method: default_queue_method(),
        merge_queue_method_revision: 0,
        merge_queue_builds: single_build(),
        merge_queue_timeout_minutes: default_timeout(),
        merge_queue_branches: vec![],
    }))
}

fn method_revision_key(branch: &str) -> String {
    let digest = blake3::hash(branch.as_bytes()).to_hex();
    format!("repository/v1/queue-method-revision/{digest}")
}

pub(in crate::cells::repository) fn load_for_ref(
    context: &Transaction<'_>,
    reference: &str,
) -> crate::cells::Result<MergeMethods> {
    let branch = reference
        .strip_prefix("refs/heads/")
        .ok_or(crate::cells::Error::Invalid(
            "expected a queue target branch",
        ))?;
    let mut effective = load(context)?;
    let revision = context
        .document::<u64>(&method_revision_key(branch))?
        .unwrap_or(0);
    let override_ = effective
        .merge_queue_branches
        .iter()
        .find(|entry| entry.branch == branch);
    if let Some(method) = override_.and_then(|entry| entry.method) {
        effective.merge_queue_method = method;
        effective.merge_queue_method_revision = revision;
    } else {
        effective.merge_queue_method_revision = effective.merge_queue_method_revision.max(revision);
    }
    if let Some(entry) = override_ {
        effective.merge_queue_builds = entry.builds.unwrap_or(effective.merge_queue_builds);
        effective.merge_queue_timeout_minutes = entry
            .timeout_minutes
            .unwrap_or(effective.merge_queue_timeout_minutes);
    }
    effective.merge_queue_branches.clear();
    Ok(effective)
}

pub(crate) struct GetEffectiveMergeMethods;
impl Read for GetEffectiveMergeMethods {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 423;
    type Input = String;
    type Output = MergeMethods;

    fn execute(
        context: &mut Transaction<'_>,
        reference: String,
    ) -> crate::cells::Result<Self::Output> {
        load_for_ref(context, &reference)
    }
}

pub(crate) struct GetMergeMethods;
impl Read for GetMergeMethods {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 408;
    type Input = ();
    type Output = MergeMethods;

    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Self::Output> {
        load(context)
    }
}

pub(crate) struct ReplaceMergeMethods;
impl Mutation for ReplaceMergeMethods {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 408;
    type Input = ReplaceMergeMethodsInput;
    type Output = ReplaceMergeMethodsOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        use ReplaceMergeMethodsOutcome::{Archived, Conflict, Saved};
        if !input.valid() {
            return Err(crate::cells::Error::Invalid(
                "invalid repository merge methods",
            ));
        }
        if archived(context)? {
            return Ok(MutationResult::Rejected(Archived));
        }
        let current = load(context)?;
        if current.version != input.expected_version || current.version >= MAX_NUMBER - 1 {
            return Ok(MutationResult::Rejected(Conflict));
        }
        let enabled = MergeMethod::ALL
            .into_iter()
            .filter(|method| input.enabled.contains(method))
            .collect();
        let auto_merge = input.auto_merge.unwrap_or(current.auto_merge);
        let merge_queue = input.merge_queue.unwrap_or(current.merge_queue);
        let merge_queue_method = input
            .merge_queue_method
            .unwrap_or(current.merge_queue_method);
        let merge_queue_builds = input
            .merge_queue_builds
            .unwrap_or(current.merge_queue_builds);
        let merge_queue_timeout_minutes = input
            .merge_queue_timeout_minutes
            .unwrap_or(current.merge_queue_timeout_minutes);
        let mut branches = input
            .merge_queue_branches
            .unwrap_or_else(|| current.merge_queue_branches.clone());
        branches.sort_unstable_by(|a, b| a.branch.cmp(&b.branch));
        if current.enabled == enabled
            && current.auto_merge == auto_merge
            && current.merge_queue == merge_queue
            && current.merge_queue_method == merge_queue_method
            && current.merge_queue_builds == merge_queue_builds
            && current.merge_queue_timeout_minutes == merge_queue_timeout_minutes
            && current.merge_queue_branches == branches
        {
            return Ok(MutationResult::Success(Saved(current)));
        }
        let methods = |entries: &[BranchQueue]| {
            entries
                .iter()
                .filter_map(|entry| entry.method.map(|method| (entry.branch.clone(), method)))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let previous_methods = methods(&current.merge_queue_branches);
        let current_methods = methods(&branches);
        // Keep a revision even after an override is removed. Otherwise an
        // older entry could regain its original default and become eligible again.
        for (branch, method) in &previous_methods {
            if current_methods.get(branch) != Some(method) {
                context.put_document(&method_revision_key(branch), &(current.version + 1))?;
            }
        }
        for branch in current_methods
            .keys()
            .filter(|branch| !previous_methods.contains_key(*branch))
        {
            context.put_document(&method_revision_key(branch), &(current.version + 1))?;
        }
        let updated = MergeMethods {
            version: current.version + 1,
            enabled,
            auto_merge,
            merge_queue,
            merge_queue_method,
            merge_queue_builds,
            merge_queue_timeout_minutes,
            merge_queue_branches: branches,
            merge_queue_method_revision: if merge_queue_method == current.merge_queue_method {
                current.merge_queue_method_revision
            } else {
                current.version + 1
            },
            merge_queue_generation: if merge_queue == current.merge_queue {
                current.merge_queue_generation
            } else {
                current.version + 1
            },
            auto_merge_generation: if auto_merge == current.auto_merge {
                current.auto_merge_generation
            } else {
                current.version + 1
            },
        };
        context.put_document(KEY, &updated)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Saved(updated)))
    }
}

fn default_queue_method() -> MergeMethod {
    MergeMethod::MergeCommit
}

fn uses_default_queue_method(method: &MergeMethod) -> bool {
    *method == default_queue_method()
}

fn zero(value: &u64) -> bool {
    *value == 0
}

fn single_build() -> u8 {
    1
}

fn is_single_build(count: &u8) -> bool {
    *count == single_build()
}

fn valid_builds(count: u8) -> bool {
    (1..=MAX_QUEUE_BUILDS).contains(&count)
}

fn default_timeout() -> u16 {
    60
}

fn uses_default_timeout(minutes: &u16) -> bool {
    *minutes == default_timeout()
}

fn valid_timeout(minutes: u16) -> bool {
    (1..=720).contains(&minutes)
}
