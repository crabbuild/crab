//! Authored workflow documents and parser-derived execution projections.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{AgentError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPayload {
    pub path: String,
    #[serde(default)]
    pub params: Vec<Value>,
    #[serde(default)]
    pub metrics: Vec<Value>,
    #[serde(default)]
    pub plots: Vec<Value>,
    #[serde(default)]
    pub defaults: WorkflowDefaultsPayload,
    pub stages: Vec<StagePayload>,
    #[serde(default)]
    pub executions: Vec<ExecutionPayload>,
    #[serde(default)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPayload {
    pub name: String,
    pub source: String,
    pub deps: Vec<DepPayload>,
    pub outs: Vec<OutPayload>,
    pub wdir: Option<String>,
}

impl WorkflowPayload {
    pub(super) fn resolve(&mut self, path: &std::path::Path, text: &str) -> Result<()> {
        let workflow = crab_workflow::parse_at(path, text)?;
        self.executions = workflow
            .stages
            .values()
            .map(|stage| {
                let source = self
                    .stages
                    .iter()
                    .find(|source| stage.name.as_str() == source.name)
                    .or_else(|| {
                        self.stages.iter().find(|source| {
                            (source.foreach.is_some() || source.matrix.is_some())
                                && stage
                                    .name
                                    .as_str()
                                    .starts_with(&format!("{}@", source.name))
                        })
                    })
                    .ok_or_else(|| {
                        AgentError::Other(format!("expanded stage '{}' has no source", stage.name))
                    })?;
                let deps = stage
                    .deps
                    .iter()
                    .filter_map(|dep| match dep {
                        crab_workflow::Dep::Path(path) => Some(DepPayload {
                            kind: "path".into(),
                            value: path.to_string_lossy().into_owned(),
                            meta: None,
                        }),
                        crab_workflow::Dep::StageOut { stage, out } => Some(DepPayload {
                            kind: "stage_out".into(),
                            value: format!("{stage}:{}", out.display()),
                            meta: None,
                        }),
                        _ => None,
                    })
                    .collect();
                let outs = stage
                    .outs
                    .iter()
                    .map(|out| OutPayload {
                        path: out.path.to_string_lossy().into_owned(),
                        kind: if out.kind == crab_workflow::OutKind::Directory {
                            "directory"
                        } else {
                            "file"
                        }
                        .into(),
                        cache: Some(out.cache),
                        persist: Some(out.persist),
                        max_bytes: out.max_bytes,
                        extra: Map::new(),
                    })
                    .collect();
                Ok(ExecutionPayload {
                    name: stage.name.to_string(),
                    source: source.name.clone(),
                    deps,
                    outs,
                    wdir: stage
                        .wdir
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                })
            })
            .collect::<Result<_>>()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDefaultsPayload {
    #[serde(default)]
    pub extra: Map<String, Value>,
    #[serde(default)]
    pub env: Option<Value>,
    #[serde(default)]
    pub retry: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagePayload {
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub foreach: Option<Value>,
    #[serde(default)]
    pub matrix: Option<Value>,
    #[serde(default)]
    pub extra: Map<String, Value>,
    pub name: String,
    pub cmd: CmdPayload,
    #[serde(default)]
    pub deps: Vec<DepPayload>,
    #[serde(default)]
    pub outs: Vec<OutPayload>,
    #[serde(default)]
    pub params: Vec<Value>,
    #[serde(default)]
    pub env: Value,
    #[serde(default)]
    pub retry: Option<Value>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub nondeterministic: Option<bool>,
    #[serde(default)]
    pub hermetic: Option<bool>,
    #[serde(default)]
    pub side_effects: Option<bool>,
    #[serde(default)]
    pub resources: Option<Value>,
    #[serde(default)]
    pub frozen: Option<bool>,
    #[serde(default)]
    pub wdir: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub condition: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdPayload {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepPayload {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutPayload {
    #[serde(default)]
    pub extra: Map<String, Value>,
    pub path: String,
    pub kind: String,
    #[serde(default)]
    pub cache: Option<bool>,
    #[serde(default)]
    pub persist: Option<bool>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

const STAGE_FIELDS: &[&str] = &[
    "cmd",
    "deps",
    "outs",
    "params",
    "env",
    "retry",
    "timeout",
    "nondeterministic",
    "hermetic",
    "side_effects",
    "resources",
    "frozen",
    "wdir",
    "desc",
    "meta",
    "condition",
    "matrix",
];

fn extra(value: &Value, fields: &[&str]) -> Map<String, Value> {
    value
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(key, _)| !fields.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn array(value: &Value, key: &str) -> Result<Vec<Value>> {
    match value.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => Ok(values.clone()),
        Some(_) => Err(AgentError::Other(format!("'{key}' must be a sequence"))),
    }
}

pub(super) fn yaml_to_payload(path: &str, yaml: &Value) -> Result<WorkflowPayload> {
    let mut stages = Vec::new();
    if let Some(values) = yaml.get("stages") {
        let values = values
            .as_object()
            .ok_or_else(|| AgentError::Other("'stages' must be a mapping".into()))?;
        for (name, value) in values {
            stages.push(parse_stage(name, value, None)?);
        }
    }
    if let Some(groups) = yaml.get("workflows") {
        let groups = groups
            .as_object()
            .ok_or_else(|| AgentError::Other("'workflows' must be a mapping".into()))?;
        for (group, value) in groups {
            let values = value
                .get("stages")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    AgentError::Other(format!("workflow group '{group}' needs a stages mapping"))
                })?;
            for (name, value) in values {
                stages.push(parse_stage(name, value, Some(group.clone()))?);
            }
        }
    }
    if yaml.get("stages").is_none() && yaml.get("workflows").is_none() {
        return Err(AgentError::Other(
            "workflow must have stages or workflow groups".into(),
        ));
    }
    let defaults = yaml
        .get("defaults")
        .map(|value| WorkflowDefaultsPayload {
            env: value.get("env").cloned(),
            retry: value.get("retry").cloned(),
            extra: extra(value, &["env", "retry"]),
        })
        .unwrap_or_default();
    Ok(WorkflowPayload {
        path: path.into(),
        stages,
        executions: Vec::new(),
        defaults,
        params: array(yaml, "params")?,
        metrics: array(yaml, "metrics")?,
        plots: array(yaml, "plots")?,
        extra: extra(yaml, &["stages", "defaults", "params", "metrics", "plots"]),
    })
}

fn parse_stage(name: &str, raw: &Value, group: Option<String>) -> Result<StagePayload> {
    let value = raw.get("do").unwrap_or(raw);
    let cmd = match value.get("cmd") {
        Some(Value::String(text)) => CmdPayload {
            kind: "shell".into(),
            value: text.clone(),
        },
        Some(Value::Object(cmd)) if cmd.contains_key("argv") => CmdPayload {
            kind: "argv".into(),
            value: serde_json::to_string(&cmd["argv"])?,
        },
        Some(Value::Array(commands)) => CmdPayload {
            kind: "shell_list".into(),
            value: serde_json::to_string(commands)?,
        },
        _ => {
            return Err(AgentError::Other(format!(
                "stage '{name}': missing or invalid 'cmd'"
            )));
        }
    };
    let condition = value.get("condition").map(|condition| {
        for kind in ["env", "file_exists", "expr"] {
            if let Some(value) = condition.get(kind) {
                return json!({"kind": kind, "value": value});
            }
        }
        condition.clone()
    });
    Ok(StagePayload {
        name: name.into(),
        group,
        cmd,
        foreach: raw.get("foreach").cloned(),
        matrix: raw.get("matrix").cloned(),
        extra: extra(value, STAGE_FIELDS),
        deps: array(value, "deps")?
            .iter()
            .map(parse_dep)
            .collect::<Result<_>>()?,
        outs: array(value, "outs")?
            .iter()
            .map(parse_outs)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect(),
        params: array(value, "params")?,
        env: value.get("env").cloned().unwrap_or(Value::Null),
        retry: value.get("retry").cloned(),
        timeout: value
            .get("timeout")
            .and_then(Value::as_str)
            .map(String::from),
        nondeterministic: value.get("nondeterministic").and_then(Value::as_bool),
        hermetic: value.get("hermetic").and_then(Value::as_bool),
        side_effects: value.get("side_effects").and_then(Value::as_bool),
        resources: value.get("resources").cloned(),
        frozen: value.get("frozen").and_then(Value::as_bool),
        wdir: value.get("wdir").and_then(Value::as_str).map(String::from),
        desc: value.get("desc").and_then(Value::as_str).map(String::from),
        meta: value.get("meta").cloned(),
        condition,
    })
}

fn parse_dep(value: &Value) -> Result<DepPayload> {
    if let Some(path) = value.as_str() {
        return Ok(DepPayload {
            kind: "path".into(),
            value: path.into(),
            meta: None,
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| AgentError::Other("dependency must be a path or mapping".into()))?;
    let kind = ["stage_out", "path", "crab", "git", "url", "oci"]
        .into_iter()
        .find(|kind| object.contains_key(*kind))
        .ok_or_else(|| AgentError::Other("unknown dependency kind".into()))?;
    let text = match kind {
        "path" => object["path"]
            .as_str()
            .ok_or_else(|| AgentError::Other("dependency path must be text".into()))?
            .into(),
        "stage_out" => {
            let stage = object["stage_out"]["stage"]
                .as_str()
                .ok_or_else(|| AgentError::Other("stage_out needs a stage".into()))?;
            let out = object["stage_out"]["out"]
                .as_str()
                .ok_or_else(|| AgentError::Other("stage_out needs an output".into()))?;
            format!("{stage}:{out}")
        }
        _ => serde_json::to_string(value)?,
    };
    Ok(DepPayload {
        kind: kind.into(),
        value: text,
        meta: Some(value.clone()),
    })
}

fn parse_outs(value: &Value) -> Result<Vec<OutPayload>> {
    if value.is_string() || value.get("path").is_some() {
        return Ok(vec![parse_out(value)?]);
    }
    let entries = value
        .as_object()
        .ok_or_else(|| AgentError::Other("output needs a path or path settings mapping".into()))?;
    entries
        .iter()
        .map(|(path, settings)| {
            let mut settings = match settings {
                Value::Null => Map::new(),
                Value::Object(settings) => settings.clone(),
                _ => {
                    return Err(AgentError::Other(
                        "output settings must be a mapping".into(),
                    ));
                }
            };
            settings.insert("path".into(), json!(path));
            parse_out(&Value::Object(settings))
        })
        .collect()
}

fn parse_out(value: &Value) -> Result<OutPayload> {
    let path = value
        .as_str()
        .or_else(|| value.get("path").and_then(Value::as_str))
        .ok_or_else(|| AgentError::Other("output needs a path".into()))?;
    Ok(OutPayload {
        path: path.into(),
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("file")
            .into(),
        cache: value.get("cache").and_then(Value::as_bool),
        persist: value.get("persist").and_then(Value::as_bool),
        max_bytes: value.get("max_bytes").and_then(Value::as_u64),
        extra: extra(value, &["path", "cache", "persist", "max_bytes"]),
    })
}

fn command(cmd: &CmdPayload) -> Result<Value> {
    match cmd.kind.as_str() {
        "shell" => Ok(json!(cmd.value)),
        "argv" | "shell_list" => {
            let parts: Vec<String> = serde_json::from_str(&cmd.value)?;
            Ok(if cmd.kind == "argv" {
                json!({"argv": parts})
            } else {
                json!(parts)
            })
        }
        kind => Err(AgentError::Other(format!(
            "unsupported command kind: {kind}"
        ))),
    }
}

fn dep_value(dep: &DepPayload) -> Result<Value> {
    let mut object = dep
        .meta
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match dep.kind.as_str() {
        "path" if object.is_empty() => return Ok(json!(dep.value)),
        "path" => {
            object.insert("path".into(), json!(dep.value));
        }
        "stage_out" => {
            let (stage, out) = dep
                .value
                .split_once(':')
                .ok_or_else(|| AgentError::Other("stage output needs stage:path".into()))?;
            object.insert("stage_out".into(), json!({"stage": stage, "out": out}));
        }
        _ => return Ok(serde_json::from_str(&dep.value)?),
    }
    Ok(Value::Object(object))
}

fn out_value(out: &OutPayload) -> Value {
    let mut object = out.extra.clone();
    object.insert("path".into(), json!(out.path));
    if out.kind != "file" || object.contains_key("kind") {
        object.insert("kind".into(), json!(out.kind));
    }
    for (key, value) in [
        ("cache", out.cache.map(Value::Bool)),
        ("persist", out.persist.map(Value::Bool)),
        ("max_bytes", out.max_bytes.map(Value::from)),
    ] {
        if let Some(value) = value {
            object.insert(key.into(), value);
        }
    }
    if object.len() == 1 {
        json!(out.path)
    } else {
        Value::Object(object)
    }
}

pub(super) fn payload_to_yaml(workflow: &WorkflowPayload) -> Result<String> {
    let mut document = workflow.extra.clone();
    for (key, values) in [
        ("params", &workflow.params),
        ("metrics", &workflow.metrics),
        ("plots", &workflow.plots),
    ] {
        if !values.is_empty() {
            document.insert(key.into(), json!(values));
        }
    }
    let mut defaults = workflow.defaults.extra.clone();
    for (key, value) in [
        ("env", &workflow.defaults.env),
        ("retry", &workflow.defaults.retry),
    ] {
        if let Some(value) = value {
            defaults.insert(key.into(), value.clone());
        }
    }
    if !defaults.is_empty() {
        document.insert("defaults".into(), Value::Object(defaults));
    }
    let mut groups = match document.remove("workflows") {
        None => Map::new(),
        Some(Value::Object(groups)) => groups,
        Some(_) => return Err(AgentError::Other("'workflows' must be a mapping".into())),
    };
    for group in groups.values_mut() {
        let group = group
            .as_object_mut()
            .ok_or_else(|| AgentError::Other("workflow group must be a mapping".into()))?;
        group.insert("stages".into(), json!({}));
    }
    let mut stages = Map::new();
    for stage in &workflow.stages {
        let mut body = stage.extra.clone();
        body.insert("cmd".into(), command(&stage.cmd)?);
        for (key, value) in [
            (
                "deps",
                stage
                    .deps
                    .iter()
                    .map(dep_value)
                    .collect::<Result<Vec<_>>>()?,
            ),
            ("outs", stage.outs.iter().map(out_value).collect::<Vec<_>>()),
            ("params", stage.params.clone()),
        ] {
            if !value.is_empty() {
                body.insert(key.into(), json!(value));
            }
        }
        let fields = serde_json::to_value(stage)?;
        for key in [
            "env",
            "retry",
            "timeout",
            "nondeterministic",
            "hermetic",
            "side_effects",
            "resources",
            "frozen",
            "wdir",
            "desc",
            "meta",
        ] {
            if !fields[key].is_null() {
                body.insert(key.into(), fields[key].clone());
            }
        }
        if let Some(condition) = &stage.condition {
            let value = if let Some(kind) = condition.get("kind").and_then(Value::as_str) {
                let mut mapping = Map::new();
                mapping.insert(kind.into(), condition["value"].clone());
                Value::Object(mapping)
            } else {
                condition.clone()
            };
            body.insert("condition".into(), value);
        }
        let value = if let Some(foreach) = &stage.foreach {
            json!({"foreach": foreach, "do": body})
        } else {
            if let Some(matrix) = &stage.matrix {
                body.insert("matrix".into(), matrix.clone());
            }
            Value::Object(body)
        };
        if let Some(group) = &stage.group {
            let value_group = groups
                .entry(group.clone())
                .or_insert_with(|| json!({"stages": {}}));
            let stages = value_group
                .get_mut("stages")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| AgentError::Other("workflow group needs a stages mapping".into()))?;
            if stages.insert(stage.name.clone(), value).is_some() {
                return Err(AgentError::Other(format!(
                    "duplicate stage '{}'",
                    stage.name
                )));
            }
        } else if stages.insert(stage.name.clone(), value).is_some() {
            return Err(AgentError::Other(format!(
                "duplicate stage '{}'",
                stage.name
            )));
        }
    }
    if !stages.is_empty() || groups.is_empty() {
        document.insert("stages".into(), Value::Object(stages));
    }
    if !groups.is_empty() {
        document.insert("workflows".into(), Value::Object(groups));
    }
    Ok(serde_yaml::to_string(&document)?)
}
