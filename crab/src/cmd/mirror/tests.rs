use super::*;

pub(super) fn base_args(cache_dir: PathBuf) -> MirrorArgs {
    MirrorArgs {
        source: "https://example.com/org/repo.git".to_owned(),
        destination: "crab://bucket/org/repo".to_owned(),
        cache_dir: Some(cache_dir),
        no_atomic: false,
        skip_lfs: false,
        force_lfs_check: false,
        check: false,
        write_plan: None,
        apply_plan: None,
        allow_delete_refs: false,
        ci: false,
        json: false,
        jsonl: false,
    }
}

pub(super) fn test_options() -> MirrorExecution {
    test_options_with_collector(fake_lfs_object_ids)
}

fn test_options_with_collector(collector: LfsObjectIdCollector) -> MirrorExecution {
    MirrorExecution {
        mode: OutputMode::Json,
        require_remote_helper: false,
        helper_path: None,
        crab_binary: "crab".to_owned(),
        lfs_object_id_collector: collector,
        initialize_destination: |_, _, _| Ok(()),
    }
}

fn fake_lfs_object_ids(
    _repo_dir: &Path,
    _local_shas: &[String],
    _remote_shas: &[String],
    _cancel: &CancellationToken,
) -> Result<Vec<String>> {
    Ok(vec![lfs_oid(0xab)])
}

fn no_lfs_object_ids(
    _repo_dir: &Path,
    _local_shas: &[String],
    _remote_shas: &[String],
    _cancel: &CancellationToken,
) -> Result<Vec<String>> {
    Ok(Vec::new())
}

fn oid(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}

fn lfs_oid(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

#[test]
fn integrity_flags_parse_as_a_plan_first_invocation() {
    let args = MirrorArgs::try_parse_from([
        "mirror",
        "source",
        "crab://bucket/repo",
        "--check",
        "--write-plan",
        "plan.json",
        "--allow-delete-refs",
        "--ci",
    ])
    .unwrap();

    assert!(args.is_integrity_operation());
    assert_eq!(args.write_plan, Some(PathBuf::from("plan.json")));
}

#[derive(Default)]
struct RecordingRunner {
    commands: Vec<ProcessCommand>,
    crab_remote_exists: bool,
    lfs_pointers_present: bool,
    local_refs: Vec<(String, String)>,
    remote_refs: Vec<(String, String)>,
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, command: &ProcessCommand, _mode: OutputMode) -> Result<ProcessOutput> {
        self.commands.push(command.clone());

        if command.program == "git"
            && command.args == ["remote", "get-url", CRAB_REMOTE]
            && !self.crab_remote_exists
        {
            return Ok(output(false, "", "No such remote"));
        }

        if command.program == "git" && command.args == ["rev-parse", "--is-bare-repository"] {
            return Ok(output(true, "true\n", ""));
        }

        if command.program == "git" && command.args == ["show-ref"] {
            return Ok(output(true, &format_refs(&self.local_refs), ""));
        }

        if command.program == "git" && command.args == ["ls-remote", "--refs", CRAB_REMOTE] {
            return Ok(output(true, &format_refs(&self.remote_refs), ""));
        }

        if command.program == "git" && command.args == ["lfs", "ls-files", "--all"] {
            if self.lfs_pointers_present {
                return Ok(output(true, "0123456789abcdef * asset.bin\n", ""));
            }
            return Ok(output(true, "", ""));
        }

        Ok(output(true, "", ""))
    }
}

fn changed_ref_runner() -> RecordingRunner {
    RecordingRunner {
        local_refs: vec![("refs/heads/main".to_owned(), oid(0xaa))],
        remote_refs: vec![("refs/heads/main".to_owned(), oid(0xbb))],
        ..RecordingRunner::default()
    }
}

fn no_op_runner() -> RecordingRunner {
    RecordingRunner {
        local_refs: vec![("refs/heads/main".to_owned(), oid(0xaa))],
        remote_refs: vec![("refs/heads/main".to_owned(), oid(0xaa))],
        ..RecordingRunner::default()
    }
}

fn format_refs(refs: &[(String, String)]) -> String {
    refs.iter()
        .map(|(name, sha)| format!("{sha}\t{name}\n"))
        .collect()
}

