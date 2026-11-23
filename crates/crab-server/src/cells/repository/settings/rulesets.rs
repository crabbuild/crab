use std::collections::HashSet;

use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::*;

const KEY: &str = "repository/v1/rulesets";
pub(crate) const MAX_RULESETS: usize = 75;
const MAX_RULESET_BYTES: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum RulesetId {
    Repository(Uuid),
    Organization { organization: Uuid, ruleset: Uuid },
}

pub(crate) fn valid_bypass_ids(ids: &[RulesetId]) -> bool {
    ids.len() <= 2 * MAX_RULESETS
        && ids.iter().all(|id| match id {
            RulesetId::Repository(id) => !id.is_nil(),
            RulesetId::Organization {
                organization,
                ruleset,
            } => !organization.is_nil() && !ruleset.is_nil(),
        })
        && ids.windows(2).all(|pair| pair[0] < pair[1])
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Branch,
    Tag,
}

impl Target {
    fn prefix(self) -> &'static str {
        match self {
            Self::Branch => "refs/heads/",
            Self::Tag => "refs/tags/",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Enforcement {
    Active,
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum BypassActor {
    RepositoryAdmin,
    RepositoryMaintain,
    RepositoryWrite,
    OrganizationOwner,
    Team { id: Uuid },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BypassMode {
    Always,
    PullRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BypassGrant {
    pub actor: BypassActor,
    pub mode: BypassMode,
}

#[derive(Clone, Copy)]
pub(crate) struct BypassContext<'a> {
    pub role: Option<crate::RepositoryAccess>,
    pub organization_owner: bool,
    pub teams: &'a [Uuid],
    pub organization_teams: &'a [Uuid],
}

impl<'a> BypassContext<'a> {
    pub(crate) fn organization_teams(mut self, teams: Option<&'a [Uuid]>) -> Self {
        if let Some(teams) = teams {
            self.organization_teams = teams;
        }
        self
    }

    pub(crate) fn for_rule(mut self, id: RulesetId) -> Self {
        if matches!(id, RulesetId::Organization { .. }) {
            self.teams = self.organization_teams;
        }
        self
    }

    pub(crate) fn allows(self, grants: &[BypassGrant], pull: bool) -> bool {
        use crate::RepositoryAccess::{Admin, Maintain, Write};

        if !self.role.is_some_and(|role| role >= Write) {
            return false;
        }
        grants.iter().any(|grant| {
            (pull || grant.mode == BypassMode::Always)
                && match grant.actor {
                    BypassActor::RepositoryAdmin => self.role == Some(Admin),
                    BypassActor::RepositoryMaintain => self.role == Some(Maintain),
                    BypassActor::RepositoryWrite => self.role == Some(Write),
                    BypassActor::OrganizationOwner => self.organization_owner,
                    BypassActor::Team { id } => self.teams.contains(&id),
                }
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ruleset {
    pub id: Uuid,
    pub name: String,
    pub target: Target,
    pub enforcement: Enforcement,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub restrict_creations: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restrict_updates: bool,
    pub restrict_deletions: bool,
    pub require_pull_request: bool,
    pub required_approvals: u8,
    pub required_code_owner_reviews: bool,
    #[serde(default)]
    pub require_conversation_resolution: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_up_to_date: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_merge_queue: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_linear_history: bool,
    pub required_checks: Vec<String>,
    pub required_integrations: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass_actors: Vec<BypassGrant>,
}

impl Ruleset {
    pub(crate) fn requires_pull(&self) -> bool {
        self.require_pull_request
            || self.require_merge_queue
            || self.required_approvals > 0
            || self.required_code_owner_reviews
            || self.require_conversation_resolution
            || !self.required_checks.is_empty()
            || !self.required_integrations.is_empty()
    }

    pub(crate) fn allows_bypass(&self, context: BypassContext<'_>, pull: bool) -> bool {
        context.allows(&self.bypass_actors, pull)
    }

    pub(crate) fn violation(
        &self,
        update: &crab_git::receive_plan::RefUpdate,
        pull: bool,
    ) -> Option<&'static str> {
        if self.restrict_creations && update.old.is_none() {
            return Some("Repository ruleset restricts ref creation");
        }
        if self.restrict_deletions && update.new.is_none() {
            return Some("Repository ruleset restricts ref deletion");
        }
        // PR publication validates its explicitly selected bypasses against
        // current branch policy when authorizing the exact merge reservation.
        if !pull && self.restrict_updates && update.old.is_some() && update.new.is_some() {
            return Some("Repository ruleset restricts ref updates");
        }
        if !pull && update.old.is_some() && self.require_merge_queue {
            return Some("Repository ruleset requires the merge queue");
        }
        if !pull && update.old.is_some() && update.new.is_some() && self.requires_pull() {
            return Some("Repository ruleset requires a pull request");
        }
        None
    }

    pub(in crate::cells) fn valid(&self) -> bool {
        let mut actors = HashSet::new();
        !self.id.is_nil()
            && self.bypass_actors.len() <= 64
            && self.bypass_actors.iter().all(|grant| {
                actors.insert(grant.actor)
                    && (self.target == Target::Branch || grant.mode == BypassMode::Always)
                    && !matches!(grant.actor, BypassActor::Team { id } if id.is_nil())
            })
            && !self.name.is_empty()
            && self.name.len() <= 100
            && self.name.trim() == self.name
            && !self.name.chars().any(char::is_control)
            && (!self.requires_pull() || self.target == Target::Branch)
            && (self.requires_pull()
                || self.restrict_creations
                || self.restrict_updates
                || self.require_linear_history
                || self.restrict_deletions)
            && !self.include.is_empty()
            && [&self.include, &self.exclude].into_iter().all(|patterns| {
                let mut unique = HashSet::new();
                patterns.len() <= 20
                    && patterns.iter().all(|pattern| {
                        unique.insert(pattern) && Pattern::parse(pattern, self.target).is_some()
                    })
            })
            && crate::config::valid_protection_requirements(
                self.required_approvals,
                self.require_up_to_date || self.require_merge_queue,
                &self.required_checks,
                &self.required_integrations,
            )
    }
}

pub(in crate::cells) enum Pattern {
    All,
    DefaultBranch,
    Glob(Regex),
}

impl Pattern {
    pub(in crate::cells) fn parse(pattern: &str, target: Target) -> Option<Self> {
        match pattern {
            "~ALL" => return Some(Self::All),
            "~DEFAULT_BRANCH" => return (target == Target::Branch).then_some(Self::DefaultBranch),
            _ => {}
        }
        if pattern.is_empty()
            || pattern.len() > 256
            || pattern.starts_with("refs/")
            || pattern
                .chars()
                .any(|value| value.is_control() || matches!(value, ' ' | '\\' | ':' | '~' | '^'))
            || pattern.split('/').any(str::is_empty)
        {
            return None;
        }
        // Match a whole short ref name. Only a full globstar segment can
        // span directories, so release/* leaves release/team/1 unaffected.
        let mut regex = String::from(r"\A");
        let mut segments = pattern.split('/').peekable();
        while let Some(segment) = segments.next() {
            if segment == "**" {
                regex.push_str(if segments.peek().is_some() {
                    "(?:[^/]+/)*"
                } else {
                    ".*"
                });
                continue;
            }
            let mut remaining = segment.chars();
            while let Some(value) = remaining.next() {
                match value {
                    '*' => regex.push_str("[^/]*"),
                    '?' => regex.push_str("[^/]"),
                    '[' => {
                        let end = remaining.as_str().find(']')?;
                        let (characters, suffix) = remaining.as_str().split_at(end);
                        if characters.is_empty()
                            || !characters
                                .chars()
                                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
                        {
                            return None;
                        }
                        regex.push('[');
                        regex.push_str(characters);
                        regex.push(']');
                        remaining = suffix[1..].chars();
                    }
                    ']' => return None,
                    ch => regex.push_str(&regex::escape(&ch.to_string())),
                }
            }
            if segments.peek().is_some() {
                regex.push('/');
            }
        }
        regex.push_str(r"\z");
        Regex::new(&regex).ok().map(Self::Glob)
    }

    pub(in crate::cells) fn matches(&self, short: &str, is_default: bool) -> bool {
        match self {
            Self::All => true,
            Self::DefaultBranch => is_default,
            Self::Glob(glob) => glob.is_match(short),
        }
    }
}

struct Active<'a> {
    ruleset: &'a Ruleset,
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

pub(crate) struct ActiveRulesets<'a>(Vec<Active<'a>>);

impl<'a> ActiveRulesets<'a> {
    pub(crate) fn new(settings: &'a Rulesets) -> crate::cells::Result<Self> {
        let mut entries = Vec::new();
        for ruleset in &settings.rulesets {
            if ruleset.enforcement == Enforcement::Disabled {
                continue;
            }
            let parse = |patterns: &[String]| {
                patterns
                    .iter()
                    .map(|pattern| {
                        Pattern::parse(pattern, ruleset.target).ok_or(crate::cells::Error::Invalid(
                            "Invalid saved ruleset pattern",
                        ))
                    })
                    .collect::<crate::cells::Result<Vec<_>>>()
            };
            entries.push(Active {
                ruleset,
                include: parse(&ruleset.include)?,
                exclude: parse(&ruleset.exclude)?,
            });
        }
        Ok(Self(entries))
    }

    pub(crate) fn matches(
        &self,
        reference: &str,
        default_ref: &str,
    ) -> impl Iterator<Item = &'a Ruleset> {
        self.0.iter().filter_map(move |entry| {
            let short = reference.strip_prefix(entry.ruleset.target.prefix())?;
            let matches = |pattern: &Pattern| pattern.matches(short, reference == default_ref);
            (entry.include.iter().any(matches) && !entry.exclude.iter().any(matches))
                .then_some(entry.ruleset)
        })
    }

    pub(crate) fn requires_pull(&self, reference: &str, default_ref: &str) -> bool {
        self.matches(reference, default_ref)
            .any(Ruleset::requires_pull)
    }

    pub(crate) fn approval_count(&self, reference: &str, default_ref: &str) -> u8 {
        self.matches(reference, default_ref)
            .map(|rule| rule.required_approvals)
            .max()
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rulesets {
    pub version: u64,
    pub rulesets: Vec<Ruleset>,
}

impl Rulesets {
    pub(crate) fn uses_default_branch(&self) -> bool {
        self.rulesets.iter().any(|rule| {
            rule.enforcement == Enforcement::Active
                && rule
                    .include
                    .iter()
                    .chain(&rule.exclude)
                    .any(|value| value == "~DEFAULT_BRANCH")
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceRulesetsInput {
    pub expected_version: u64,
    pub rulesets: Vec<Ruleset>,
}

impl ReplaceRulesetsInput {
    pub(crate) fn valid(&self) -> bool {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        self.expected_version < MAX_NUMBER
            && self.rulesets.len() <= MAX_RULESETS
            && serde_json::to_vec(&self.rulesets)
                .is_ok_and(|bytes| bytes.len() <= MAX_RULESET_BYTES)
            && self.rulesets.iter().all(|rule| {
                ids.insert(rule.id) && names.insert(rule.name.to_lowercase()) && rule.valid()
            })
    }
}

fn load(context: &Transaction<'_>) -> crate::cells::Result<Rulesets> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

pub(crate) struct GetRulesets;
impl Read for GetRulesets {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 409;
    type Input = ();
    type Output = Rulesets;

    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Self::Output> {
        load(context)
    }
}

pub(crate) struct GetRefPolicy;
impl Read for GetRefPolicy {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 410;
    type Input = ();
    type Output = (Option<BranchProtectionSettings>, Rulesets);

    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Self::Output> {
        Ok((GetBranchProtections::execute(context, ())?, load(context)?))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ReplaceRulesetsOutcome {
    Saved(Rulesets),
    Conflict,
    Archived,
}

pub(crate) struct ReplaceRulesets;
impl Mutation for ReplaceRulesets {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 409;
    type Input = ReplaceRulesetsInput;
    type Output = ReplaceRulesetsOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        use ReplaceRulesetsOutcome::{Archived, Conflict, Saved};
        if !input.valid() {
            return Err(crate::cells::Error::Invalid("Invalid repository rulesets"));
        }
        if archived(context)? {
            return Ok(MutationResult::Rejected(Archived));
        }
        let current = load(context)?;
        if current.version != input.expected_version || current.version >= MAX_NUMBER - 1 {
            return Ok(MutationResult::Rejected(Conflict));
        }
        if current.rulesets == input.rulesets {
            return Ok(MutationResult::Success(Saved(current)));
        }
        let updated = Rulesets {
            version: current.version + 1,
            rulesets: input.rulesets,
        };
        context.put_document(KEY, &updated)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Saved(updated)))
    }
}
