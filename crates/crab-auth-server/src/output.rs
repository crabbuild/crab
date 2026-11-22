use serde::Serialize;

use crate::error::{AuthServerError, Result};

/// Selects the shipped helper output policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperOutputPolicy {
    Receive,
    View,
    ViewRetention,
}

/// Rendered helper process output.
#[derive(Debug, Eq, PartialEq)]
pub struct RenderedHelperOutput {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: i32,
}

/// Renders a JSON helper result into process output and an exit code.
pub fn render_json_result<T: Serialize>(
    policy: HelperOutputPolicy,
    result: Result<T>,
) -> RenderedHelperOutput {
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(json) => RenderedHelperOutput {
                stdout: Some(json),
                stderr: None,
                exit_code: 0,
            },
            Err(error) => render_error(policy, serialize_error(policy, error)),
        },
        Err(error) => render_error(policy, error),
    }
}

/// Emits a JSON helper result to stdout/stderr and returns the process exit code.
pub fn emit_json_result<T: Serialize>(policy: HelperOutputPolicy, result: Result<T>) -> i32 {
    let rendered = render_json_result(policy, result);
    if let Some(stdout) = rendered.stdout {
        println!("{stdout}");
    }
    if let Some(stderr) = rendered.stderr {
        eprintln!("{stderr}");
    }
    rendered.exit_code
}

fn render_error(policy: HelperOutputPolicy, error: AuthServerError) -> RenderedHelperOutput {
    let (prefix, exit_code) = error_status(policy, &error);
    let stderr = if policy == HelperOutputPolicy::ViewRetention {
        format!("{prefix}: view retention request failed")
    } else {
        format!("{prefix}: {error}")
    };
    RenderedHelperOutput {
        stdout: None,
        stderr: Some(stderr),
        exit_code,
    }
}

fn serialize_error(policy: HelperOutputPolicy, error: serde_json::Error) -> AuthServerError {
    let message = match policy {
        HelperOutputPolicy::Receive => format!("JSON serialize: {error}"),
        HelperOutputPolicy::View | HelperOutputPolicy::ViewRetention => {
            format!("failed to encode view output: {error}")
        }
    };
    AuthServerError::Internal(message)
}

fn error_status(policy: HelperOutputPolicy, error: &AuthServerError) -> (&'static str, i32) {
    match (policy, error) {
        (HelperOutputPolicy::ViewRetention, error) if retention_conflict(error) => ("conflict", 2),
        (
            HelperOutputPolicy::Receive,
            AuthServerError::CasConflict { .. } | AuthServerError::NonFastForward { .. },
        ) => ("conflict", 2),
        (
            HelperOutputPolicy::Receive,
            AuthServerError::CorruptObject { .. }
            | AuthServerError::NotFound { .. }
            | AuthServerError::Configuration { .. },
        ) => ("invalid", 1),
        _ => ("error", 1),
    }
}

fn retention_conflict(error: &AuthServerError) -> bool {
    use crab_coordination::CoordinationError;
    use object_store::Error;

    match error {
        AuthServerError::NotFound { .. } | AuthServerError::CasConflict { .. } => true,
        AuthServerError::Read(source) => matches!(
            source.as_ref(),
            crab_read::ReadError::Retention(
                CoordinationError::ReadRetentionLost { .. }
                    | CoordinationError::ObjectStore {
                        source: Error::NotFound { .. }
                            | Error::Precondition { .. }
                            | Error::AlreadyExists { .. },
                        ..
                    }
            )
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct Status<'a> {
        status: &'a str,
    }

    #[test]
    fn success_renders_json_stdout_and_zero_exit() {
        let rendered = render_json_result(HelperOutputPolicy::Receive, Ok(Status { status: "ok" }));

        assert_eq!(rendered.stdout.as_deref(), Some(r#"{"status":"ok"}"#));
        assert_eq!(rendered.stderr, None);
        assert_eq!(rendered.exit_code, 0);
    }

    #[test]
    fn receive_conflict_uses_conflict_prefix_and_exit_two() {
        let rendered = render_json_result::<Status<'_>>(
            HelperOutputPolicy::Receive,
            Err(AuthServerError::CasConflict {
                path: "manifest.json".to_owned(),
                expected_etag: Some("etag".to_owned()),
            }),
        );

        assert_eq!(
            rendered.stderr.as_deref(),
            Some("conflict: CAS conflict at manifest.json")
        );
        assert_eq!(rendered.exit_code, 2);
    }

    #[test]
    fn receive_configuration_uses_invalid_prefix_and_exit_one() {
        let rendered = render_json_result::<Status<'_>>(
            HelperOutputPolicy::Receive,
            Err(AuthServerError::Configuration {
                key: "repo_url".to_owned(),
                origin: "missing bucket".to_owned(),
            }),
        );

        assert_eq!(
            rendered.stderr.as_deref(),
            Some("invalid: configuration error in missing bucket: repo_url")
        );
        assert_eq!(rendered.exit_code, 1);
    }

    #[test]
    fn view_errors_keep_plain_error_prefix() {
        let rendered = render_json_result::<Status<'_>>(
            HelperOutputPolicy::View,
            Err(AuthServerError::Configuration {
                key: "repo_url".to_owned(),
                origin: "missing bucket".to_owned(),
            }),
        );

        assert_eq!(
            rendered.stderr.as_deref(),
            Some("error: configuration error in missing bucket: repo_url")
        );
        assert_eq!(rendered.exit_code, 1);
    }

    #[test]
    fn view_retention_distinguishes_conflicts_and_redacts_backend_details() {
        let expired = crab_coordination::CoordinationError::ReadRetentionLost {
            path: "private claim path".into(),
        };
        for (error, code, kind) in [
            (
                AuthServerError::from(crab_read::ReadError::from(expired)),
                2,
                "conflict",
            ),
            (
                AuthServerError::Internal("private backend diagnostic".into()),
                1,
                "error",
            ),
        ] {
            let reply =
                render_json_result::<Status<'_>>(HelperOutputPolicy::ViewRetention, Err(error));
            assert_eq!(reply.exit_code, code);
            assert!(reply.stdout.is_none());
            assert_eq!(
                reply.stderr,
                Some(format!("{kind}: view retention request failed"))
            );
        }
    }
}
