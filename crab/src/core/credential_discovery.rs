//! Credential discovery chain for cloud storage backends.
//!
//! Probes credentials in priority order:
//! 1. Project config (`[auth]` section in `.crab.toml`)
//! 2. Environment variables (AWS_PROFILE, GOOGLE_APPLICATION_CREDENTIALS, etc.)
//! 3. Cloud SDK default configs (~/.aws/config, gcloud, az)
//! 4. Instance metadata (EC2 IMDS with 200ms timeout)
//!
//! The discovery chain is read-only — it never writes credentials, creates
//! files, or modifies cloud SDK state.

use std::path::PathBuf;

use crate::core::project_config::ProjectAuthConfig;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where the discovered credential came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// Explicit `[auth]` section in `.crab.toml`.
    ProjectConfig,
    /// Environment variables (AWS_PROFILE, AWS_ACCESS_KEY_ID, etc.).
    Environment,
    /// Cloud SDK config files (~/.aws/config, gcloud, az).
    CloudSdk,
    /// Instance metadata service (EC2 IMDS, GCE metadata).
    InstanceMetadata,
    /// No credentials found.
    None,
}

/// Result of a credential discovery probe.
#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    /// Which source provided the credential.
    pub source: CredentialSource,
    /// Human-readable description of what was found and where.
    pub description: String,
    /// Whether the credential passed a quick validation check.
    pub valid: bool,
}

// ---------------------------------------------------------------------------
// Cloud provider detection
// ---------------------------------------------------------------------------

/// Cloud provider inferred from a remote URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloudProvider {
    Aws,
    Gcp,
    Azure,
}

/// Detect the cloud provider from a URL scheme.
fn detect_provider(url: &str) -> Option<CloudProvider> {
    let lower = url.to_lowercase();
    if lower.starts_with("crab://") || lower.starts_with("s3://") {
        Some(CloudProvider::Aws)
    } else if lower.starts_with("gs://") {
        Some(CloudProvider::Gcp)
    } else if lower.starts_with("az://") || lower.starts_with("azure://") {
        Some(CloudProvider::Azure)
    } else {
        Option::None
    }
}

// ---------------------------------------------------------------------------
// Check: project config
// ---------------------------------------------------------------------------

fn check_project_config(auth: Option<&ProjectAuthConfig>) -> Option<DiscoveryResult> {
    let auth = auth?;

    if auth.provider.is_none() && auth.profile.is_none() {
        return Option::None;
    }

    let provider = auth.provider.as_deref().unwrap_or("unknown");
    let profile = auth.profile.as_deref().unwrap_or("default");

    Some(DiscoveryResult {
        source: CredentialSource::ProjectConfig,
        description: format!("project config: provider={provider}, profile={profile}"),
        valid: true,
    })
}

// ---------------------------------------------------------------------------
// Check: environment variables
// ---------------------------------------------------------------------------

/// Check environment variables for cloud credentials based on URL scheme.
pub fn check_environment_vars(url: &str) -> Option<DiscoveryResult> {
    let provider = detect_provider(url)?;

    match provider {
        CloudProvider::Aws => check_aws_env_vars(),
        CloudProvider::Gcp => check_gcp_env_vars(),
        CloudProvider::Azure => check_azure_env_vars(),
    }
}

fn check_aws_env_vars() -> Option<DiscoveryResult> {
    // Check AWS_PROFILE first (named profile)
    if let Ok(profile) = std::env::var("AWS_PROFILE") {
        return Some(DiscoveryResult {
            source: CredentialSource::Environment,
            description: format!("AWS_PROFILE={profile}"),
            valid: true,
        });
    }

    // Check explicit access key pair
    let key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
    let _secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;

    // Mask the key ID for display
    let masked = if key_id.len() > 8 {
        format!("{}...{}", &key_id[..4], &key_id[key_id.len() - 4..])
    } else {
        "****".to_string()
    };

    Some(DiscoveryResult {
        source: CredentialSource::Environment,
        description: format!("AWS_ACCESS_KEY_ID={masked}"),
        valid: true,
    })
}

