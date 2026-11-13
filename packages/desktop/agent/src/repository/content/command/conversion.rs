use super::super::publication::IndexFile;
use super::*;
use crab_git::batch::{Checkout, CheckoutEol};

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
    if let Plan::Builtin(checkout) = plan(index, private_index, scope).await? {
        return copy_checkout_to_file(index, size, file, checkout, scope, progress).await;
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

enum Plan {
    Builtin(Checkout),
    Git,
}

enum Attribute {
    Set,
    Unset,
    Value(Vec<u8>),
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Text {
    #[default]
    Unspecified,
    Binary,
    Forced,
    Auto,
    Input,
}

impl From<Attribute> for Text {
    fn from(attribute: Attribute) -> Self {
        match attribute {
            Attribute::Set => Self::Forced,
            Attribute::Unset => Self::Binary,
            Attribute::Value(value) if value == b"auto" => Self::Auto,
            Attribute::Value(value) if value == b"input" => Self::Input,
            _ => Self::Unspecified,
        }
    }
}

async fn plan(index: &IndexFile, private_index: &Path, scope: &ReadScope) -> Result<Plan> {
    let mut command = git_command(
        &index.root,
        &["check-attr", "--all", "-z", "--", &index.path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let attributes = checked_stdout(execute(command, scope, capture, None).await?)?;
    let mut text = Text::Unspecified;
    let mut crlf = Text::Unspecified;
    let mut eol = None;
    let mut ident = false;
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
                // These stages retain Git's existing semantics until their own
                // bounded replacements cover encoding and process-filter contracts.
                b"working-tree-encoding" => return Ok(Plan::Git),
                b"filter" => {
                    if matches!(
                        attribute(index, private_index, "filter", record[2], scope).await?,
                        Attribute::Value(_)
                    ) {
                        return Ok(Plan::Git);
                    }
                }
                b"eol" => eol = Some(record[2].to_vec()),
                b"text" => {
                    text = attribute(index, private_index, "text", record[2], scope)
                        .await?
                        .into()
                }
                b"crlf" => {
                    crlf = attribute(index, private_index, "crlf", record[2], scope)
                        .await?
                        .into()
                }
                b"ident" => {
                    ident = matches!(
                        attribute(index, private_index, "ident", record[2], scope).await?,
                        Attribute::Set
                    )
                }
                _ => {}
            }
        }
    }
    if text == Text::Unspecified {
        text = crlf;
    }
    let explicit = match (text, eol.as_deref()) {
        (Text::Binary, _) | (_, Some(b"lf")) => Some(CheckoutEol::Preserve),
        (Text::Auto, Some(b"crlf")) => Some(CheckoutEol::AutoCrLf),
        (_, Some(b"crlf")) => Some(CheckoutEol::CrLf),
        (Text::Input, _) => Some(CheckoutEol::Preserve),
        _ => None,
    };
    let eol = if let Some(eol) = explicit {
        eol
    } else {
        let autocrlf = config(&index.root, "core.autocrlf", scope).await?;
        let input_only = autocrlf
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(b"input"));
        let enabled = if autocrlf.is_some() && !input_only {
            run(
                &index.root,
                &["config", "--null", "--bool", "--get", "core.autocrlf"],
                scope,
            )
            .await?
                == b"true\0"
        } else {
            false
        };
        let crlf = if enabled {
            true
        } else if input_only {
            false
        } else if matches!(text, Text::Forced | Text::Auto) {
            match config(&index.root, "core.eol", scope).await?.as_deref() {
                Some(value) if value.eq_ignore_ascii_case(b"crlf") => true,
                Some(value) if value.eq_ignore_ascii_case(b"lf") => false,
                _ => cfg!(windows),
            }
        } else {
            false
        };
        match (crlf, text) {
            (false, _) => CheckoutEol::Preserve,
            (true, Text::Forced) => CheckoutEol::CrLf,
            (true, _) => CheckoutEol::AutoCrLf,
        }
    };
    Ok(Plan::Builtin(Checkout { eol, ident }))
}

async fn attribute(
    index: &IndexFile,
    private_index: &Path,
    name: &str,
    value: &[u8],
    scope: &ReadScope,
) -> Result<Attribute> {
    let state = match value {
        b"set" => Some(name.to_owned()),
        b"unset" => Some(format!("-{name}")),
        _ => None,
    };
    if let Some(state) = state {
        // --all preserves absence; a typed literal pathspec distinguishes true
        // and false from strings literally named set/unset without parsing files.
        let selector = format!(":(top,literal,attr:{state}){}", index.path);
        let mut command = git_command(
            &index.root,
            &["--no-literal-pathspecs", "ls-files", "-z", "--", &selector],
        )?;
        command.env("GIT_INDEX_FILE", private_index);
        let selected = checked_stdout(execute(command, scope, capture, None).await?)?;
        if selected == format!("{}\0", index.path).as_bytes() {
            return Ok(if value == b"set" {
                Attribute::Set
            } else {
                Attribute::Unset
            });
        }
    }
    Ok(Attribute::Value(value.to_vec()))
}

async fn config(root: &Path, name: &str, scope: &ReadScope) -> Result<Option<Vec<u8>>> {
    let configured = output(root, &["config", "--null", "--get", name], scope).await?;
    if configured.status.code() == Some(1) {
        return Ok(None);
    }
    let value = checked_stdout(configured)?;
    let value = value
        .strip_suffix(&[0])
        .ok_or_else(|| std::io::Error::other("invalid Git configuration framing"))?;
    Ok(Some(value.to_vec()))
}

async fn copy_checkout_to_file(
    index: &IndexFile,
    size: u64,
    file: std::fs::File,
    checkout: Checkout,
    scope: &ReadScope,
    mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
) -> Result<()> {
    let observed = file.try_clone()?;
    let copied = read_blob(&index.root, &index.oid, size, file, checkout, scope);
    tokio::pin!(copied);
    loop {
        tokio::select! {
            result = &mut copied => {
                result?;
                let written = observed.metadata()?.len();
                drop(report_progress(progress, written).await?);
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
