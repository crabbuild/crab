use super::{management_body, management_request};
use crab_server::{Config, Error, Result};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
};

fn key(record: &Value) -> Result<(u8, u64)> {
    let kind = match record["target"]["kind"].as_str() {
        Some("issue") => 0,
        Some("pull") => 1,
        _ => {
            return Err(Error::Config(
                "Assignment mapping requires an issue or pull target",
            ));
        }
    };
    let number = record["target"]["number"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
        .ok_or(Error::Config(
            "Assignment mapping requires a valid discussion number",
        ))?;
    Ok((kind, number))
}
fn line(reader: &mut dyn BufRead) -> Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let bytes = Read::take(&mut *reader, 1024 * 1024 + 1).read_line(&mut line)?;
        if bytes > 1024 * 1024 {
            return Err(Error::Config("Assignment mapping line exceeds 1 MiB"));
        }
        if bytes == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            let record = serde_json::from_str(&line)?;
            key(&record)?;
            return Ok(Some(record));
        }
    }
}
async fn post(config: &Config, path: &str, input: &Value) -> Result<Value> {
    management_request(config, reqwest::Method::POST, path, Some(input))
        .await?
        .json()
        .await
        .map_err(|source| Error::Healthcheck { source })
}
pub(super) async fn inventory(config: &Config, owner: &str, name: &str) -> Result<()> {
    let path = format!("/internal/repositories/{owner}/{name}/assignments/inventory");
    let mut after = Value::Null;
    let mut output = std::io::stdout();
    loop {
        let batch = post(config, &path, &after).await?;
        let records = batch["records"]
            .as_array()
            .ok_or(Error::Config("Invalid assignment inventory reply"))?;
        if records.is_empty() {
            return Ok(());
        }
        for record in records {
            serde_json::to_writer(&mut output, record)?;
            output.write_all(b"\n")?;
        }
        after = records
            .last()
            .ok_or(Error::Config("Invalid assignment inventory cursor"))?["target"]
            .clone();
    }
}
pub(super) async fn migrate(
    config: &Config,
    owner: &str,
    name: &str,
    mappings: Option<&Path>,
) -> Result<Vec<u8>> {
    let path = format!("/internal/repositories/{owner}/{name}/assignments");
    let mut reader: Box<dyn BufRead> = match mappings {
        Some(path) => Box::new(BufReader::new(File::open(path)?)),
        None => Box::new(std::io::empty()),
    };
    let mut next = line(reader.as_mut())?;
    let mut lower_bound = None;
    let mut previous = None;
    loop {
        let batch: Value = serde_json::from_slice(&management_body(config, &path).await?)?;
        let state = &batch["state"];
        if state["state"] == "ready" {
            return Ok(serde_json::to_vec(state)?);
        }
        let records = batch["records"]
            .as_array()
            .ok_or(Error::Config("Invalid assignment migration reply"))?;
        if lower_bound.is_none() {
            lower_bound = Some(if state["state"] == "migrating" {
                records.first().map(key).transpose()?.unwrap_or((2, 0))
            } else {
                (0, 0)
            });
            while next
                .as_ref()
                .map(key)
                .transpose()?
                .is_some_and(|key| Some(key) < lower_bound)
            {
                next = line(reader.as_mut())?;
            }
        }
        let mut mapped = Vec::new();
        for record in records {
            if record["subjects"]
                .as_array()
                .ok_or(Error::Config("Invalid assignment source reply"))?
                .is_empty()
            {
                continue;
            }
            let source = next.take().ok_or(Error::Config("Next assigned record needs an explicit mapping; inspect cells assignment-inventory and resume with --mappings-file"))?;
            let target = key(&source)?;
            if target != key(record)? || previous.is_some_and(|previous| previous >= target) {
                return Err(Error::Config(
                    "Assignment mappings must match source records in issue/pull number order",
                ));
            }
            previous = Some(target);
            mapped.push(source);
            next = line(reader.as_mut())?;
        }
        if records.is_empty() && next.is_some() {
            return Err(Error::Config(
                "Assignment mappings contain an extra source record",
            ));
        }
        let version = match state["state"].as_str() {
            Some("outdated") => 0,
            Some("migrating") => state["version"]
                .as_u64()
                .ok_or(Error::Config("Invalid assignment migration version"))?,
            _ => return Err(Error::Config("Invalid assignment preparation state")),
        };
        let state = post(config, &path, &json!({"version":version,"records":mapped})).await?;
        if state["state"] == "ready" {
            return Ok(serde_json::to_vec(&state)?);
        }
    }
}
