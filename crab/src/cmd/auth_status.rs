//! `crab auth status` — display authentication status and configuration.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crab_auth::token_cache::{TokenCache, expand_token_cache_path};
use serde::Serialize;

use crate::core::config::{AuthProvider, Config};
use crate::core::error::Result;

/// JSON-serializable auth status for `--json` output.
#[derive(Serialize)]
struct AuthStatus {
    provider: String,
    identity: Option<String>,
    token_expiry: Option<String>,
    token_expired: bool,
    refresh: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provider_settings: Vec<ProviderSetting>,
}

/// A single provider-specific key-value setting.
#[derive(Serialize)]
struct ProviderSetting {
    key: String,
    value: String,
}

/// Run the `crab auth status` command.
///
/// Reads the auth config and token cache, then displays the current
/// authentication state. With `--json`, outputs machine-readable JSON.
/// Suggests `crab login` when tokens are missing or expired.
pub fn run_auth_status(json: bool, config: &Config) -> Result<()> {
    let provider = config.auth.provider;
    let provider_name = provider.as_str();

    // Load cached tokens (if any).
    let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
    let cache = TokenCache::new(cache_dir)?;
    let cached = cache.load_any(provider.token_cache_keys())?;

    let (identity, expiry_str, expired, has_refresh) = match &cached {
        Some(tokens) => {
            let display_identity = tokens
                .identity
                .email
                .as_deref()
                .unwrap_or(&tokens.identity.subject);

            let (expiry, is_expired) = parse_token_expiry(&tokens.id_token);

            (
                Some(display_identity.to_owned()),
                expiry,
                is_expired,
                tokens.refresh_token.is_some(),
            )
        }
        None => (None, None, true, false),
    };

    let provider_settings = collect_provider_settings(provider, config);

    if json {
        let status = AuthStatus {
            provider: provider_name.to_owned(),
            identity,
            token_expiry: expiry_str,
            token_expired: expired,
            refresh: has_refresh,
            provider_settings,
        };
        crate::core::output::emit_json("auth.status", "1.0", &status);
    } else {
        print_text_status(
            provider_name,
            identity.as_deref(),
            expiry_str.as_deref(),
            expired,
            has_refresh,
            &provider_settings,
            cached.is_none(),
        );
    }

    Ok(())
}

/// Display auth status as aligned text on stderr.
fn print_text_status(
    provider: &str,
    identity: Option<&str>,
    expiry: Option<&str>,
    expired: bool,
    has_refresh: bool,
    settings: &[ProviderSetting],
    no_tokens: bool,
) {
    if no_tokens {
        eprintln!("Provider:     {provider}");
        eprintln!("Status:       Not authenticated");
        eprintln!();
        eprintln!("Run `crab login` to authenticate.");
        return;
    }

    eprintln!("Provider:     {provider}");

    if let Some(id) = identity {
        eprintln!("Identity:     {id}");
    }

    match (expiry, expired) {
        (Some(exp), false) => {
            let remaining = format_remaining(exp);
            eprintln!("Token expiry: {exp}{remaining}");
        }
        (Some(exp), true) => {
            eprintln!("Token expiry: {exp} (expired)");
            eprintln!();
            eprintln!("Run `crab login` to re-authenticate.");
        }
        (None, _) => {
            eprintln!("Token expiry: unknown");
        }
    }

    let refresh_str = if has_refresh { "yes" } else { "no" };
    eprintln!("Refresh:      {refresh_str}");

    for s in settings {
        // Right-pad the key to align with the 14-char label column.
        eprintln!("{:<14}{}", format!("{}:", s.key), s.value);
    }
}

