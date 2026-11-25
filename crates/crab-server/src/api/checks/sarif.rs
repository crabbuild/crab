use std::collections::{BTreeMap, BTreeSet};

use url::Url;

use super::*;
mod history;

const MAX_UPLOAD_BYTES: usize = 1024 * 1024;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/check-runs/sarif", post(upload))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .merge(history::routes())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    request_id: String,
    head_sha: String,
    category: String,
    checkout_uri: Option<Url>,
    #[serde(default)]
    fail_on: FailOn,
    sarif: Log,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FailOn {
    #[default]
    Error,
    Warning,
}

#[derive(Deserialize)]
struct Log {
    version: String,
    runs: Vec<Run>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    tool: Tool,
    results: Option<Vec<Finding>>,
    #[serde(default)]
    invocations: Vec<Invocation>,
    #[serde(default)]
    artifacts: Vec<Artifact>,
    #[serde(default)]
    original_uri_base_ids: BTreeMap<String, ArtifactLocation>,
}

#[derive(Deserialize)]
struct Tool {
    driver: Driver,
}

#[derive(Deserialize)]
struct Driver {
    name: String,
    guid: Option<String>,
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    id: String,
    default_configuration: Option<Configuration>,
}

#[derive(Deserialize)]
struct Configuration {
    level: Option<Level>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invocation {
    execution_successful: bool,
    #[serde(default)]
    rule_configuration_overrides: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    rule_id: Option<String>,
    rule_index: Option<i64>,
    rule: Option<RuleReference>,
    kind: Option<Kind>,
    level: Option<Level>,
    baseline_state: Option<Baseline>,
    message: Message,
    #[serde(default)]
    fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    partial_fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    locations: Vec<Location>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleReference {
    id: Option<String>,
    index: Option<i64>,
    tool_component: Option<serde::de::IgnoredAny>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Level {
    None,
    Note,
    Warning,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "Error",
            Level::Warning => "Warning",
            Level::Note | Level::None => "Note",
        }
    }

    fn annotation(self) -> AnnotationLevel {
        match self {
            Level::Error => AnnotationLevel::Failure,
            Level::Warning => AnnotationLevel::Warning,
            Level::None | Level::Note => AnnotationLevel::Notice,
        }
    }
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Fail,
    Pass,
    Open,
    Review,
    NotApplicable,
    Informational,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Baseline {
    New,
    Unchanged,
    Updated,
    Absent,
}

#[derive(Deserialize)]
struct Message {
    text: Option<String>,
    markdown: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    physical_location: Option<PhysicalLocation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhysicalLocation {
    artifact_location: Option<ArtifactLocation>,
    region: Option<Region>,
}

#[derive(Deserialize)]
struct Artifact {
    location: Option<ArtifactLocation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactLocation {
    uri: Option<String>,
    uri_base_id: Option<String>,
    index: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Region {
    start_line: Option<u64>,
    end_line: Option<u64>,
}

async fn upload(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(scope): Query<statuses::CommitScope>,
    input: std::result::Result<Json<Upload>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(repo.id) {
        return Err(Error::CheckPermission);
    }
    let Json(input) = input?;
    create_run(&server, &principal, &repo, scope, input.into_check()?).await
}

impl Upload {
    fn into_check(self) -> Result<NewCheckRun> {
        validate_plain(
            &self.category,
            84,
            "Scan categories must contain 1–84 characters without controls",
        )?;
        if self.sarif.version != "2.1.0" || self.sarif.runs.is_empty() {
            return Err(Error::Invalid("Upload SARIF 2.1.0 with at least one run"));
        }
        let checkout = self.checkout_uri.as_ref().map(checkout_root).transpose()?;
        let mut errors = 0;
        let mut warnings = 0;
        let mut notes = 0;
        let mut failed_runs = 0;
        let mut annotations = Vec::new();
        let mut unlocated = String::new();
        let mut evidence = Vec::new();
        let mut tools = BTreeSet::new();
        for run in &self.sarif.runs {
            let driver = &run.tool.driver;
            validate_plain(&driver.name, 200, "A SARIF run requires a valid tool name")?;
            tools.insert((&driver.name, &driver.guid));
            let failed = run.invocations.iter().any(|row| !row.execution_successful);
            let configured = run
                .invocations
                .iter()
                .any(|row| !row.rule_configuration_overrides.is_empty());
            failed_runs += usize::from(failed);
            let Some(findings) = &run.results else {
                if failed {
                    continue;
                }
                return Err(Error::Invalid(
                    "A successful SARIF run requires a results array",
                ));
            };
            let inferred = run
                .original_uri_base_ids
                .get("%SRCROOT%")
                .and_then(|source| source.uri.as_deref())
                .and_then(|uri| Url::parse(uri).ok())
                .and_then(|uri| checkout_root(&uri).ok());
            let root = checkout.as_ref().or(inferred.as_ref());
            for finding in findings {
                if finding
                    .kind
                    .as_ref()
                    .is_some_and(|kind| *kind != Kind::Fail)
                {
                    if finding.level.is_some_and(|level| level != Level::None) {
                        return Err(Error::Invalid(
                            "A non-failing SARIF result cannot report a finding severity",
                        ));
                    }
                    continue;
                }
                if finding.baseline_state == Some(Baseline::Absent) {
                    continue;
                }
                let reference = finding.rule.as_ref();
                let external_rule = reference.is_some_and(|rule| rule.tool_component.is_some());
                if finding.level.is_none() && (external_rule || configured) {
                    return Err(Error::Invalid(
                        "Report explicit SARIF severity when using tool components or invocation overrides",
                    ));
                }
                let id = reference
                    .and_then(|rule| rule.id.as_deref())
                    .or(finding.rule_id.as_deref());
                let index = reference.and_then(|rule| rule.index).or(finding.rule_index);
                let rule = if external_rule {
                    None
                } else {
                    resolve_rule(driver, id, index)?
                };
                let level = finding
                    .level
                    .or_else(|| rule.and_then(|rule| rule.default_configuration.as_ref()?.level))
                    .unwrap_or(Level::Warning);
                match level {
                    Level::Error => errors += 1,
                    Level::Warning => warnings += 1,
                    Level::Note => notes += 1,
                    Level::None => {
                        return Err(Error::Invalid(
                            "SARIF findings require error, warning, or note severity",
                        ));
                    }
                }
                if errors + warnings + notes > MAX_ANNOTATIONS {
                    return Err(Error::Invalid(
                        "A SARIF check supports at most 50 current findings",
                    ));
                }
                let rule_id = id.or_else(|| rule.map(|rule| rule.id.as_str()));
                let title = rule_id.unwrap_or(&driver.name);
                validate_plain(title, 200, "A SARIF finding has an invalid rule identifier")?;
                let message = finding
                    .message
                    .text
                    .as_deref()
                    .or(finding.message.markdown.as_deref())
                    .ok_or(Error::Invalid("A SARIF finding requires an inline message"))?;
                validate_markdown(message, 4 * 1024, true)?;
                let location = finding
                    .locations
                    .iter()
                    .find_map(|location| location.annotation(run, root));
                let finding_rule = if external_rule {
                    None
                } else {
                    rule_id
                        .map(|rule| {
                            serde_json::to_vec(&(&driver.name, &driver.guid, rule))
                                .map(|data| *blake3::hash(&data).as_bytes())
                        })
                        .transpose()?
                };
                let fingerprint = fingerprint(
                    finding_rule,
                    finding,
                    location.as_ref().map(|item| item.0.as_str()),
                )?;
                evidence.push(check_scans::Finding {
                    rule: finding_rule,
                    fingerprint,
                    annotated: location.is_some(),
                });
                if let Some((path, start_line, end_line)) = location {
                    annotations.push(CheckAnnotation {
                        path,
                        start_line,
                        end_line,
                        level: level.annotation(),
                        title: Some(title.to_owned()),
                        message: message.to_owned(),
                    });
                } else {
                    unlocated.push_str(&format!("    {} · {title}\n", level.label()));
                    for line in message.lines() {
                        unlocated.push_str("    ");
                        unlocated.push_str(line);
                        unlocated.push('\n');
                    }
                    unlocated.push('\n');
                }
            }
        }
        let failed = failed_runs > 0
            || errors > 0
            || matches!(self.fail_on, FailOn::Warning) && warnings > 0;
        let threshold = match self.fail_on {
            FailOn::Error => "errors",
            FailOn::Warning => "errors or warnings",
        };
        let summary = format!(
            "Errors {errors} · Warnings {warnings} · Notes {notes}\n\n\
             Fails on {threshold}. {failed_runs} of {} scanner runs reported a failed execution.",
            self.sarif.runs.len()
        );
        Ok(NewCheckRun {
            request_id: self.request_id,
            head_sha: self.head_sha,
            name: format!("{}{}", check_scans::CHECK_PREFIX, self.category),
            status: CheckStatus::Completed,
            conclusion: Some(if failed {
                CheckConclusion::Failure
            } else {
                CheckConclusion::Success
            }),
            details_url: None,
            output: CheckOutput {
                title: "Code scanning results".into(),
                summary,
                text: (!unlocated.is_empty())
                    .then(|| format!("#### Findings without a source line\n\n{unlocated}")),
                steps: vec![],
                annotations,
            },
            scan: Some(check_scans::Scan {
                tools: *blake3::hash(&serde_json::to_vec(&tools)?).as_bytes(),
                complete: failed_runs == 0,
                findings: evidence,
            }),
        })
    }
}

fn fingerprint(
    rule: Option<[u8; 32]>,
    finding: &Finding,
    path: Option<&str>,
) -> Result<Option<check_scans::Fingerprint>> {
    let Some(rule) = rule else { return Ok(None) };
    let primary = finding
        .partial_fingerprints
        .get("primaryLocationLineHash")
        .filter(|value| !value.trim().is_empty());
    let (method, data) = if let Some((path, primary)) = path.zip(primary) {
        (
            check_scans::Method::PrimaryLocation,
            serde_json::to_vec(&(rule, path, primary))?,
        )
    } else if !finding.fingerprints.is_empty()
        && finding
            .fingerprints
            .iter()
            .all(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty())
    {
        (
            check_scans::Method::Full,
            serde_json::to_vec(&(rule, &finding.fingerprints))?,
        )
    } else {
        return Ok(None);
    };
    Ok(Some(check_scans::Fingerprint {
        method,
        digest: *blake3::hash(&data).as_bytes(),
    }))
}

fn resolve_rule<'a>(
    driver: &'a Driver,
    id: Option<&str>,
    index: Option<i64>,
) -> Result<Option<&'a Rule>> {
    if let Some(index) = index.filter(|index| *index != -1) {
        let rule = usize::try_from(index)
            .ok()
            .and_then(|index| driver.rules.get(index))
            .filter(|rule| id.is_none_or(|id| id == rule.id))
            .ok_or(Error::Invalid(
                "A SARIF rule index must match the tool rule",
            ))?;
        return Ok(Some(rule));
    }
    Ok(id.and_then(|id| driver.rules.iter().find(|rule| rule.id == id)))
}

fn checkout_root(uri: &Url) -> Result<Url> {
    if uri.scheme() != "file" || uri.has_host() || uri.query().is_some() || uri.fragment().is_some()
    {
        return Err(Error::Invalid(
            "A SARIF checkout URI must be a local file URI",
        ));
    }
    let mut root = uri.clone();
    if !root.path().ends_with('/') {
        root.path_segments_mut()
            .map_err(|_| Error::Invalid("A SARIF checkout URI must have a directory path"))?
            .push("");
    }
    Ok(root)
}

impl Location {
    fn annotation(&self, run: &Run, root: Option<&Url>) -> Option<(String, u64, u64)> {
        let location = self.physical_location.as_ref()?;
        let region = location.region.as_ref()?;
        let start = region.start_line?;
        let artifact = location.artifact_location.as_ref()?;
        let indexed = artifact
            .index
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| run.artifacts.get(index))
            .and_then(|artifact| artifact.location.as_ref());
        let uri = artifact
            .uri
            .as_deref()
            .or_else(|| indexed?.uri.as_deref())?;
        let base_id = artifact
            .uri_base_id
            .as_deref()
            .or_else(|| indexed?.uri_base_id.as_deref());
        let placeholder = Url::parse("file:///crab-sarif-root/").ok()?;
        let checkout = root.unwrap_or(&placeholder);
        let base = match base_id {
            None | Some("%SRCROOT%") => checkout.clone(),
            Some(id) => {
                let source = run.original_uri_base_ids.get(id)?;
                if source
                    .uri_base_id
                    .as_deref()
                    .is_some_and(|parent| parent != "%SRCROOT%")
                {
                    return None;
                }
                checkout.join(source.uri.as_deref()?).ok()?
            }
        };
        let file = base.join(uri).ok()?;
        if file.scheme() != "file"
            || file.has_host()
            || file.query().is_some()
            || file.fragment().is_some()
            || root.is_none() && (uri.starts_with('/') || Url::parse(uri).is_ok())
        {
            return None;
        }
        let path = file
            .path()
            .strip_prefix(checkout.path())?
            .split('/')
            .map(decode_segment)
            .collect::<Option<Vec<_>>>()?
            .join("/");
        Some((path, start, region.end_line.unwrap_or(start)))
    }
}

fn decode_segment(segment: &str) -> Option<String> {
    let mut source = segment.bytes();
    let mut decoded = Vec::with_capacity(segment.len());
    while let Some(byte) = source.next() {
        let byte = if byte == b'%' {
            let high = char::from(source.next()?).to_digit(16)?;
            let low = char::from(source.next()?).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()?
        } else {
            byte
        };
        if matches!(byte, b'/' | b'\\') || byte.is_ascii_control() {
            return None;
        }
        decoded.push(byte);
    }
    let text = String::from_utf8(decoded).ok()?;
    (!matches!(text.as_str(), "" | "." | "..")).then_some(text)
}

#[cfg(test)]
mod tests;
