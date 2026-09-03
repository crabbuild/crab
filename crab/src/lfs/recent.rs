//! Git LFS recent-ref and recent-commit selection helpers.

use std::path::Path;
use std::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::{self, MAX_CAPTURE_BYTES};

/// Returns recent local and optionally remote ref tips.
///
/// `extra_days` is added to `lfs.fetchrecentrefsdays`, matching prune's
/// offset-based retention window.
pub(crate) fn recent_ref_oids(extra_days: u64, cancel: &CancellationToken) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_ref_oids_in(&root, extra_days, cancel)
}

pub(crate) fn git_config_u64(key: &str, default: u64, cancel: &CancellationToken) -> Result<u64> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    git_config_u64_in(&root, key, default, cancel)
}

pub(crate) fn recent_ref_oids_in(
    repo_root: &Path,
    extra_days: u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let fetch_days = git_config_u64_in(repo_root, "lfs.fetchrecentrefsdays", 7, cancel)?;
    if fetch_days == 0 {
        return Ok(Vec::new());
    }
    let include_remotes = git_config_bool_in(repo_root, "lfs.fetchrecentremoterefs", true, cancel)?;
    let cutoff = cutoff_unix(fetch_days.saturating_add(extra_days));

    let mut args = vec![
        "for-each-ref",
        "--format=%(objectname) %(committerdate:unix)",
        "refs/heads",
    ];
    if include_remotes {
        args.push("refs/remotes");
    }

    let output = git_output(repo_root, &args, cancel)?;

    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(parse_recent_ref_oids(
        &String::from_utf8_lossy(&output.stdout),
        cutoff,
    ))
}

/// Returns commits inside `lfs.fetchrecentcommitsdays` reachable from revisions.
pub(crate) fn recent_commit_oids(
    revisions: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_commit_oids_in(&root, revisions, cancel)
}

pub(crate) fn recent_commit_oids_in(
    repo_root: &Path,
    revisions: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    if revisions.is_empty() {
        return Ok(Vec::new());
    }

    let days = git_config_u64_in(repo_root, "lfs.fetchrecentcommitsdays", 0, cancel)?;
    if days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = cutoff_unix(days);

    let mut args = vec!["rev-list", "--timestamp"];
    args.extend(revisions.iter().map(String::as_str));

    let output = git_output(repo_root, &args, cancel)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if rev_list_can_be_empty(&stderr) {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    Ok(parse_timestamped_commit_oids(
        &String::from_utf8_lossy(&output.stdout),
        cutoff,
    ))
}

pub(crate) fn git_config_u64_in(
    repo_root: &Path,
    key: &str,
    default: u64,
    cancel: &CancellationToken,
) -> Result<u64> {
    let output = git_output(
        repo_root,
        &["config", "--type", "int", "--get", key],
        cancel,
    )?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(default);
        }
        return Err(CrabError::Internal(format!(
            "git config failed while reading {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    value.parse::<u64>().map_err(|e| CrabError::Configuration {
        key: key.to_owned(),
        origin: format!("expected unsigned integer, got {value}: {e}"),
    })
}

fn git_config_bool_in(
    repo_root: &Path,
    key: &str,
    default: bool,
    cancel: &CancellationToken,
) -> Result<bool> {
    let output = git_output(
        repo_root,
        &["config", "--type", "bool", "--get", key],
        cancel,
    )?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(default);
        }
        return Err(CrabError::Internal(format!(
            "git config failed while reading {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    match value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("expected boolean, got {value}"),
        }),
    }
}

fn git_output(
    repo_root: &Path,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<process::Output<Vec<u8>>> {
    let mut command = Command::new("git");
    command.args(args).current_dir(repo_root);
    // Selection holds up both fetch and prune. Join the owned Git process
    // and both bounded pipes before cancellation returns to either caller.
    process::run(
        command,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| Ok(process::capture_output(stdout, MAX_CAPTURE_BYTES)?),
    )
}

pub(crate) fn rev_list_can_be_empty(stderr: &str) -> bool {
    stderr.contains("bad default revision")
        || stderr.contains("does not have any commits")
        || stderr.contains("ambiguous argument")
        || stderr.contains("unknown revision")
}

fn cutoff_unix(days: u64) -> u64 {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();
    now_secs.saturating_sub(days.saturating_mul(24 * 60 * 60))
}

fn parse_recent_ref_oids(output: &str, cutoff_unix: u64) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (oid, timestamp) = line.split_once(' ')?;
            let timestamp = timestamp.trim().parse::<u64>().ok()?;
            if timestamp < cutoff_unix {
                return None;
            }
            Some(oid.trim().to_owned())
        })
        .filter(|oid| !oid.is_empty())
        .collect()
}

