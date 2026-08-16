//! Shared active probe for cache-service client readiness checks.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

#[derive(Debug, Clone, Copy)]
pub enum ActiveProbeAuth<'a> {
    None,
    Psk(&'a str),
    Bearer(&'a str),
    Mtls,
}

#[derive(Debug)]
pub struct ActiveProbeObject {
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct ActiveProbeOutcome {
    pub body_len: usize,
    pub evicted_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ActiveProbeEvictStats {
    evicted_count: u64,
    evicted_bytes: u64,
}

pub fn build_active_probe(
    repo_path: &str,
    identity_prefix: &str,
    body_label: &str,
) -> ActiveProbeObject {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let identity = format!("{identity_prefix}-{}-{now_ms}", std::process::id());
    let path = format!("{repo_path}/packs/{identity}.pack");
    let body = format!("{body_label}\n{identity}\n").into_bytes();
    ActiveProbeObject { path, body }
}

pub async fn run_active_probe(
    client: &reqwest::Client,
    base_url: &str,
    auth: ActiveProbeAuth<'_>,
    auth_failure_hint: &str,
    probe: &ActiveProbeObject,
) -> Result<ActiveProbeOutcome, String> {
    if probe.body.len() < 4 {
        return Err("active probe body must be at least 4 bytes".to_string());
    }

    match evict_probe_object(client, base_url, auth, auth_failure_hint, &probe.path).await {
        Ok(_) => {}
        Err(detail) => {
            return Err(format!(
                "preflight exact eviction failed before write: {detail}"
            ));
        }
    }

    put_probe_object(client, base_url, auth, auth_failure_hint, probe).await?;

    if let Err(detail) = get_probe_object(client, base_url, auth, auth_failure_hint, probe).await {
        let cleanup = cleanup_detail(
            evict_probe_object(client, base_url, auth, auth_failure_hint, &probe.path).await,
        );
        return Err(format!("{detail}; {cleanup}"));
    }

    if let Err(detail) = range_probe_object(client, base_url, auth, auth_failure_hint, probe).await
    {
        let cleanup = cleanup_detail(
            evict_probe_object(client, base_url, auth, auth_failure_hint, &probe.path).await,
        );
        return Err(format!("{detail}; {cleanup}"));
    }

    let cleanup =
        evict_probe_object(client, base_url, auth, auth_failure_hint, &probe.path).await?;
    if cleanup.evicted_count != 1 || cleanup.evicted_bytes != probe.body.len() as u64 {
        return Err(format!(
            "cleanup returned evicted_count={}, evicted_bytes={}",
            cleanup.evicted_count, cleanup.evicted_bytes
        ));
    }

    Ok(ActiveProbeOutcome {
        body_len: probe.body.len(),
        evicted_bytes: cleanup.evicted_bytes,
    })
}

async fn put_probe_object(
    client: &reqwest::Client,
    base_url: &str,
    auth: ActiveProbeAuth<'_>,
    auth_failure_hint: &str,
    probe: &ActiveProbeObject,
) -> Result<(), String> {
    let response = apply_auth(
        client
            .put(format!("{base_url}/v1/{}", probe.path))
            .body(probe.body.clone()),
        auth,
    )
    .send()
    .await
    .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "probe write returned HTTP {}; {}",
            status.as_u16(),
            status_hint(status, auth_failure_hint)
        ));
    }
    Ok(())
}

async fn get_probe_object(
    client: &reqwest::Client,
    base_url: &str,
    auth: ActiveProbeAuth<'_>,
    auth_failure_hint: &str,
    probe: &ActiveProbeObject,
) -> Result<(), String> {
    let response = apply_auth(client.get(format!("{base_url}/v1/{}", probe.path)), auth)
        .send()
        .await
        .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "probe read returned HTTP {}; {}",
            status.as_u16(),
            status_hint(status, auth_failure_hint)
        ));
    }
    let cache_status = response
        .headers()
        .get("x-cache")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response
        .bytes()
        .await
        .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    if body.as_ref() != probe.body.as_slice() {
        return Err("probe read body did not match written bytes".to_string());
    }
    if cache_status.as_deref() != Some("HIT") {
        return Err(format!(
            "probe read expected cache HIT, got {}",
            cache_status.unwrap_or_else(|| "missing".to_string())
        ));
    }
    Ok(())
}

async fn range_probe_object(
    client: &reqwest::Client,
    base_url: &str,
    auth: ActiveProbeAuth<'_>,
    auth_failure_hint: &str,
    probe: &ActiveProbeObject,
) -> Result<(), String> {
    let response = apply_auth(
        client
            .get(format!("{base_url}/v1/{}", probe.path))
            .header(reqwest::header::RANGE, "bytes=0-3"),
        auth,
    )
    .send()
    .await
    .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    let status = response.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "probe range read returned HTTP {}; {}",
            status.as_u16(),
            status_hint(status, auth_failure_hint)
        ));
    }
    let cache_status = response
        .headers()
        .get("x-cache")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response
        .bytes()
        .await
        .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    if body.as_ref() != &probe.body[..4] {
        return Err("probe range body did not match written bytes".to_string());
    }
    if cache_status.as_deref() != Some("HIT") {
        return Err(format!(
            "probe range expected cache HIT, got {}",
            cache_status.unwrap_or_else(|| "missing".to_string())
        ));
    }
    Ok(())
}

async fn evict_probe_object(
    client: &reqwest::Client,
    base_url: &str,
    auth: ActiveProbeAuth<'_>,
    auth_failure_hint: &str,
    path: &str,
) -> Result<ActiveProbeEvictStats, String> {
    let response = apply_auth(
        client
            .post(format!("{base_url}/v1/admin/evict"))
            .json(&serde_json::json!({ "path": path })),
        auth,
    )
    .send()
    .await
    .map_err(|error| redact_probe_error(base_url, &error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "exact eviction returned HTTP {}; {}",
            status.as_u16(),
            status_hint(status, auth_failure_hint)
        ));
    }
    response
        .json::<ActiveProbeEvictStats>()
        .await
        .map_err(|error| format!("exact eviction JSON did not match expected schema: {error}"))
}

fn cleanup_detail(result: Result<ActiveProbeEvictStats, String>) -> String {
    match result {
        Ok(stats) => format!(
            "cleanup evicted_count={}, evicted_bytes={}",
            stats.evicted_count, stats.evicted_bytes
        ),
        Err(detail) => format!("cleanup failed: {detail}"),
    }
}

fn apply_auth(
    request: reqwest::RequestBuilder,
    auth: ActiveProbeAuth<'_>,
) -> reqwest::RequestBuilder {
    match auth {
        ActiveProbeAuth::None | ActiveProbeAuth::Mtls => request,
        ActiveProbeAuth::Psk(psk) => request.header("x-cache-psk", psk),
        ActiveProbeAuth::Bearer(token) => request.bearer_auth(token),
    }
}

fn status_hint(status: reqwest::StatusCode, auth_failure_hint: &str) -> &str {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return auth_failure_hint;
    }
    "check cache-service logs and authorization policy"
}

fn redact_probe_error(base_url: &str, error: &str) -> String {
    error.replace(base_url, "configured-redacted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_probe_path_uses_cacheable_pack_shape() {
        let probe = build_active_probe("org/repo", "crab-test-probe", "test active probe");

        assert!(probe.path.starts_with("org/repo/packs/crab-test-probe-"));
        assert!(probe.path.ends_with(".pack"));
        assert!(probe.body.starts_with(b"test active probe\n"));
    }
}
