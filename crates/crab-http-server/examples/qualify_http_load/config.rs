use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use reqwest::header::{
    CONTENT_LENGTH, HOST, HeaderMap, HeaderName, HeaderValue, TRANSFER_ENCODING,
};
use url::{Host, Url};

use super::Error;

const MAX_HEADERS: usize = 64;
const MAX_HEADER_FILE_BYTES: u64 = 64 * 1024;
const MAX_MUTATION_TEMPLATE_BYTES: u64 = 1024 * 1024;
const MAX_TARGETS: usize = 64;
const MAX_TOTAL_CONCURRENCY: usize = 4_096;

#[derive(Clone, Debug)]
pub(super) struct TargetSpec {
    pub(super) name: String,
    pub(super) concurrency: usize,
    pub(super) path: String,
}

#[derive(Clone, Debug)]
pub(super) struct MutationSpec {
    pub(super) target: TargetSpec,
    pub(super) body_file: PathBuf,
}

impl FromStr for TargetSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, request) = value
            .split_once('=')
            .ok_or_else(|| "target must contain '='".to_owned())?;
        let (concurrency, path) = request
            .split_once('@')
            .ok_or_else(|| "target must contain '@'".to_owned())?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("target name must be 1-64 ASCII letters, digits, '-' or '_'".to_owned());
        }
        let concurrency = concurrency
            .parse::<usize>()
            .map_err(|_| "target concurrency is not an integer".to_owned())?;
        if !(1..=1_024).contains(&concurrency) {
            return Err("target concurrency must be between 1 and 1024".to_owned());
        }
        if !path.starts_with('/')
            || path.starts_with("//")
            || path.contains('#')
            || path.chars().any(char::is_control)
        {
            return Err(
                "target path must be one absolute-origin path without a fragment".to_owned(),
            );
        }
        Ok(Self {
            name: name.to_owned(),
            concurrency,
            path: path.to_owned(),
        })
    }
}

impl FromStr for MutationSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (target, body_file) = value
            .rsplit_once('|')
            .ok_or_else(|| "mutation must contain '|' before its body file".to_owned())?;
        if body_file.is_empty() {
            return Err("mutation body file must not be empty".to_owned());
        }
        Ok(Self {
            target: target.parse()?,
            body_file: PathBuf::from(body_file),
        })
    }
}

pub(super) fn validate_origin(url: &Url) -> Result<(), Error> {
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Configuration(
            "base URL must be one HTTP(S) origin without credentials, path, query, or fragment",
        ));
    }
    Ok(())
}

pub(super) fn validate_path(path: &str) -> Result<(), Error> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return Err(Error::Configuration(
            "health path must be one absolute-origin path without a fragment",
        ));
    }
    Ok(())
}

pub(super) fn validate_targets(targets: &[TargetSpec]) -> Result<(), Error> {
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        return Err(Error::Configuration("target count is outside its bound"));
    }
    let mut names = HashSet::with_capacity(targets.len());
    let mut total = 0usize;
    for target in targets {
        if !names.insert(target.name.as_str()) {
            return Err(Error::Configuration("target names must be unique"));
        }
        total = total
            .checked_add(target.concurrency)
            .ok_or(Error::Configuration("total concurrency overflow"))?;
    }
    if total > MAX_TOTAL_CONCURRENCY {
        return Err(Error::Configuration(
            "total target concurrency exceeds 4096",
        ));
    }
    Ok(())
}

pub(super) fn load_headers(path: Option<&Path>) -> Result<HeaderMap, Error> {
    let Some(path) = path else {
        return Ok(HeaderMap::new());
    };
    let metadata = fs::metadata(path).map_err(Error::HeaderFile)?;
    if metadata.len() > MAX_HEADER_FILE_BYTES {
        return Err(Error::Configuration("header file exceeds 64 KiB"));
    }
    let body = fs::read_to_string(path).map_err(Error::HeaderFile)?;
    let mut headers = HeaderMap::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        if headers.len() == MAX_HEADERS {
            return Err(Error::Configuration("header file exceeds 64 headers"));
        }
        let (name, value) = line.split_once(':').ok_or(Error::Header)?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| Error::Header)?;
        if matches!(name, HOST | CONTENT_LENGTH | TRANSFER_ENCODING) {
            return Err(Error::Configuration(
                "header file cannot override HTTP framing or authority",
            ));
        }
        let value = HeaderValue::from_str(value.trim()).map_err(|_| Error::Header)?;
        headers.append(name, value);
    }
    Ok(headers)
}

pub(super) fn load_authority(value: Option<&str>) -> Result<Option<HeaderValue>, Error> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > 253 || Host::parse(value).is_err() {
        return Err(Error::Configuration(
            "authority must be one DNS name or IP address without a port",
        ));
    }
    HeaderValue::from_str(value)
        .map(Some)
        .map_err(|_| Error::Configuration("authority is not a valid HTTP Host value"))
}

pub(super) fn load_mutation_template(path: &Path) -> Result<Arc<str>, Error> {
    let metadata = fs::metadata(path).map_err(Error::MutationFile)?;
    if metadata.len() > MAX_MUTATION_TEMPLATE_BYTES {
        return Err(Error::Configuration("mutation template exceeds 1 MiB"));
    }
    let body = fs::read_to_string(path).map_err(Error::MutationFile)?;
    let value = serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|_| Error::MutationTemplate("body must be valid JSON"))?;
    if value.get("request_id").and_then(|value| value.as_str()) != Some("{{request_id}}") {
        return Err(Error::MutationTemplate(
            "top-level request_id must equal '{{request_id}}'",
        ));
    }
    Ok(Arc::from(body))
}
