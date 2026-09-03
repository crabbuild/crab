//! Owned bare Git cache initialization and source refresh.

use crab_cache::lifecycle::{CacheUseGuard, cleanup_preview};
use tokio_util::sync::CancellationToken;

use super::{
    CommandRunner, CrabError, MirrorArgs, MirrorExecution, ORIGIN_REMOTE, Path, Result,
    check_cancelled, git_command, git_command_from_vec, parse_ref_lines, run_required,
};

pub(super) fn prepare_cache(
    args: &MirrorArgs,
    cache: &CacheUseGuard,
    cancel: &CancellationToken,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<bool> {
    let path = cache.path();
    check_cancelled(cancel)?;
    let created = needs_initialization(path, cancel)?;
    if created {
        // Git clone requires an empty target and would conflict with retained
        // lock inodes. Init plus one mirror fetch handles every new cache,
        // including marker-only skeletons, without deleting coordination files.
        let target = path.to_string_lossy();
        run_required(
            runner,
            git_command(
                ["init", "--bare", "--object-format=sha1", "--", &target],
                path.parent(),
                options,
                true,
            ),
            options.mode,
        )?;
    } else {
        validate_bare_cache(path, options, runner)?;
    }

    // Replace all three owned settings on every refresh. An interrupted first
    // initialization must resume through the same path, not leave an origin
    // missing its mirror refspec or retain stale extra source URLs.
    for (key, value) in [
        ("remote.origin.url", args.source.as_str()),
        ("remote.origin.fetch", "+refs/*:refs/*"),
        ("remote.origin.mirror", "true"),
    ] {
        check_cancelled(cancel)?;
        run_required(
            runner,
            git_command(
                ["config", "--replace-all", key, value],
                Some(path),
                options,
                false,
            ),
            options.mode,
        )?;
    }
    check_cancelled(cancel)?;
    let advertisement = run_required(
        runner,
        git_command(
            ["ls-remote", "--symref", ORIGIN_REMOTE],
            Some(path),
            options,
            false,
        ),
        options.mode,
    )?;
    let expected_refs = parse_ref_lines(&advertisement.stdout, false);
    let head = advertised_head(&advertisement.stdout)?;
    let mut fetch = vec![
        "fetch".to_owned(),
        // Background maintenance could outlive directory ownership.
        "--no-auto-gc".to_owned(),
        "--prune".to_owned(),
        ORIGIN_REMOTE.to_owned(),
        "+refs/*:refs/*".to_owned(),
    ];
    if let Some(MirrorHead::Detached(oid)) = &head {
        // A detached source HEAD need not be reachable from any ref. Fetch its
        // exact advertised object too, without inventing a ref to mirror back.
        fetch.push(oid.clone());
    }
    check_cancelled(cancel)?;
    run_required(
        runner,
        git_command_from_vec(fetch, Some(path), options, true),
        options.mode,
    )?;
    check_cancelled(cancel)?;
    if let Some(head) = head {
        let command = match &head {
            MirrorHead::Symbolic(target) => {
                git_command(["symbolic-ref", "HEAD", target], Some(path), options, false)
            }
            MirrorHead::Detached(oid) => git_command(
                ["update-ref", "--no-deref", "HEAD", oid],
                Some(path),
                options,
                false,
            ),
        };
        run_required(runner, command, options.mode)?;
    }
    check_cancelled(cancel)?;
    let fetched = run_required(
        runner,
        git_command(
            ["for-each-ref", "--format=%(objectname) %(refname)"],
            Some(path),
            options,
            false,
        ),
        options.mode,
    )?;
    // Fetch can exit successfully while rejecting shallow refs. It may also
    // see a newer advertisement than the one above. Neither is an exact source
    // snapshot, so no caller may plan deletions from the resulting cache.
    if parse_ref_lines(&fetched.stdout, false) != expected_refs {
        return Err(CrabError::Protocol(
            "mirror source refs changed or were not completely fetched; retry from a complete, stable source".to_owned(),
        ));
    }
    Ok(created)
}

fn needs_initialization(path: &Path, cancel: &CancellationToken) -> Result<bool> {
    if !path.try_exists()? {
        return Ok(true);
    }
    if !path.is_dir() {
        return Err(CrabError::Configuration {
            key: "mirror cache path exists but is not a directory".to_owned(),
            origin: path.display().to_string(),
        });
    }
    if path.join("HEAD").try_exists()? {
        return Ok(false);
    }
    Ok(cleanup_preview(path, cancel)?.files_removed == 0)
}

fn validate_bare_cache(
    path: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let output = runner.run(
        &git_command(
            ["rev-parse", "--is-bare-repository"],
            Some(path),
            options,
            false,
        ),
        options.mode,
    )?;
    if output.status.success && output.stdout.trim() == "true" {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "mirror cache is not a bare Git repository".to_owned(),
        origin: path.display().to_string(),
    })
}

#[derive(Debug, PartialEq, Eq)]
enum MirrorHead {
    Symbolic(String),
    Detached(String),
}

fn advertised_head(output: &str) -> Result<Option<MirrorHead>> {
    let mut symbolic = None;
    let mut detached = None;
    for line in output.lines() {
        let Some((value, "HEAD")) = line.split_once('\t') else {
            continue;
        };
        if let Some(target) = value.strip_prefix("ref: ") {
            symbolic = Some(MirrorHead::Symbolic(target.to_owned()));
        } else if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            detached = Some(MirrorHead::Detached(value.to_owned()));
        } else {
            return Err(CrabError::Protocol(
                "source HEAD is not a SHA-1 Git object".to_owned(),
            ));
        }
    }
    Ok(symbolic.or(detached))
}

#[cfg(test)]
mod tests;