fn output(success: bool, stdout: &str, stderr: &str) -> ProcessOutput {
    ProcessOutput {
        status: ProcessStatus {
            success,
            code: Some(if success { 0 } else { 1 }),
        },
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

fn assert_command(command: &ProcessCommand, program: &str, args: &[&str]) {
    assert_eq!(command.program, program);
    assert_eq!(command.args, args);
}

fn assert_mirror_git_only_env(command: &ProcessCommand) {
    assert!(command.envs.iter().any(|(key, value)| {
        key == crate::git::push_native::MIRROR_GIT_ONLY_ENV && value == &OsString::from("1")
    }));
}

fn assert_stdin(command: &ProcessCommand, lines: &[String]) {
    assert_eq!(command.stdin.as_deref(), Some(stdin_lines(lines).as_str()));
}

fn action_commands(runner: &RecordingRunner) -> Vec<&ProcessCommand> {
    runner
        .commands
        .iter()
        .filter(|command| {
            !(command.program == "git" && command.args == ["--version"])
                && !(command.program == "git" && command.args == ["lfs", "version"])
        })
        .collect()
}

fn destination_commands(runner: &RecordingRunner) -> Vec<&ProcessCommand> {
    runner
        .commands
        .iter()
        .skip_while(|command| command.args != ["remote", "get-url", CRAB_REMOTE])
        .collect()
}

#[test]
fn cache_key_changes_with_source_and_destination() {
    let first = mirror_cache_key("https://example.com/a.git", "crab://bucket/a");
    let same = mirror_cache_key("https://example.com/a.git", "crab://bucket/a");
    let different_source = mirror_cache_key("https://example.com/b.git", "crab://bucket/a");
    let different_destination = mirror_cache_key("https://example.com/a.git", "crab://bucket/b");

    assert_eq!(first, same);
    assert_ne!(first, different_source);
    assert_ne!(first, different_destination);
}

#[test]
fn relative_local_source_resolves_from_invocation_directory()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source.git");
    std::fs::create_dir(&source)?;

    let resolved = resolve_source("source.git", temp.path())?;

    assert_eq!(resolved, source.canonicalize()?.display().to_string());
    Ok(())
}

#[test]
fn remote_source_url_is_not_rewritten() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;

    let resolved = resolve_source("https://example.com/org/repo.git", temp.path())?;

    assert_eq!(resolved, "https://example.com/org/repo.git");
    Ok(())
}

#[test]
fn relative_cache_resolves_before_git_changes_directory() {
    let dir = tempfile::tempdir().unwrap();
    let args = base_args(PathBuf::from("nested/cache.git"));
    assert_eq!(
        resolve_cache_dir(&args, dir.path()),
        dir.path().join("nested/cache.git")
    );
}

#[test]
fn legacy_mirror_refuses_an_owned_cache_before_any_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.git");
    let _owner = CacheUseGuard::acquire(&path, &CancellationToken::new()).unwrap();
    let mut runner = changed_ref_runner();
    let result = run_mirror_with_runner(
        &base_args(path),
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    );
    assert!(
        matches!(result, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(action_commands(&runner).is_empty());
}

#[test]
fn ref_parser_preserves_source_tracking_refs_and_ignores_pseudo_refs() {
    let parsed = parse_ref_lines(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n\
         bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/remotes/crab/main\n\
         cccccccccccccccccccccccccccccccccccccccc HEAD\n\
         dddddddddddddddddddddddddddddddddddddddd refs/tags/v1^{}\n",
    );

    assert_eq!(parsed.len(), 2);
    assert_eq!(
        parsed.get("refs/remotes/crab/main"),
        Some(&"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned())
    );
    assert_eq!(
        parsed.get("refs/heads/main"),
        Some(&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned())
    );
}

#[test]
fn invalid_destination_fails_before_subprocess()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let mut args = base_args(temp.path().join("cache.git"));
    args.destination = "https://example.com/not-crab.git".to_owned();
    let mut runner = RecordingRunner::default();

    let result = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    );

    assert!(matches!(result, Err(CrabError::Configuration { .. })));
    assert!(runner.commands.is_empty());
    Ok(())
}