fn parse_timestamped_commit_oids(output: &str, cutoff_unix: u64) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (timestamp, oid) = line.split_once(' ')?;
            let timestamp = timestamp.trim().parse::<u64>().ok()?;
            if timestamp < cutoff_unix {
                return None;
            }
            Some(oid.trim().to_owned())
        })
        .filter(|oid| !oid.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_git(root: &Path, args: &[&str]) -> String {
        let output = git_output(root, args, &CancellationToken::new()).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn cancelled_recent_queries_stop_before_repository_access() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let root = Path::new("missing-recent-query-repository");
        assert!(matches!(
            recent_ref_oids_in(root, 0, &cancel),
            Err(CrabError::Cancelled)
        ));
        for revisions in [vec![], vec!["HEAD".to_owned()]] {
            assert!(matches!(
                recent_commit_oids_in(root, &revisions, &cancel),
                Err(CrabError::Cancelled)
            ));
        }
        assert!(matches!(
            git_config_u64_in(root, "lfs.pruneoffsetdays", 3, &cancel),
            Err(CrabError::Cancelled)
        ));
        assert!(matches!(
            git_config_bool_in(root, "lfs.fetchrecentremoterefs", true, &cancel),
            Err(CrabError::Cancelled)
        ));
    }

    #[test]
    fn typed_config_preserves_defaults_and_rejects_invalid_values() {
        let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(root, &["init", "-q"]);
        let cancel = CancellationToken::new();
        let key = "crab.recentQueryFixture";
        assert_eq!(git_config_u64_in(root, key, 7, &cancel).unwrap(), 7);
        assert!(git_config_bool_in(root, key, true, &cancel).unwrap());
        for (value, expected) in [("0", 0), ("2k", 2048)] {
            fixture_git(root, &["config", key, value]);
            assert_eq!(git_config_u64_in(root, key, 7, &cancel).unwrap(), expected);
        }
        fixture_git(root, &["config", key, "off"]);
        assert!(!git_config_bool_in(root, key, true, &cancel).unwrap());
        for value in ["-1", "invalid"] {
            fixture_git(root, &["config", key, value]);
            assert!(git_config_u64_in(root, key, 7, &cancel).is_err());
        }
        assert!(git_config_bool_in(root, key, true, &cancel).is_err());
    }

    #[test]
    fn supervised_recent_queries_preserve_ref_and_commit_selection() {
        let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(root, &["init", "-q"]);
        for args in [
            ["config", "user.name", "Fixture"],
            ["config", "user.email", "fixture@example.invalid"],
            ["config", "commit.gpgsign", "false"],
            ["config", "lfs.fetchrecentrefsdays", "7"],
            ["config", "lfs.fetchrecentcommitsdays", "7"],
            ["config", "lfs.fetchrecentremoterefs", "false"],
        ] {
            fixture_git(root, &args);
        }
        fixture_git(root, &["commit", "--allow-empty", "-qm", "first"]);
        let first = fixture_git(root, &["rev-parse", "HEAD"]);
        fixture_git(root, &["update-ref", "refs/remotes/origin/other", &first]);
        fixture_git(root, &["commit", "--allow-empty", "-qm", "second"]);
        let second = fixture_git(root, &["rev-parse", "HEAD"]);
        let cancel = CancellationToken::new();
        assert_eq!(
            recent_ref_oids_in(root, 0, &cancel).unwrap(),
            [second.clone()]
        );
        fixture_git(root, &["config", "lfs.fetchrecentremoterefs", "true"]);
        let mut refs = recent_ref_oids_in(root, 0, &cancel).unwrap();
        refs.sort();
        let mut expected = vec![first.clone(), second.clone()];
        expected.sort();
        assert_eq!(refs, expected);
        assert_eq!(
            recent_commit_oids_in(root, &["HEAD".to_owned()], &cancel).unwrap(),
            [second, first]
        );
        fixture_git(root, &["config", "lfs.fetchrecentrefsdays", "0"]);
        assert!(recent_ref_oids_in(root, 3, &cancel).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn recent_query_output_is_bounded_on_both_pipes() {
        let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        for redirect in ["", " >&2"] {
            let alias = format!("alias.recent-query=!head -c 67108865 /dev/zero{redirect}");
            let error = git_output(
                temporary.path(),
                &["-c", &alias, "recent-query"],
                &CancellationToken::new(),
            )
            .err()
            .expect("oversized output must fail");
            assert!(
                matches!(error, CrabError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn stalled_recent_query_is_cancelled_and_joined() {
        use std::time::{Duration, Instant};
        let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let ready = root.join("ready");
        let cancel = CancellationToken::new();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                git_output(
                    root,
                    &[
                        "-c",
                        "alias.recent-query=!echo $$ > ready; exec sleep 30",
                        "recent-query",
                    ],
                    &cancel,
                )
            });
            let started = Instant::now();
            let mut pid = None;
            while started.elapsed() < Duration::from_secs(15) {
                pid = std::fs::read_to_string(&ready)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if pid.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            cancel.cancel();
            assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
            let pid = pid.expect("query must have started").to_string();
            let status = Command::new("kill")
                .args(["-0", &pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success(), "cancelled Git descendant is still alive");
            assert!(started.elapsed() < Duration::from_secs(25));
        });
    }

    #[test]
    fn parse_recent_ref_oids_filters_by_cutoff() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 100
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 90
cccccccccccccccccccccccccccccccccccccccc bad
";

        let refs = parse_recent_ref_oids(output, 100);

        assert_eq!(refs, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
    }

    #[test]
    fn parse_timestamped_commit_oids_filters_by_cutoff() {
        let output = "\
100 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
90 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
bad cccccccccccccccccccccccccccccccccccccccc
";

        let commits = parse_timestamped_commit_oids(output, 100);

        assert_eq!(commits, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
    }

    #[test]
    fn rev_list_empty_errors_are_non_fatal() {
        assert!(rev_list_can_be_empty("fatal: bad default revision 'HEAD'"));
        assert!(rev_list_can_be_empty(
            "fatal: your current branch does not have any commits"
        ));
        assert!(rev_list_can_be_empty(
            "fatal: ambiguous argument 'refs/stash'"
        ));
        assert!(!rev_list_can_be_empty("fatal: object database is corrupt"));
    }
}