fn check_gcp_env_vars() -> Option<DiscoveryResult> {
    let creds_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok()?;

    // Verify the file exists
    let path = PathBuf::from(&creds_path);
    if !path.exists() {
        return Some(DiscoveryResult {
            source: CredentialSource::Environment,
            description: format!("GOOGLE_APPLICATION_CREDENTIALS={creds_path} (file not found)"),
            valid: false,
        });
    }

    Some(DiscoveryResult {
        source: CredentialSource::Environment,
        description: format!("GOOGLE_APPLICATION_CREDENTIALS={creds_path}"),
        valid: true,
    })
}

fn check_azure_env_vars() -> Option<DiscoveryResult> {
    let account = std::env::var("AZURE_STORAGE_ACCOUNT").ok()?;
    let _key = std::env::var("AZURE_STORAGE_KEY").ok()?;

    Some(DiscoveryResult {
        source: CredentialSource::Environment,
        description: format!("AZURE_STORAGE_ACCOUNT={account}"),
        valid: true,
    })
}

// ---------------------------------------------------------------------------
// Check: cloud SDK config files
// ---------------------------------------------------------------------------

/// Check cloud SDK configuration files for credentials.
pub fn check_cloud_sdk(url: &str) -> Option<DiscoveryResult> {
    let provider = detect_provider(url)?;

    match provider {
        CloudProvider::Aws => check_aws_sdk(),
        CloudProvider::Gcp => check_gcp_sdk(),
        CloudProvider::Azure => check_azure_sdk(),
    }
}

fn home_dir() -> Option<PathBuf> {
    dirs_or_env()
}

/// Resolve the user's home directory from env vars (HOME on Unix,
/// USERPROFILE on Windows). We avoid the `dirs` crate dependency.
fn dirs_or_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn check_aws_sdk() -> Option<DiscoveryResult> {
    let home = home_dir()?;

    let config_path = home.join(".aws").join("config");
    let credentials_path = home.join(".aws").join("credentials");

    // Both files must exist
    if !config_path.exists() && !credentials_path.exists() {
        return Option::None;
    }

    // Check for a [default] section in either file
    let has_default = has_ini_section(&config_path, "[default]")
        || has_ini_section(&credentials_path, "[default]");

    if has_default {
        Some(DiscoveryResult {
            source: CredentialSource::CloudSdk,
            description: "AWS SDK: profile 'default' from ~/.aws/config".to_string(),
            valid: true,
        })
    } else {
        Some(DiscoveryResult {
            source: CredentialSource::CloudSdk,
            description: "AWS SDK: ~/.aws/config exists but no [default] section".to_string(),
            valid: false,
        })
    }
}

/// Check if an INI-style file contains a given section header.
fn has_ini_section(path: &PathBuf, section: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| line.trim() == section)
}

fn check_gcp_sdk() -> Option<DiscoveryResult> {
    let home = home_dir()?;
    let adc_path = home
        .join(".config")
        .join("gcloud")
        .join("application_default_credentials.json");

    if adc_path.exists() {
        Some(DiscoveryResult {
            source: CredentialSource::CloudSdk,
            description: "GCP SDK: application default credentials from ~/.config/gcloud/"
                .to_string(),
            valid: true,
        })
    } else {
        Option::None
    }
}

fn check_azure_sdk() -> Option<DiscoveryResult> {
    let home = home_dir()?;
    let profile_path = home.join(".azure").join("azureProfile.json");

    if profile_path.exists() {
        Some(DiscoveryResult {
            source: CredentialSource::CloudSdk,
            description: "Azure SDK: profile from ~/.azure/azureProfile.json".to_string(),
            valid: true,
        })
    } else {
        Option::None
    }
}

// ---------------------------------------------------------------------------
// Check: instance metadata (IMDS)
// ---------------------------------------------------------------------------

/// Check EC2 instance metadata service for credentials.
///
/// Sends a PUT request to the IMDSv2 token endpoint with a 200ms timeout.
/// If the endpoint responds with 200 OK, the instance has IAM role
/// credentials available.
pub async fn check_instance_metadata(url: &str) -> Option<DiscoveryResult> {
    let provider = detect_provider(url)?;

    // Currently only AWS IMDS is implemented
    if provider != CloudProvider::Aws {
        return Option::None;
    }

    check_aws_imds().await
}