#[test]
fn missing_cache_initializes_then_mirrors_lfs_and_git_refs()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let cache = temp.path().canonicalize()?.join("cache.git");
    let args = base_args(cache.clone());
    let mut runner = RecordingRunner {
        local_refs: vec![("refs/heads/main".to_owned(), oid(0xaa))],
        ..RecordingRunner::default()
    };

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(summary.created_cache);
    let init = runner
        .commands
        .iter()
        .find(|command| command.args.first().is_some_and(|arg| arg == "init"))
        .unwrap();
    assert_command(
        init,
        "git",
        &["init", "--bare", "--object-format=sha1", "--", "."],
    );
    assert_eq!(init.current_dir.as_deref(), Some(cache.as_path()));
    assert!(runner.commands.iter().any(|command| command.args
        == [
            "config",
            "--replace-all",
            "remote.origin.url",
            args.source.as_str()
        ]));
    let commands = destination_commands(&runner);
    assert_eq!(commands.len(), 8);
    assert_command(commands[0], "git", &["remote", "get-url", CRAB_REMOTE]);
    assert_command(
        commands[1],
        "git",
        &["remote", "add", CRAB_REMOTE, "crab://bucket/org/repo"],
    );
    assert_command(
        commands[2],
        "git",
        &["config", "--unset-all", "remote.crab.fetch"],
    );
    assert_command(commands[3], "git", &["show-ref"]);
    assert_command(commands[4], "git", &["ls-remote", "--refs", CRAB_REMOTE]);
    assert_command(
        commands[5],
        "git",
        &["lfs", "fetch", "--all", ORIGIN_REMOTE, &oid(0xaa)],
    );
    assert_command(
        commands[6],
        "crab",
        &["lfs", "push", CRAB_REMOTE, "--object-id", "--stdin"],
    );
    assert_stdin(commands[6], &[lfs_oid(0xab)]);
    assert_command(
        commands[7],
        "git",
        &["push", "--mirror", "--atomic", CRAB_REMOTE],
    );
    assert_mirror_git_only_env(commands[7]);
    Ok(())
}

#[test]
fn existing_cache_updates_origin_and_existing_crab_remote()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("cache.git");
    std::fs::create_dir_all(&cache)?;
    std::fs::write(cache.join("HEAD"), "ref: refs/heads/main\n")?;
    let args = base_args(cache);
    let mut runner = RecordingRunner {
        crab_remote_exists: true,
        local_refs: vec![("refs/heads/main".to_owned(), oid(0xaa))],
        remote_refs: vec![("refs/heads/main".to_owned(), oid(0xbb))],
        ..RecordingRunner::default()
    };

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(!summary.created_cache);
    let commands = destination_commands(&runner);
    assert_eq!(commands.len(), 8);
    assert_command(commands[0], "git", &["remote", "get-url", CRAB_REMOTE]);
    assert_command(
        commands[1],
        "git",
        &["remote", "set-url", CRAB_REMOTE, "crab://bucket/org/repo"],
    );
    assert_command(
        commands[2],
        "git",
        &["config", "--unset-all", "remote.crab.fetch"],
    );
    assert_command(commands[3], "git", &["show-ref"]);
    assert_command(commands[4], "git", &["ls-remote", "--refs", CRAB_REMOTE]);
    assert_command(
        commands[5],
        "git",
        &["lfs", "fetch", "--all", ORIGIN_REMOTE, &oid(0xaa)],
    );
    assert_command(
        commands[6],
        "crab",
        &["lfs", "push", CRAB_REMOTE, "--object-id", "--stdin"],
    );
    assert_command(
        commands[7],
        "git",
        &["push", "--mirror", "--atomic", CRAB_REMOTE],
    );
    Ok(())
}

#[test]
fn no_lfs_pointers_skips_lfs_fetch_and_push() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("cache.git");
    let args = base_args(cache);
    let mut runner = changed_ref_runner();

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options_with_collector(no_lfs_object_ids),
        &mut runner,
    )?;

    assert!(summary.lfs_enabled);
    let commands = action_commands(&runner);
    assert!(!commands.iter().any(|command| {
        command.program == "git" && command.args.first().map(String::as_str) == Some("lfs")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "crab" && command.args.first().map(String::as_str) == Some("lfs")
    }));
    assert_command(
        commands[commands.len() - 1],
        "git",
        &["push", "--mirror", "--atomic", CRAB_REMOTE],
    );
    assert_mirror_git_only_env(commands[commands.len() - 1]);
    Ok(())
}

