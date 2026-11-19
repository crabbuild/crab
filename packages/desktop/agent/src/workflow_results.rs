use crate::{AgentError, Result, shellout};
use crab_workflow::results::{
    ArtifactPromotionRequest, LocalResults, Report, ReportDraft, ResultsRead, ResultsReader,
    ResultsResponse, RunSelection,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub repo_id: String,
    pub op_id: Option<String>,
    pub request: ResultsRead,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveParams {
    pub repo_id: String,
    pub draft: ReportDraft,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionParams {
    pub repo_id: String,
    pub action: String,
    pub run: Option<RunSelection>,
    pub report_id: Option<String>,
    pub branch: Option<String>,
    pub promotion: Option<ArtifactPromotionRequest>,
}

pub async fn save_report(root: &Path, draft: ReportDraft) -> Result<Value> {
    if draft.title.trim().is_empty() || draft.title.len() > 256 {
        return Err(AgentError::Other(
            "Report title must contain 1–256 characters".into(),
        ));
    }
    let local = LocalResults::new(root)?;
    let response = ResultsReader::Local(local.clone())
        .read(ResultsRead::Compare {
            runs: draft.runs,
            spec: draft.spec,
        })
        .await?;
    let ResultsResponse::Comparison(view) = response else {
        return Err(AgentError::Other("Expected a comparison".into()));
    };
    let report = Report::new(draft.title, view.comparison, view.charts)?;
    Ok(json!({"id": local.save_report(&report)?}))
}

pub async fn action(root: &Path, p: ActionParams) -> Result<Value> {
    let args = if matches!(p.action.as_str(), "publish_report" | "retrieve_report") {
        let id = p
            .report_id
            .ok_or_else(|| AgentError::Other("Report ID is required".into()))?;
        vec![
            "results".into(),
            "report".into(),
            if p.action == "publish_report" {
                "push"
            } else {
                "pull"
            }
            .into(),
            id,
            "--json".into(),
        ]
    } else {
        let selected = p
            .run
            .ok_or_else(|| AgentError::Other("Run selection is required".into()))?;
        // Verify the exact displayed snapshot before any command can alter a workspace.
        let local = LocalResults::new(root)?;
        let run = local.run_at(&selected.id, &selected.digest)?;
        if matches!(p.action.as_str(), "apply" | "resume" | "branch" | "publish")
            && local.run(&selected.id)?.digest != run.digest
        {
            return Err(AgentError::Other(
                "Run changed; refresh Results before acting".into(),
            ));
        }
        match p.action.as_str() {
            "preview_apply" | "preview_resume" => {
                let mut args = vec![
                    "results".into(),
                    "preview".into(),
                    selected.id,
                    "--digest".into(),
                    selected.digest,
                    "--json".into(),
                ];
                if p.action == "preview_resume" {
                    args.push("--resume".into());
                }
                args
            }
            "apply" => vec![
                "exp".into(),
                "apply".into(),
                selected.id,
                "--result-digest".into(),
                selected.digest,
                "--json".into(),
            ],
            "reproduce" => vec![
                "exp".into(),
                "reproduce".into(),
                selected.id,
                "--digest".into(),
                selected.digest,
                "--json".into(),
            ],
            "resume" => vec![
                "exp".into(),
                "run".into(),
                "--resume".into(),
                selected.id,
                "--result-digest".into(),
                selected.digest,
                "--json".into(),
            ],
            "branch" => vec![
                "exp".into(),
                "promote".into(),
                selected.id,
                "--result-digest".into(),
                selected.digest,
                "--branch".into(),
                p.branch
                    .ok_or_else(|| AgentError::Other("Branch name is required".into()))?,
                "--json".into(),
            ],
            "promote" => {
                let promotion = p.promotion.ok_or_else(|| {
                    AgentError::Other("Artifact version and target stage are required".into())
                })?;
                let mut args = vec![
                    "results".into(),
                    "promote".into(),
                    selected.id,
                    "--digest".into(),
                    selected.digest,
                    "--name".into(),
                    promotion.name,
                    "--version".into(),
                    promotion.version,
                    "--stage".into(),
                    promotion.stage,
                    "--json".into(),
                ];
                if let Some(expected) = promotion.expected {
                    args.extend(["--expected".into(), expected]);
                }
                args
            }
            "publish" => vec![
                "results".into(),
                "push".into(),
                selected.id,
                "--json".into(),
            ],
            _ => return Err(AgentError::Other("Unknown Results action".into())),
        }
    };
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    shellout::run_json(Some(root), &args, &[]).await
}