/// Parse the `exp` claim from a JWT and return (ISO 8601 string, is_expired).
fn parse_token_expiry(id_token: &str) -> (Option<String>, bool) {
    let parts: Vec<&str> = id_token.splitn(3, '.').collect();
    if parts.len() < 2 {
        return (None, true);
    }

    let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(parts[1]) else {
        return (None, true);
    };

    let claims: serde_json::Value = match serde_json::from_slice(&payload_bytes) {
        Ok(v) => v,
        Err(_) => return (None, true),
    };

    let Some(exp) = claims.get("exp").and_then(serde_json::Value::as_u64) else {
        return (None, true);
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let is_expired = now >= exp;

    // Format as ISO 8601 UTC.
    let expiry_str = format_unix_timestamp(exp);

    (Some(expiry_str), is_expired)
}

/// Format a Unix timestamp as an ISO 8601 UTC string (YYYY-MM-DDTHH:MM:SSZ).
fn format_unix_timestamp(ts: u64) -> String {
    // Manual formatting to avoid pulling in chrono just for this.
    let secs = ts as i64;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since Unix epoch to (year, month, day) — civil calendar algorithm.
    let (y, m, d) = days_to_ymd(days + 719_468);

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert a day count (from a shifted epoch) to (year, month, day).
/// Uses Howard Hinnant's civil_from_days algorithm.
fn days_to_ymd(z: i64) -> (i64, u32, u32) {
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Format a human-readable "remaining" suffix like " (52 minutes remaining)".
fn format_remaining(expiry_iso: &str) -> String {
    // Parse the ISO 8601 timestamp back to seconds for comparison.
    // We only handle our own format: YYYY-MM-DDTHH:MM:SSZ
    let Some(exp_secs) = parse_iso_timestamp(expiry_iso) else {
        return String::new();
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if now >= exp_secs {
        return String::new();
    }

    let remaining = exp_secs - now;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;

    if hours > 0 {
        format!(" ({hours}h {minutes}m remaining)")
    } else {
        format!(" ({minutes} minutes remaining)")
    }
}

/// Parse our own ISO 8601 format back to Unix seconds. Returns None on failure.
fn parse_iso_timestamp(s: &str) -> Option<u64> {
    // Expected: YYYY-MM-DDTHH:MM:SSZ
    if s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let hh: u64 = s[11..13].parse().ok()?;
    let mm: u64 = s[14..16].parse().ok()?;
    let ss: u64 = s[17..19].parse().ok()?;

    let days = ymd_to_days(y, m, d)?;
    Some(days as u64 * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Convert (year, month, day) to days since Unix epoch.
/// Inverse of `days_to_ymd`.
fn ymd_to_days(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let z = era * 146_097 + i64::from(doe) - 719_468;
    Some(z)
}

/// Collect provider-specific settings for display.
fn collect_provider_settings(provider: AuthProvider, config: &Config) -> Vec<ProviderSetting> {
    let mut settings = Vec::new();

    match provider {
        AuthProvider::AwsOidc => {
            if let Some(ref role) = config.auth.aws.role_arn {
                settings.push(ProviderSetting {
                    key: "AWS role".into(),
                    value: role.clone(),
                });
            }
            if let Some(ref region) = config.auth.aws.region {
                settings.push(ProviderSetting {
                    key: "Region".into(),
                    value: region.clone(),
                });
            }
        }
        AuthProvider::GcpWorkloadIdentity => {
            if let Some(ref pool) = config.auth.gcp.workload_identity_pool {
                settings.push(ProviderSetting {
                    key: "WI pool".into(),
                    value: pool.clone(),
                });
            }
            if let Some(ref sa) = config.auth.gcp.service_account {
                settings.push(ProviderSetting {
                    key: "Service acct".into(),
                    value: sa.clone(),
                });
            }
            if let Some(ref pid) = config.auth.gcp.project_id {
                settings.push(ProviderSetting {
                    key: "Project".into(),
                    value: pid.clone(),
                });
            }
        }
        AuthProvider::AzureEntra => {
            if let Some(ref tid) = config.auth.azure.tenant_id {
                settings.push(ProviderSetting {
                    key: "Tenant".into(),
                    value: tid.clone(),
                });
            }
            if let Some(ref sa) = config.auth.azure.storage_account {
                settings.push(ProviderSetting {
                    key: "Storage acct".into(),
                    value: sa.clone(),
                });
            }
        }
        AuthProvider::CrabAuth => {
            if let Some(ref ep) = config.auth.auth_endpoint {
                settings.push(ProviderSetting {
                    key: "Endpoint".into(),
                    value: ep.clone(),
                });
            }
        }
        AuthProvider::Static => {
            settings.push(ProviderSetting {
                key: "Storage".into(),
                value: config.auth.storage_provider.toml_value().into(),
            });
        }
        AuthProvider::None => {}
    }

    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn format_unix_timestamp_known_date() {
        // 2026-04-24T17:30:00Z = 1777076400 (approx)
        // Use a well-known epoch: 2024-01-01T00:00:00Z = 1704067200
        let ts = 1704067200;
        assert_eq!(format_unix_timestamp(ts), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn format_unix_timestamp_epoch() {
        assert_eq!(format_unix_timestamp(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parse_iso_round_trip() {
        let ts = 1704067200u64;
        let iso = format_unix_timestamp(ts);
        let parsed = parse_iso_timestamp(&iso).unwrap();
        assert_eq!(parsed, ts);
    }

    #[test]
    fn parse_token_expiry_valid() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        // Token that expires far in the future.
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = format!(r#"{{"sub":"u1","exp":{exp}}}"#);
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_some());
        assert!(!is_expired);
    }

    #[test]
    fn parse_token_expiry_expired() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let exp = 1000u64; // way in the past
        let claims = format!(r#"{{"sub":"u1","exp":{exp}}}"#);
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_some());
        assert!(is_expired);
    }

    #[test]
    fn parse_token_expiry_no_exp_claim() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims = r#"{"sub":"u1"}"#;
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_none());
        assert!(is_expired);
    }

    #[test]
    fn parse_token_expiry_malformed_jwt() {
        let (expiry_str, is_expired) = parse_token_expiry("not-a-jwt");
        assert!(expiry_str.is_none());
        assert!(is_expired);
    }

    #[test]
    fn collect_settings_aws() {
        let mut config = Config::default();
        config.auth.provider = AuthProvider::AwsOidc;
        config.auth.aws.role_arn = Some("arn:aws:iam::123:role/test".into());
        config.auth.aws.region = Some("us-west-2".into());

        let settings = collect_provider_settings(AuthProvider::AwsOidc, &config);
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0].key, "AWS role");
        assert_eq!(settings[1].key, "Region");
    }

    #[test]
    fn collect_settings_none_is_empty() {
        let config = Config::default();
        let settings = collect_provider_settings(AuthProvider::None, &config);
        assert!(settings.is_empty());
    }

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_token_cache_path("~/.config/crab/tokens/");
        assert!(!result.as_os_str().is_empty());
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let result = expand_token_cache_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }
}
