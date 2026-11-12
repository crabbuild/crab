use super::super::publication::IndexFile;
use super::*;

pub(in crate::repository::content) async fn copy(
    index: &IndexFile,
    private_index: &Path,
    size: u64,
    mut file: std::fs::File,
    scope: &ReadScope,
    mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
) -> Result<()> {
    // Git can read a Windows verbatim index path as an empty index. Scratch is
    // inside this worktree; keep native ownership paths canonical and give Git
    // the owned index relative to its working directory.
    let root = tokio::fs::canonicalize(&index.root).await?;
    let private_index = private_index
        .strip_prefix(root)
        .map_err(std::io::Error::other)?;
    let mut command = git_command(
        &index.root,
        &["ls-files", "--stage", "-z", "--", &index.path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let selected = checked_stdout(execute(command, scope, capture, None).await?)?;
    let entry = file_entry_at(&selected, &index.path, true)?;
    if entry.0.as_deref() != Some(&index.mode) || entry.1.as_deref() != Some(&index.oid) {
        return Err(changed());
    }
    if !requires_git_conversion(index, private_index, scope).await? {
        return copy_blob_to_file(index, size, file, scope, progress).await;
    }
    copy_blob(&index.root, &index.oid, size, std::io::sink(), scope).await?;
    let path = format!(":{}", index.path);
    // The index expression makes Git load index-backed attributes when working
    // attributes are absent. A raw OID with --path skips that on supported Git.
    let mut command = git_command(
        &index.root,
        &["--no-replace-objects", "cat-file", "--filters", &path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let observed = file.try_clone()?;
    let observer = move || progress(observed.metadata()?.len());
    let read = move |mut stdout: std::process::ChildStdout| {
        std::io::copy(&mut stdout, &mut file)?;
        Ok(Vec::new())
    };
    let output = execute(command, scope, read, Some(Box::new(observer))).await?;
    checked_stdout(output)?;
    // Git owns conversion, so authenticate its immutable input again before
    // publishing. The physical-copy branch authenticates its own output bytes.
    copy_blob(&index.root, &index.oid, size, std::io::sink(), scope).await
}

async fn requires_git_conversion(
    index: &IndexFile,
    private_index: &Path,
    scope: &ReadScope,
) -> Result<bool> {
    let mut command = git_command(
        &index.root,
        &["check-attr", "--all", "-z", "--", &index.path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let attributes = checked_stdout(execute(command, scope, capture, None).await?)?;
    // --all distinguishes an absent attribute from a literal value such as
    // filter=unspecified. Named queries print the same text for both states.
    let mut eol_conversion = false;
    let mut lf_only = false;
    let mut unset_text = false;
    if !attributes.is_empty() {
        let fields = attributes
            .strip_suffix(&[0])
            .ok_or_else(|| std::io::Error::other("invalid Git attribute framing"))?
            .split(|byte| *byte == 0)
            .collect::<Vec<_>>();
        let (records, remainder) = fields.as_chunks::<3>();
        if !remainder.is_empty() {
            return Err(std::io::Error::other("invalid Git attribute record").into());
        }
        for record in records {
            if record[0] != index.path.as_bytes() {
                return Err(std::io::Error::other("Git attribute path differs").into());
            }
            match record[1] {
                b"ident" | b"filter" | b"working-tree-encoding" => return Ok(true),
                b"crlf" | b"eol" | b"text" => {
                    eol_conversion = true;
                    lf_only |= record[1] == b"eol" && record[2] == b"lf";
                    unset_text |= record[1] == b"text" && record[2] == b"unset";
                }
                _ => {}
            }
        }
    }
    if lf_only {
        return Ok(false);
    }
    if unset_text {
        // A typed attribute pathspec distinguishes -text from text=unset.
        // Literal/top magic keeps the user's exact path while testing its state.
        let selector = format!(":(top,literal,attr:-text){}", index.path);
        let mut command = git_command(
            &index.root,
            &["--no-literal-pathspecs", "ls-files", "-z", "--", &selector],
        )?;
        command.env("GIT_INDEX_FILE", private_index);
        let selected = checked_stdout(execute(command, scope, capture, None).await?)?;
        if selected == format!("{}\0", index.path).as_bytes() {
            return Ok(false);
        }
    }
    if eol_conversion {
        return Ok(true);
    }
    let configured = output(
        &index.root,
        &["config", "--null", "--get", "core.autocrlf"],
        scope,
    )
    .await?;
    if configured.status.code() == Some(1) {
        return Ok(false);
    }
    if checked_stdout(configured)? == b"input\0" {
        return Ok(false);
    }
    // Let Git interpret its boolean spellings. With no conversion attributes,
    // false/input never alter checkout bytes, independently of core.eol.
    let normalized = run(
        &index.root,
        &["config", "--null", "--bool", "--get", "core.autocrlf"],
        scope,
    )
    .await?;
    match normalized.as_slice() {
        b"false\0" => Ok(false),
        b"true\0" => Ok(true),
        _ => Err(std::io::Error::other("invalid Git autocrlf value").into()),
    }
}

async fn copy_blob_to_file(
    index: &IndexFile,
    size: u64,
    file: std::fs::File,
    scope: &ReadScope,
    mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
) -> Result<()> {
    let observed = file.try_clone()?;
    let copied = copy_blob(&index.root, &index.oid, size, file, scope);
    tokio::pin!(copied);
    loop {
        tokio::select! {
            result = &mut copied => {
                result?;
                drop(report_progress(progress, size).await?);
                return scope.check();
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                let reported = match observed.metadata() {
                    Ok(metadata) => report_progress(progress, metadata.len()).await,
                    Err(error) => Err(error.into()),
                };
                match reported {
                    Ok(reporter) => progress = reporter,
                    Err(error) => {
                        // A failed observer cannot drop a live physical worker.
                        // Cancel and join before the publication workspace cleans up.
                        scope.cancel();
                        return match copied.await {
                            Err(error) if !error.is_cancelled() => Err(error),
                            _ => Err(error),
                        };
                    }
                }
            }
        }
    }
}

async fn report_progress<F>(mut progress: F, written: u64) -> Result<F>
where
    F: FnMut(u64) -> Result<()> + Send + 'static,
{
    // Transfer reporters block on async notification delivery. Keep the same
    // blocking-worker contract as Git conversion and join each callback.
    tokio::task::spawn_blocking(move || {
        progress(written)?;
        Ok(progress)
    })
    .await
    .map_err(std::io::Error::other)?
}
