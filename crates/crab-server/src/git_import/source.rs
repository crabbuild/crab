use super::ImportError;
use serde::Deserialize;
use url::{Host, Url};
const MAX_TOKEN_BYTES: usize = 512;
const MAX_DESCRIPTION_CHARS: usize = 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitImportRequest {
    source: String,
    owner: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Clone)]
pub(super) struct ValidatedImport {
    pub(super) source: String,
    pub(super) owner: String,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) token: Option<String>,
}

pub(super) fn validate_request(
    request: GitImportRequest,
    allowed_hosts: &[String],
) -> Result<ValidatedImport, ImportError> {
    let source = normalize_source(&request.source, allowed_hosts)?;
    let owner = identifier(request.owner, "owner")?;
    let name = identifier(request.name, "name")?;
    let description = request.description.unwrap_or_default().trim().to_owned();
    if description.chars().count() > MAX_DESCRIPTION_CHARS
        || description.chars().any(char::is_control)
    {
        return Err(ImportError::Invalid(
            "description must be at most 1,000 characters without controls",
        ));
    }
    let token = request
        .token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    if token.as_deref().is_some_and(|token| {
        token.len() > MAX_TOKEN_BYTES || token.bytes().any(|byte| !byte.is_ascii_graphic())
    }) {
        return Err(ImportError::Invalid("the Git source token is invalid"));
    }
    if token.is_some() && !source.starts_with("https://") {
        return Err(ImportError::Invalid(
            "source tokens require an HTTPS Git URL",
        ));
    }
    Ok(ValidatedImport {
        source,
        owner,
        name,
        description,
        token,
    })
}

fn normalize_source(value: &str, allowed_hosts: &[String]) -> Result<String, ImportError> {
    let value = value.trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    if let Some(path) = value
        .strip_prefix("github.com:")
        .or_else(|| value.strip_prefix("git@github.com:"))
    {
        return normalize_github_path(path, allowed_hosts);
    }
    if !value.contains("://") && !value.starts_with("git@") {
        let mut segments = value.split('/');
        let owner = segments.next().unwrap_or_default();
        let name = segments.next().unwrap_or_default();
        if segments.next().is_none() && valid_identifier(owner) && valid_identifier(name) {
            return normalize_github_path(value, allowed_hosts);
        }
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    if let Some(rest) = value.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return normalize_ssh_source(host, path, allowed_hosts);
    }
    let url = Url::parse(value)
        .map_err(|_| ImportError::Invalid("source must be an allowed HTTPS or SSH clone URL"))?;
    if !matches!(url.scheme(), "https" | "ssh")
        || url.host_str().is_none()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    let host = url
        .host_str()
        .ok_or(ImportError::Invalid("source host is required"))?;
    if has_dot_path_segment(value) {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    let host = normalize_host(host)?;
    require_allowed_host(&host, allowed_hosts)?;
    if url.scheme() == "https" && !url.username().is_empty() {
        return Err(ImportError::Invalid(
            "HTTPS source URLs must not contain a username",
        ));
    }
    if url.scheme() == "ssh" && !url.username().is_empty() && url.username() != "git" {
        return Err(ImportError::Invalid(
            "SSH source URLs must use the git user",
        ));
    }
    let path = normalize_source_path(url.path())?;
    if host == "github.com" {
        if url.scheme() == "ssh" && url.port().is_some() {
            return Err(ImportError::Invalid(
                "GitHub SSH URLs must use the default SSH port",
            ));
        }
        let port = url
            .port()
            .map_or_else(String::new, |port| format!(":{port}"));
        return Ok(format!("https://github.com{port}/{path}"));
    }
    let scheme = url.scheme();
    let user = if scheme == "ssh" { "git@" } else { "" };
    let port = url
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    let host = format_url_host(&host);
    Ok(format!("{scheme}://{user}{host}{port}/{path}"))
}

fn normalize_github_path(path: &str, allowed_hosts: &[String]) -> Result<String, ImportError> {
    require_allowed_host("github.com", allowed_hosts)?;
    let path = normalize_source_path(path)?;
    let mut segments = path.trim_end_matches(".git").split('/');
    let owner = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    if segments.next().is_some() || !valid_identifier(owner) || !valid_identifier(name) {
        return Err(ImportError::Invalid(
            "GitHub shorthand must be owner/repository",
        ));
    }
    Ok(format!("https://github.com/{owner}/{name}.git"))
}

fn normalize_ssh_source(
    host: &str,
    path: &str,
    allowed_hosts: &[String],
) -> Result<String, ImportError> {
    let host = normalize_host(host)?;
    require_allowed_host(&host, allowed_hosts)?;
    let path = normalize_source_path(path)?;
    if host == "github.com" {
        return normalize_github_path(&path, allowed_hosts);
    }
    Ok(format!("ssh://git@{}/{path}", format_url_host(&host)))
}

fn format_url_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn has_dot_path_segment(value: &str) -> bool {
    let Some(scheme_end) = value.find("://") else {
        return false;
    };
    let authority_start = scheme_end + 3;
    let Some(path_offset) = value[authority_start..].find('/') else {
        return false;
    };
    let path_start = authority_start + path_offset;
    let path_end = value[path_start..]
        .find(['?', '#'])
        .map_or(value.len(), |offset| path_start + offset);
    value[path_start..path_end]
        .split('/')
        .any(is_dot_path_segment)
}

fn is_dot_path_segment(segment: &str) -> bool {
    let mut decoded = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            decoded.push(char::from(high * 16 + low));
            index += 3;
        } else {
            decoded.push(char::from(bytes[index]));
            index += 1;
        }
    }
    matches!(decoded.as_str(), "." | "..")
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn normalize_host(value: &str) -> Result<String, ImportError> {
    let host = Host::parse(value).map_err(|_| ImportError::Invalid("source host is invalid"))?;
    let normalized = match host {
        Host::Domain(value) => value.to_ascii_lowercase(),
        Host::Ipv4(value) => value.to_string(),
        Host::Ipv6(value) => value.to_string(),
    };
    if normalized.is_empty() || normalized.len() > 253 {
        return Err(ImportError::Invalid("source host is invalid"));
    }
    Ok(normalized)
}

