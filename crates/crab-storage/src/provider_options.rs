//! Shared object-store provider construction options.

use std::time::Duration;

use object_store::ClientOptions;
use object_store::aws::AmazonS3Builder;

/// Applies S3-compatible endpoint and addressing overrides from environment.
#[must_use]
pub fn apply_s3_env_overrides(builder: AmazonS3Builder) -> AmazonS3Builder {
    let mut builder = builder;
    if let Some(endpoint) = s3_endpoint_from_env() {
        builder = builder.with_endpoint(endpoint);
    }
    if let Some(virtual_hosted) = s3_virtual_hosted_style_from_env() {
        builder = builder.with_virtual_hosted_style_request(virtual_hosted);
    }
    builder
}

/// Reads the first non-empty S3 endpoint override from supported env vars.
#[must_use]
pub fn s3_endpoint_from_env() -> Option<String> {
    for key in [
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT",
        "ENDPOINT_URL",
        "ENDPOINT",
    ] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// Reads S3 virtual-hosted-style override from supported env vars.
#[must_use]
pub fn s3_virtual_hosted_style_from_env() -> Option<bool> {
    for key in [
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST",
        "VIRTUAL_HOSTED_STYLE_REQUEST",
    ] {
        if let Ok(value) = std::env::var(key) {
            let normalized = value.trim().to_ascii_lowercase();
            return match normalized.as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// Default HTTP client options applied to every object-store builder.
///
/// Respects `AWS_ALLOW_HTTP=true` so plain-HTTP endpoints such as local MinIO
/// continue to work after explicit client options are applied.
#[must_use]
pub fn default_client_options() -> ClientOptions {
    let opts = ClientOptions::new()
        .with_timeout(Duration::from_secs(300))
        .with_connect_timeout(Duration::from_secs(10));

    if std::env::var("AWS_ALLOW_HTTP")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
    {
        opts.with_allow_http(true)
    } else {
        opts
    }
}

/// Parses a SAS token query string into key-value pairs for Azure builders.
#[must_use]
pub fn parse_sas_query_pairs(sas: &str) -> Vec<(String, String)> {
    let s = sas.strip_prefix('?').unwrap_or(sas);
    s.split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_client_options_builds_expected_type() {
        let opts = default_client_options();
        let _: ClientOptions = opts;
    }

    #[test]
    fn s3_endpoint_env_accepts_standard_object_store_names() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _aws_endpoint_url = EnvGuard::set("AWS_ENDPOINT_URL", None);
        let _aws_endpoint = EnvGuard::set("AWS_ENDPOINT", None);
        let _endpoint_url = EnvGuard::set("ENDPOINT_URL", Some(" http://localhost:5000 "));
        let _endpoint = EnvGuard::set("ENDPOINT", None);

        assert_eq!(
            s3_endpoint_from_env(),
            Some("http://localhost:5000".to_owned())
        );
    }

    #[test]
    fn s3_virtual_hosted_style_env_parses_booleans() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _aws_virtual = EnvGuard::set("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", Some("false"));
        let _virtual = EnvGuard::set("VIRTUAL_HOSTED_STYLE_REQUEST", None);

        assert_eq!(s3_virtual_hosted_style_from_env(), Some(false));
    }

    #[test]
    fn parse_sas_with_leading_question_mark() {
        let pairs = parse_sas_query_pairs("?sv=2021-06-08&ss=b&srt=sco&sp=rwdlacup&se=2025-01-01");
        assert_eq!(pairs.len(), 5);
        assert_eq!(pairs[0], ("sv".into(), "2021-06-08".into()));
        assert_eq!(pairs[1], ("ss".into(), "b".into()));
        assert_eq!(pairs[2], ("srt".into(), "sco".into()));
        assert_eq!(pairs[3], ("sp".into(), "rwdlacup".into()));
        assert_eq!(pairs[4], ("se".into(), "2025-01-01".into()));
    }

    #[test]
    fn parse_sas_without_leading_question_mark() {
        let pairs = parse_sas_query_pairs("sv=2021-06-08&ss=b");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("sv".into(), "2021-06-08".into()));
        assert_eq!(pairs[1], ("ss".into(), "b".into()));
    }

    #[test]
    fn parse_sas_empty_string() {
        let pairs = parse_sas_query_pairs("");
        assert!(pairs.is_empty());
    }

    #[test]
    fn parse_sas_skips_malformed_pairs() {
        let pairs = parse_sas_query_pairs("good=val&bad&also=ok");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("good".into(), "val".into()));
        assert_eq!(pairs[1], ("also".into(), "ok".into()));
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: these tests restore every mutated variable before returning.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see EnvGuard::set.
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