#[test]
fn matching_refs_skip_lfs_scan_and_git_push_by_default()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("cache.git");
    std::fs::create_dir_all(&cache)?;
    std::fs::write(cache.join("HEAD"), "ref: refs/heads/main\n")?;
    let args = base_args(cache);
    let mut runner = RecordingRunner {
        crab_remote_exists: true,
        ..no_op_runner()
    };

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(!summary.created_cache);
    let commands = destination_commands(&runner);
    assert_command(
        commands[2],
        "git",
        &["config", "--unset-all", "remote.crab.fetch"],
    );
    assert_command(commands[3], "git", &["show-ref"]);
    assert_command(commands[4], "git", &["ls-remote", "--refs", CRAB_REMOTE]);
    assert!(!commands.iter().any(|command| {
        command.program == "git" && command.args.first().map(String::as_str) == Some("lfs")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "crab" && command.args.first().map(String::as_str) == Some("lfs")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "git" && command.args.first().map(String::as_str) == Some("push")
    }));
    Ok(())
}

#[test]
fn force_lfs_check_runs_full_lfs_verification_without_git_push()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("cache.git");
    std::fs::create_dir_all(&cache)?;
    std::fs::write(cache.join("HEAD"), "ref: refs/heads/main\n")?;
    let mut args = base_args(cache);
    args.force_lfs_check = true;
    let mut runner = RecordingRunner {
        crab_remote_exists: true,
        lfs_pointers_present: true,
        ..no_op_runner()
    };

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(summary.lfs_enabled);
    let commands = destination_commands(&runner);
    assert_command(commands[5], "git", &["lfs", "ls-files", "--all"]);
    assert_command(
        commands[6],
        "git",
        &["lfs", "fetch", "--all", ORIGIN_REMOTE],
    );
    assert_command(commands[7], "crab", &["lfs", "push", "--all", CRAB_REMOTE]);
    assert!(!commands.iter().any(|command| {
        command.program == "git" && command.args.first().map(String::as_str) == Some("push")
    }));
    Ok(())
}

#[test]
fn failed_destination_initialization_stops_before_discovery_or_publication() {
    let temp = tempfile::tempdir().unwrap();
    let args = base_args(temp.path().join("cache.git"));
    let mut options = test_options();
    options.initialize_destination = |_, _, _| Err(CrabError::Cancelled);
    let mut runner = changed_ref_runner();
    let result = run_mirror_with_runner(&args, &CancellationToken::new(), options, &mut runner);
    assert!(matches!(result, Err(CrabError::Cancelled)));
    assert!(!runner.commands.iter().any(|command| command.args
        == ["ls-remote", "--refs", CRAB_REMOTE]
        || command.args.first().is_some_and(|arg| arg == "push")));
}

#[test]
fn no_atomic_omits_atomic_push_flag() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let mut args = base_args(temp.path().join("cache.git"));
    args.no_atomic = true;
    let mut runner = changed_ref_runner();

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(!summary.atomic);
    let last = &runner.commands[runner.commands.len() - 1];
    assert_command(last, "git", &["push", "--mirror", CRAB_REMOTE]);
    assert_mirror_git_only_env(last);
    Ok(())
}

#[test]
fn skip_lfs_omits_lfs_subprocesses() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let mut args = base_args(temp.path().join("cache.git"));
    args.skip_lfs = true;
    let mut runner = changed_ref_runner();

    let summary = run_mirror_with_runner(
        &args,
        &CancellationToken::new(),
        test_options(),
        &mut runner,
    )?;

    assert!(!summary.lfs_enabled);
    let commands = action_commands(&runner);
    assert!(
        commands
            .iter()
            .all(|command| command.args.first().map(String::as_str) != Some("lfs"))
    );
    assert_command(
        commands[commands.len() - 1],
        "git",
        &["push", "--mirror", "--atomic", CRAB_REMOTE],
    );
    Ok(())
}