async fn check_aws_imds() -> Option<DiscoveryResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(200))
        .build()
        .ok()?;

    // IMDSv2 requires a PUT to get a session token
    let result = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => Some(DiscoveryResult {
            source: CredentialSource::InstanceMetadata,
            description: "EC2 instance metadata (IMDSv2): IAM role credentials available"
                .to_string(),
            valid: true,
        }),
        _ => Option::None,
    }
}

// ---------------------------------------------------------------------------
// Main discovery function
// ---------------------------------------------------------------------------

/// Probe credentials in priority order, returning the first that works.
///
/// Priority:
/// 1. Project config (`[auth]` section)
/// 2. Environment variables
/// 3. Cloud SDK config files
/// 4. Instance metadata (IMDS)
///
/// Returns `CredentialSource::None` with a helpful message if nothing found.
pub async fn discover_credentials(
    url: &str,
    project_auth: Option<&ProjectAuthConfig>,
) -> DiscoveryResult {
    // 1. Project config
    if let Some(result) = check_project_config(project_auth) {
        tracing::debug!(source = "project_config", "credential discovered");
        return result;
    }

    // 2. Environment variables
    if let Some(result) = check_environment_vars(url)
        && result.valid
    {
        tracing::debug!(source = "environment", "credential discovered");
        return result;
    }

    // 3. Cloud SDK config files
    if let Some(result) = check_cloud_sdk(url)
        && result.valid
    {
        tracing::debug!(source = "cloud_sdk", "credential discovered");
        return result;
    }

    // 4. Instance metadata
    if let Some(result) = check_instance_metadata(url).await {
        tracing::debug!(source = "instance_metadata", "credential discovered");
        return result;
    }

    // Nothing found — provide helpful instructions
    let provider = detect_provider(url);
    let instructions = match provider {
        Some(CloudProvider::Aws) => {
            "No AWS credentials found. Try: aws configure, or set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY"
        }
        Some(CloudProvider::Gcp) => {
            "No GCP credentials found. Try: gcloud auth application-default login, or set GOOGLE_APPLICATION_CREDENTIALS"
        }
        Some(CloudProvider::Azure) => {
            "No Azure credentials found. Try: az login, or set AZURE_STORAGE_ACCOUNT + AZURE_STORAGE_KEY"
        }
        None => {
            "Could not determine cloud provider from URL scheme. Supported: crab://, s3://, gs://, az://, azure://"
        }
    };

    DiscoveryResult {
        source: CredentialSource::None,
        description: instructions.to_string(),
        valid: false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // Holds ENV_MUTEX for the full scope because env vars are process-global
    // and discovery reads them after setup.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        vars: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&str, &str)]) -> Self {
            Self::apply(vars, &[])
        }

        fn clear(vars: &[&str]) -> Self {
            Self::apply(&[], vars)
        }

        fn apply(set_vars: &[(&str, &str)], clear_vars: &[&str]) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for key in clear_vars {
                saved.push((key.to_string(), std::env::var(key).ok()));
                // SAFETY: ENV_MUTEX is held for the guard lifetime.
                unsafe { std::env::remove_var(key) };
            }
            for (key, value) in set_vars {
                saved.push((key.to_string(), std::env::var(key).ok()));
                // SAFETY: ENV_MUTEX is held for the guard lifetime.
                unsafe { std::env::set_var(key, value) };
            }
            EnvGuard {
                _lock: lock,
                vars: saved,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, original) in &self.vars {
                match original {
                    // SAFETY: ENV_MUTEX is still held while restoring.
                    Some(val) => unsafe { std::env::set_var(key, val) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[test]
    fn detect_provider_aws_crab_scheme() {
        assert_eq!(
            detect_provider("crab://bucket/repo"),
            Some(CloudProvider::Aws)
        );
    }

    #[test]
    fn detect_provider_aws_s3_scheme() {
        assert_eq!(
            detect_provider("s3://bucket/repo"),
            Some(CloudProvider::Aws)
        );
    }

    #[test]
    fn detect_provider_gcp() {
        assert_eq!(
            detect_provider("gs://bucket/repo"),
            Some(CloudProvider::Gcp)
        );
    }

    #[test]
    fn detect_provider_azure() {
        assert_eq!(
            detect_provider("az://container/path"),
            Some(CloudProvider::Azure)
        );
        assert_eq!(
            detect_provider("azure://container/path"),
            Some(CloudProvider::Azure)
        );
    }

    #[test]
    fn detect_provider_unknown() {
        assert_eq!(detect_provider("https://example.com"), None);
        assert_eq!(detect_provider("file:///tmp/repo"), None);
    }

    #[test]
    fn env_vars_aws_profile() {
        let _guard = EnvGuard::apply(
            &[("AWS_PROFILE", "production")],
            &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"],
        );
        let result = check_environment_vars("crab://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(
            result.description.contains("production"),
            "expected 'production' in description: {}",
            result.description
        );
        assert!(result.valid);
    }

    #[test]
    fn env_vars_aws_access_key() {
        let _guard = EnvGuard::apply(
            &[
                ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
                (
                    "AWS_SECRET_ACCESS_KEY",
                    "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                ),
            ],
            &["AWS_PROFILE"],
        );
        let result = check_environment_vars("crab://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(result.description.contains("AWS_ACCESS_KEY_ID=AKIA...MPLE"));
        assert!(result.valid);
    }

    #[test]
    fn env_vars_gcp() {
        let dir = tempfile::TempDir::new().unwrap();
        let creds_file = dir.path().join("creds.json");
        std::fs::write(&creds_file, "{}").unwrap();

        let _guard = EnvGuard::set(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            creds_file.to_str().unwrap(),
        )]);
        let result = check_environment_vars("gs://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(
            result
                .description
                .contains("GOOGLE_APPLICATION_CREDENTIALS")
        );
        assert!(result.valid);
    }

    #[test]
    fn env_vars_gcp_missing_file() {
        let _guard = EnvGuard::set(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            "/nonexistent/path/creds.json",
        )]);
        let result = check_environment_vars("gs://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(!result.valid);
        assert!(result.description.contains("file not found"));
    }

    #[test]
    fn env_vars_azure() {
        let _guard = EnvGuard::set(&[
            ("AZURE_STORAGE_ACCOUNT", "myaccount"),
            ("AZURE_STORAGE_KEY", "base64key=="),
        ]);
        let result = check_environment_vars("az://container/path").unwrap();
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(
            result
                .description
                .contains("AZURE_STORAGE_ACCOUNT=myaccount")
        );
        assert!(result.valid);
    }

    #[test]
    fn env_vars_none_when_missing() {
        let _guard =
            EnvGuard::clear(&["AWS_PROFILE", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]);
        let result = check_environment_vars("crab://bucket/repo");
        assert!(result.is_none());
    }

    #[test]
    fn cloud_sdk_aws_with_default_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).unwrap();
        std::fs::write(aws_dir.join("config"), "[default]\nregion = us-east-1\n").unwrap();
        std::fs::write(
            aws_dir.join("credentials"),
            "[default]\naws_access_key_id = AKIA...\n",
        )
        .unwrap();

        // Override HOME to point to our temp dir
        let _guard = EnvGuard::apply(&[("HOME", dir.path().to_str().unwrap())], &["USERPROFILE"]);
        let result = check_cloud_sdk("crab://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::CloudSdk);
        assert!(result.valid);
        assert!(result.description.contains("default"));
    }

    #[test]
    fn cloud_sdk_aws_no_default_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).unwrap();
        std::fs::write(
            aws_dir.join("config"),
            "[profile production]\nregion = us-west-2\n",
        )
        .unwrap();

        let _guard = EnvGuard::apply(&[("HOME", dir.path().to_str().unwrap())], &["USERPROFILE"]);
        let result = check_cloud_sdk("crab://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::CloudSdk);
        assert!(!result.valid);
        assert!(result.description.contains("no [default] section"));
    }

    #[test]
    fn cloud_sdk_gcp_adc_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let gcloud_dir = dir.path().join(".config").join("gcloud");
        std::fs::create_dir_all(&gcloud_dir).unwrap();
        std::fs::write(
            gcloud_dir.join("application_default_credentials.json"),
            "{}",
        )
        .unwrap();

        let _guard = EnvGuard::apply(&[("HOME", dir.path().to_str().unwrap())], &["USERPROFILE"]);
        let result = check_cloud_sdk("gs://bucket/repo").unwrap();
        assert_eq!(result.source, CredentialSource::CloudSdk);
        assert!(result.valid);
        assert!(
            result
                .description
                .contains("application default credentials")
        );
    }

    #[test]
    fn cloud_sdk_azure_profile_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let azure_dir = dir.path().join(".azure");
        std::fs::create_dir_all(&azure_dir).unwrap();
        std::fs::write(azure_dir.join("azureProfile.json"), "{}").unwrap();

        let _guard = EnvGuard::apply(&[("HOME", dir.path().to_str().unwrap())], &["USERPROFILE"]);
        let result = check_cloud_sdk("az://container/path").unwrap();
        assert_eq!(result.source, CredentialSource::CloudSdk);
        assert!(result.valid);
        assert!(result.description.contains("azureProfile.json"));
    }

    #[test]
    fn cloud_sdk_none_when_no_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let _guard = EnvGuard::apply(&[("HOME", dir.path().to_str().unwrap())], &["USERPROFILE"]);
        let result = check_cloud_sdk("crab://bucket/repo");
        assert!(result.is_none());
    }

    #[test]
    fn project_config_check_with_auth() {
        let auth = ProjectAuthConfig {
            provider: Some("aws".to_string()),
            profile: Some("production".to_string()),
            storage_provider: None,
        };
        let result = check_project_config(Some(&auth)).unwrap();
        assert_eq!(result.source, CredentialSource::ProjectConfig);
        assert!(result.valid);
        assert!(result.description.contains("aws"));
        assert!(result.description.contains("production"));
    }

    #[test]
    fn project_config_check_none_without_auth() {
        let result = check_project_config(None);
        assert!(result.is_none());
    }

    #[test]
    fn project_config_storage_provider_is_not_credentials() {
        let auth = ProjectAuthConfig {
            provider: None,
            profile: None,
            storage_provider: Some(crate::core::config::StorageProvider::Gcs),
        };
        let result = check_project_config(Some(&auth));
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn discover_returns_project_config_first() {
        let auth = ProjectAuthConfig {
            provider: Some("aws".to_string()),
            profile: Some("dev".to_string()),
            storage_provider: None,
        };
        let result = discover_credentials("crab://bucket/repo", Some(&auth)).await;
        assert_eq!(result.source, CredentialSource::ProjectConfig);
        assert!(result.valid);
    }

    #[tokio::test]
    async fn discover_falls_back_to_env() {
        let _guard = EnvGuard::set(&[("AWS_PROFILE", "test-profile")]);
        let result = discover_credentials("crab://bucket/repo", None).await;
        assert_eq!(result.source, CredentialSource::Environment);
        assert!(result.valid);
    }

    #[tokio::test]
    async fn discover_returns_none_with_instructions() {
        let dir = tempfile::TempDir::new().unwrap();
        let _guard = EnvGuard::apply(
            &[("HOME", dir.path().to_str().unwrap())],
            &["AWS_PROFILE", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"],
        );

        let result = discover_credentials("crab://bucket/repo", None).await;
        assert_eq!(result.source, CredentialSource::None);
        assert!(!result.valid);
        assert!(result.description.contains("No AWS credentials found"));
    }

    #[tokio::test]
    async fn discover_returns_none_for_unknown_scheme() {
        let dir = tempfile::TempDir::new().unwrap();
        let _home_guard = EnvGuard::set(&[("HOME", dir.path().to_str().unwrap())]);

        let result = discover_credentials("https://example.com/repo", None).await;
        assert_eq!(result.source, CredentialSource::None);
        assert!(!result.valid);
        assert!(
            result
                .description
                .contains("Could not determine cloud provider")
        );
    }

    #[tokio::test]
    async fn imds_check_returns_none_when_not_on_ec2() {
        // This test verifies that the IMDS check times out quickly
        // when not running on an EC2 instance (which is the normal dev case).
        let result = check_instance_metadata("crab://bucket/repo").await;
        // On a dev machine, IMDS is unreachable so this should be None
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn imds_check_returns_none_for_non_aws() {
        let result = check_instance_metadata("gs://bucket/repo").await;
        assert!(result.is_none());
    }
}