fn require_allowed_host(host: &str, allowed_hosts: &[String]) -> Result<(), ImportError> {
    if allowed_hosts.iter().any(|allowed| allowed == host) {
        Ok(())
    } else {
        Err(ImportError::Invalid(
            "source host is not enabled by the server import allowlist",
        ))
    }
}

fn normalize_source_path(value: &str) -> Result<String, ImportError> {
    let path = value.trim_matches('/');
    if path.is_empty()
        || path.len() > 4_096
        || path.chars().any(char::is_control)
        || path.bytes().any(|byte| matches!(byte, b'?' | b'#'))
    {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty()
        || path
            .split('/')
            .any(|segment| segment.is_empty() || is_dot_path_segment(segment))
    {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    Ok(format!("{path}.git"))
}

#[cfg(test)]
fn normalize_github_source(value: &str) -> Result<String, ImportError> {
    normalize_source(value, &["github.com".to_owned()])
}

fn identifier(value: String, field: &'static str) -> Result<String, ImportError> {
    let value = value.trim().to_owned();
    if !valid_identifier(&value)
        || (field == "owner" && matches!(value.as_str(), "api" | "assets" | "auth" | "git"))
    {
        return Err(ImportError::Invalid(
            "owner and name must be URL-safe identifiers",
        ));
    }
    Ok(value)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !matches!(value, "." | "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_github_locators_without_credentials() {
        for value in [
            "denoland/celld",
            "https://github.com/denoland/celld.git",
            "git@github.com:denoland/celld.git",
            "ssh://git@github.com/denoland/celld",
        ] {
            assert_eq!(
                normalize_github_source(value).unwrap(),
                "https://github.com/denoland/celld.git"
            );
        }
    }

    #[test]
    fn rejects_source_credentials_and_non_github_hosts() {
        for value in [
            "https://user:secret@github.com/team/repo",
            "https://github.com/team/repo?token=secret",
            "https://gitlab.com/team/repo",
            "team/repo/extra",
        ] {
            assert!(normalize_github_source(value).is_err(), "{value}");
        }
    }

    #[test]
    fn accepts_allowlisted_git_hosts_and_rejects_untrusted_transports() {
        let allowed = ["github.com".to_owned(), "gitlab.com".to_owned()];
        assert_eq!(
            normalize_source("https://gitlab.com/group/repo", &allowed).unwrap(),
            "https://gitlab.com/group/repo.git"
        );
        assert_eq!(
            normalize_source("https://gitlab.com/group/subgroup/repo.git", &allowed,).unwrap(),
            "https://gitlab.com/group/subgroup/repo.git"
        );
        assert_eq!(
            normalize_source("git@gitlab.com:group/repo.git", &allowed).unwrap(),
            "ssh://git@gitlab.com/group/repo.git"
        );
        let ipv6 = ["2001:db8::7".to_owned()];
        assert_eq!(
            normalize_source("ssh://git@[2001:db8::7]:2222/group/repo", &ipv6).unwrap(),
            "ssh://git@[2001:db8::7]:2222/group/repo.git"
        );
        for value in [
            "https://gitlab.com/group/repo?token=secret",
            "https://gitlab.com/group/../repo",
            "https://gitlab.com/group/%2e%2e/repo",
            "git@gitlab.com:group/repo?token=secret",
            "git://gitlab.com/group/repo",
            "https://example.com/group/repo",
        ] {
            assert!(normalize_source(value, &allowed).is_err(), "{value}");
        }
    }
}
