//! Versioned executable contract shared by Crab and local SDK clients.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Current JSON schema emitted by `crab sdk-capabilities --json`.
pub const CAPABILITIES_SCHEMA_VERSION: u32 = 1;

/// Minimum compatible local-workflow protocol implemented by this crate.
pub const LOCAL_WORKFLOW_PROTOCOL_VERSION: u32 = 1;
pub const STORAGE_FORMAT_VERSION: u32 = 1;
pub const STAGING_FORMAT_VERSION: u32 = 1;

/// Process-local bridge to the canonical durable publication-plan owner.
pub const PUBLICATION_PLAN_ID_ENV: &str = "CRAB_INTERNAL_MIRROR_PLAN_ID";

/// Canonical capability document emitted without fallible runtime serialization.
pub const CURRENT_CAPABILITIES_JSON: &str = concat!(
    "{\"schema_version\":1,\"crab_version\":\"",
    env!("CARGO_PKG_VERSION"),
    "\",\"product_build\":\"crab-remote/",
    env!("CARGO_PKG_VERSION"),
    "\",\"local_workflow_protocol\":1,\"storage_format_version\":1,\"staging_format_version\":1,\"operations\":[\"clone\",\"fetch-content\",\"stage\",\"hydrate\",\"dehydrate\",\"filter-process\",\"remote-helper\"]}"
);

/// Capabilities advertised by a Crab executable to a local SDK client.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutableCapabilities {
    /// Version of this serialized object.
    pub schema_version: u32,
    /// Version string of the executable.
    pub crab_version: String,
    /// Product build identity used to diagnose an exact executable artifact.
    pub product_build: String,
    /// Local-workflow protocol understood by the executable.
    pub local_workflow_protocol: u32,
    /// Remote repository layout version understood by the executable.
    pub storage_format_version: u32,
    /// Local staged-content format version understood by the executable.
    pub staging_format_version: u32,
    /// Stable operation names accepted by the executable.
    pub operations: Vec<String>,
}

impl ExecutableCapabilities {
    /// Return the capabilities of the current Crab executable.
    #[must_use]
    pub fn current() -> Self {
        Self::current_with_build(format!("crab-remote/{}", env!("CARGO_PKG_VERSION")))
    }

    /// Return current capabilities with the executable's exact build identity.
    #[must_use]
    pub fn current_with_build(product_build: String) -> Self {
        Self::for_executable(env!("CARGO_PKG_VERSION").to_owned(), product_build)
    }

    /// Return current capabilities for an executable release and build.
    #[must_use]
    pub fn for_executable(crab_version: String, product_build: String) -> Self {
        Self {
            schema_version: CAPABILITIES_SCHEMA_VERSION,
            crab_version,
            product_build,
            local_workflow_protocol: LOCAL_WORKFLOW_PROTOCOL_VERSION,
            storage_format_version: STORAGE_FORMAT_VERSION,
            staging_format_version: STAGING_FORMAT_VERSION,
            operations: [
                "clone",
                "fetch-content",
                "stage",
                "hydrate",
                "dehydrate",
                "filter-process",
                "remote-helper",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }

    /// Validate an executable before any local repository state is changed.
    pub fn validate_for_sdk(&self) -> Result<(), CapabilityError> {
        if self.schema_version != CAPABILITIES_SCHEMA_VERSION {
            return Err(CapabilityError::Schema {
                expected: CAPABILITIES_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        if self.local_workflow_protocol != LOCAL_WORKFLOW_PROTOCOL_VERSION {
            return Err(CapabilityError::Protocol {
                expected: LOCAL_WORKFLOW_PROTOCOL_VERSION,
                actual: self.local_workflow_protocol,
            });
        }
        if self.storage_format_version != STORAGE_FORMAT_VERSION
            || self.staging_format_version != STAGING_FORMAT_VERSION
            || self.product_build.is_empty()
        {
            return Err(CapabilityError::Format);
        }
        for required in ["clone", "filter-process", "remote-helper"] {
            if !self
                .operations
                .iter()
                .any(|operation| operation == required)
            {
                return Err(CapabilityError::MissingOperation(required));
            }
        }
        Ok(())
    }
}

/// Incompatibility between a local SDK and Crab executable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapabilityError {
    #[error("unsupported Crab capability schema {actual}; expected {expected}")]
    Schema { expected: u32, actual: u32 },
    #[error("unsupported Crab local-workflow protocol {actual}; expected {expected}")]
    Protocol { expected: u32, actual: u32 },
    #[error("Crab executable does not advertise required operation {0}")]
    MissingOperation(&'static str),
    #[error("Crab executable uses incompatible storage or staging formats")]
    Format,
}

/// Maximum bytes retained from either subprocess output stream.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Explicit Git and Crab executables with process-scoped command discovery.
#[derive(Clone)]
pub struct LocalTools {
    inner: Arc<ToolEnvironment>,
}

struct ToolEnvironment {
    git: PathBuf,
    crab: PathBuf,
    directory: tempfile::TempDir,
    hooks: tempfile::TempDir,
}

impl LocalTools {
    /// Validate absolute executable paths and construct private command aliases.
    pub fn new(git: &Path, crab: &Path) -> Result<Self, LocalError> {
        let git = executable(git, "Git")?;
        let crab = executable(crab, "Crab")?;
        let directory = tempfile::tempdir().map_err(LocalError::Scratch)?;
        let hooks = tempfile::tempdir().map_err(LocalError::Scratch)?;
        install_alias(&git, directory.path(), executable_name("git"))?;
        install_alias(&crab, directory.path(), executable_name("crab"))?;
        install_alias(&crab, directory.path(), executable_name("git-remote-crab"))?;
        Ok(Self {
            inner: Arc::new(ToolEnvironment {
                git,
                crab,
                directory,
                hooks,
            }),
        })
    }

    /// Return the selected Git executable.
    #[must_use]
    pub fn git(&self) -> &Path {
        &self.inner.git
    }

    /// Return the selected Crab executable.
    #[must_use]
    pub fn crab(&self) -> &Path {
        &self.inner.crab
    }

    /// Validate both executables and their versioned interoperability contract.
    pub async fn handshake(&self, cancel: &CancellationToken) -> Result<ToolHandshake, LocalError> {
        let git = self.run_git(None, ["--version"], false, cancel).await?;
        let git_version = parse_git_version(&git.stdout)?;
        if git_version < (2, 30, 9) {
            return Err(LocalError::GitVersion {
                actual: format!("{}.{}.{}", git_version.0, git_version.1, git_version.2),
            });
        }
        let crab = self
            .run_crab(None, ["sdk-capabilities", "--json"], false, cancel)
            .await?;
        let capabilities: ExecutableCapabilities =
            serde_json::from_slice(&crab.stdout).map_err(LocalError::CapabilityJson)?;
        capabilities
            .validate_for_sdk()
            .map_err(LocalError::Capability)?;
        Ok(ToolHandshake {
            git_version: format!("{}.{}.{}", git_version.0, git_version.1, git_version.2),
            capabilities,
        })
    }

    /// Install the selected Crab executable as this repository's filter owner.
    pub async fn configure_filters(
        &self,
        repository: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), LocalError> {
        let values = filter_driver_values(&self.inner.crab)?;
        for (key, value) in values {
            self.run_git(
                Some(repository),
                ["config", "--local", key, value.as_str()],
                false,
                cancel,
            )
            .await?;
        }
        Ok(())
    }

    /// Return whether this repository uses the exact selected Crab filter owner.
    pub async fn filters_match(
        &self,
        repository: &Path,
        cancel: &CancellationToken,
    ) -> Result<bool, LocalError> {
        let expected = filter_driver_values(&self.inner.crab)?;
        for (key, value) in expected {
            let actual = match self
                .run_git(
                    Some(repository),
                    ["config", "--local", "--get", key],
                    false,
                    cancel,
                )
                .await
            {
                Ok(output) => output,
                Err(LocalError::Exit(output)) if output.status.code() == Some(1) => {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            let actual = actual.stdout.strip_suffix(b"\n").unwrap_or(&actual.stdout);
            let actual = actual.strip_suffix(b"\r").unwrap_or(actual);
            if actual != value.as_bytes() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Run Git with exact tool discovery and bounded output.
    pub async fn run_git<I, S>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(
            &self.inner.git,
            current_dir,
            args,
            std::iter::empty::<(&OsStr, &OsStr)>(),
            ToolIo::default(),
            trust_hooks,
            cancel,
        )
        .await
    }

    /// Run Git with additional process-local environment values.
    pub async fn run_git_with_env<I, S, E, K, V>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        env: E,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run(
            &self.inner.git,
            current_dir,
            args,
            env,
            ToolIo::default(),
            trust_hooks,
            cancel,
        )
        .await
    }

    /// Run Git with bounded standard input and output.
    pub async fn run_git_with_input<I, S>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        input: Vec<u8>,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(
            &self.inner.git,
            current_dir,
            args,
            std::iter::empty::<(&OsStr, &OsStr)>(),
            ToolIo {
                input: Some(input),
                output: None,
            },
            trust_hooks,
            cancel,
        )
        .await
    }

    /// Run Git with bounded input while streaming standard output to a new file.
    pub async fn run_git_to_file_with_input<I, S>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        input: Vec<u8>,
        output: &Path,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(output)
            .map_err(LocalError::Output)?;
        self.run(
            &self.inner.git,
            current_dir,
            args,
            std::iter::empty::<(&OsStr, &OsStr)>(),
            ToolIo {
                input: Some(input),
                output: Some(output),
            },
            trust_hooks,
            cancel,
        )
        .await
    }

    /// Run Crab with exact child Git/helper discovery and bounded output.
    pub async fn run_crab<I, S>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(
            &self.inner.crab,
            current_dir,
            args,
            std::iter::empty::<(&OsStr, &OsStr)>(),
            ToolIo::default(),
            trust_hooks,
            cancel,
        )
        .await
    }

    /// Run Crab with additional process-local environment values.
    pub async fn run_crab_with_env<I, S, E, K, V>(
        &self,
        current_dir: Option<&Path>,
        args: I,
        env: E,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run(
            &self.inner.crab,
            current_dir,
            args,
            env,
            ToolIo::default(),
            trust_hooks,
            cancel,
        )
        .await
    }

    async fn run<I, S, E, K, V>(
        &self,
        executable: &Path,
        current_dir: Option<&Path>,
        args: I,
        env: E,
        io: ToolIo,
        trust_hooks: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, LocalError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        if cancel.is_cancelled() {
            return Err(LocalError::Cancelled);
        }
        if io
            .input
            .as_ref()
            .is_some_and(|input| input.len() > MAX_TOOL_OUTPUT_BYTES)
        {
            return Err(LocalError::OutputLimit);
        }
        let untrusted_config = if trust_hooks {
            Vec::new()
        } else {
            self.untrusted_git_config(current_dir, cancel).await?
        };
        let capture_stdout = io.output.is_none();
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(if io.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(io.output.map_or_else(Stdio::piped, Stdio::from))
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        command.envs(env);
        let path = process_path(self.inner.directory.path());
        command.env("PATH", path);
        if !trust_hooks {
            command.env("GIT_CONFIG_COUNT", (untrusted_config.len() + 1).to_string());
            command
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", self.inner.hooks.path());
            for (offset, (key, value)) in untrusted_config.into_iter().enumerate() {
                let index = offset + 1;
                command
                    .env(format!("GIT_CONFIG_KEY_{index}"), key)
                    .env(format!("GIT_CONFIG_VALUE_{index}"), value);
            }
        }
        let mut child = command.spawn().map_err(|source| LocalError::Spawn {
            executable: executable.to_owned(),
            source,
        })?;
        let stdout = if capture_stdout {
            let stdout = child.stdout.take().ok_or(LocalError::MissingPipe)?;
            Some(tokio::spawn(read_bounded(stdout)))
        } else {
            None
        };
        let stderr = child.stderr.take().ok_or(LocalError::MissingPipe)?;
        let stderr = tokio::spawn(read_bounded(stderr));
        let stdin = match io.input {
            Some(input) => {
                let mut stdin = child.stdin.take().ok_or(LocalError::MissingPipe)?;
                Some(tokio::spawn(async move {
                    stdin.write_all(&input).await.map_err(LocalError::Write)?;
                    stdin.shutdown().await.map_err(LocalError::Write)
                }))
            }
            None => None,
        };
        let status = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                child.kill().await.map_err(LocalError::Kill)?;
                let _ = child.wait().await;
                None
            }
            result = child.wait() => Some(result.map_err(LocalError::Wait)?),
        };
        if status.is_none() {
            if let Some(stdin) = stdin {
                let _ = stdin.await;
            }
            if let Some(stdout) = stdout {
                let _ = stdout.await;
            }
            let _ = stderr.await;
            return Err(LocalError::Cancelled);
        }
        let status = status.ok_or(LocalError::Cancelled)?;
        if let Some(stdin) = stdin {
            stdin.await.map_err(LocalError::WriterTask)??;
        }
        let stdout = match stdout {
            Some(stdout) => stdout.await.map_err(LocalError::ReaderTask)??,
            None => BoundedBytes {
                bytes: Vec::new(),
                overflow: false,
            },
        };
        let stderr = stderr.await.map_err(LocalError::ReaderTask)??;
        if stdout.overflow || stderr.overflow {
            return Err(LocalError::OutputLimit);
        }
        let output = ToolOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        };
        if !output.status.success() {
            return Err(LocalError::Exit(output));
        }
        Ok(output)
    }

    async fn untrusted_git_config(
        &self,
        current_dir: Option<&Path>,
        cancel: &CancellationToken,
    ) -> Result<Vec<(OsString, OsString)>, LocalError> {
        const EXECUTABLE_CONFIG: &str = "^(core\\.fsmonitor|diff\\..*\\.(command|textconv)|filter\\..*\\.(clean|smudge|process)|merge\\..*\\.driver)$";
        let Some(current_dir) = current_dir else {
            return Ok(Vec::new());
        };
        let output = match Box::pin(self.run(
            &self.inner.git,
            Some(current_dir),
            ["config", "--null", "--get-regexp", EXECUTABLE_CONFIG],
            std::iter::empty::<(&OsStr, &OsStr)>(),
            ToolIo::default(),
            true,
            cancel,
        ))
        .await
        {
            Ok(output) => output,
            Err(LocalError::Exit(output)) if output.status.code() == Some(1) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let expected = filter_driver_values(&self.inner.crab).unwrap_or_default();
        let mut overrides = Vec::new();
        for entry in output.stdout.split(|byte| *byte == 0) {
            if entry.is_empty() {
                continue;
            }
            let Some(separator) = entry.iter().position(|byte| *byte == b'\n') else {
                return Err(LocalError::GitConfigSyntax);
            };
            let name = std::str::from_utf8(&entry[..separator])
                .map_err(|_| LocalError::GitConfigSyntax)?;
            let value = &entry[separator + 1..];
            if expected.iter().any(|(key, expected)| {
                name.eq_ignore_ascii_case(key) && value == expected.as_bytes()
            }) {
                continue;
            }
            let disabled = if name.eq_ignore_ascii_case("core.fsmonitor") {
                "false"
            } else {
                ""
            };
            overrides.push((name.into(), disabled.into()));
        }
        Ok(overrides)
    }
}

/// Git-local driver values for an exact Crab executable path.
pub fn filter_driver_values(crab: &Path) -> Result<Vec<(&'static str, String)>, LocalError> {
    let quoted = quote_git_command_path(crab)?;
    Ok(vec![
        ("filter.crab.process", format!("{quoted} filter-process")),
        ("filter.crab.clean", format!("{quoted} filter-process")),
        ("filter.crab.smudge", format!("{quoted} filter-process")),
        ("filter.crab.required", "true".to_owned()),
        ("diff.crab.command", format!("{quoted} diff-driver")),
    ])
}

fn quote_git_command_path(path: &Path) -> Result<String, LocalError> {
    let path = path.to_str().ok_or_else(|| {
        LocalError::Executable(
            "Crab",
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Git filter command paths must be Unicode",
            ),
        )
    })?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

/// Validated tool versions returned before local state mutation.
#[derive(Clone, Debug)]
pub struct ToolHandshake {
    pub git_version: String,
    pub capabilities: ExecutableCapabilities,
}

/// Bounded successful or failed subprocess output.
#[derive(Debug)]
pub struct ToolOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Default)]
struct ToolIo {
    input: Option<Vec<u8>>,
    output: Option<std::fs::File>,
}

impl std::fmt::Display for ToolOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            write!(formatter, "{}", self.status)
        } else {
            write!(formatter, "{}: {stderr}", self.status)
        }
    }
}

/// Failure while validating or owning an SDK local subprocess.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("{0} executable path must be absolute")]
    RelativeExecutable(&'static str),
    #[error("{0} executable cannot be read")]
    Executable(&'static str, #[source] std::io::Error),
    #[error("cannot create local tool scratch directory")]
    Scratch(#[source] std::io::Error),
    #[error("cannot install process-scoped tool alias")]
    Alias(#[source] std::io::Error),
    #[error("cannot start local tool {executable}")]
    Spawn {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local tool output pipes are unavailable")]
    MissingPipe,
    #[error("cannot read local tool output")]
    Read(#[source] std::io::Error),
    #[error("cannot write local tool input")]
    Write(#[source] std::io::Error),
    #[error("cannot create local tool output")]
    Output(#[source] std::io::Error),
    #[error("local tool output reader stopped")]
    ReaderTask(#[source] tokio::task::JoinError),
    #[error("local tool input writer stopped")]
    WriterTask(#[source] tokio::task::JoinError),
    #[error("cannot wait for local tool")]
    Wait(#[source] std::io::Error),
    #[error("cannot terminate cancelled local tool")]
    Kill(#[source] std::io::Error),
    #[error("local tool operation cancelled")]
    Cancelled,
    #[error("local tool output exceeded its safety limit")]
    OutputLimit,
    #[error("local tool exited unsuccessfully ({0})")]
    Exit(ToolOutput),
    #[error("Git returned an invalid version")]
    GitVersionSyntax,
    #[error("Git returned invalid executable-driver configuration")]
    GitConfigSyntax,
    #[error("Git {actual} is unsupported; version 2.30.9 or later is required")]
    GitVersion { actual: String },
    #[error("Crab returned an invalid capability document")]
    CapabilityJson(#[source] serde_json::Error),
    #[error("Crab executable is incompatible with this SDK")]
    Capability(#[source] CapabilityError),
    #[error("cannot open local SDK repository lease")]
    LeaseOpen(#[source] std::io::Error),
    #[error("cannot acquire local SDK repository lease")]
    Lease(#[source] std::io::Error),
}

/// Cross-process lease keyed by Git's canonical common directory.
pub struct LocalRepositoryLease {
    file: std::fs::File,
}

impl Drop for LocalRepositoryLease {
    fn drop(&mut self) {
        if let Err(error) = fs4::fs_std::FileExt::unlock(&self.file) {
            tracing_fallback(&error);
        }
    }
}

/// Acquire a cancellable cross-process lease for one repository mutation.
pub async fn acquire_repository_lease(
    common_directory: &Path,
    cancel: &CancellationToken,
) -> Result<LocalRepositoryLease, LocalError> {
    let path = common_directory.join("crab-sdk.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(LocalError::LeaseOpen)?;
    loop {
        if cancel.is_cancelled() {
            return Err(LocalError::Cancelled);
        }
        if fs4::fs_std::FileExt::try_lock_exclusive(&file).map_err(LocalError::Lease)? {
            return Ok(LocalRepositoryLease { file });
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn tracing_fallback(error: &std::io::Error) {
    // Drop cannot return cleanup failure. Git's own lock files still protect refs
    // and indexes; the OS also releases this descriptor at process termination.
    tracing::warn!(%error, "Crab SDK repository lease release failed");
}

struct BoundedBytes {
    bytes: Vec<u8>,
    overflow: bool,
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<BoundedBytes, LocalError> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await.map_err(LocalError::Read)?;
        if read == 0 {
            return Ok(BoundedBytes { bytes, overflow });
        }
        let remaining = MAX_TOOL_OUTPUT_BYTES.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        overflow |= retained != read;
    }
}

fn executable(path: &Path, label: &'static str) -> Result<PathBuf, LocalError> {
    if !path.is_absolute() {
        return Err(LocalError::RelativeExecutable(label));
    }
    let metadata =
        std::fs::metadata(path).map_err(|source| LocalError::Executable(label, source))?;
    if !metadata.is_file() {
        return Err(LocalError::Executable(
            label,
            std::io::Error::other("path is not a regular file"),
        ));
    }
    Ok(path.to_owned())
}

fn executable_name(stem: &str) -> OsString {
    if cfg!(windows) {
        format!("{stem}.exe").into()
    } else {
        stem.into()
    }
}

fn install_alias(source: &Path, directory: &Path, name: OsString) -> Result<(), LocalError> {
    let destination = directory.join(name);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, destination).map_err(LocalError::Alias)
    }
    #[cfg(windows)]
    {
        std::fs::hard_link(source, &destination)
            .or_else(|_| std::fs::copy(source, &destination).map(drop))
            .map_err(LocalError::Alias)
    }
}

fn process_path(directory: &Path) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![directory.to_owned()];
    paths.extend(std::env::split_paths(&inherited));
    std::env::join_paths(paths).unwrap_or_else(|_| directory.as_os_str().to_owned())
}

fn parse_git_version(bytes: &[u8]) -> Result<(u32, u32, u32), LocalError> {
    let text = std::str::from_utf8(bytes).map_err(|_| LocalError::GitVersionSyntax)?;
    let raw = text
        .trim()
        .strip_prefix("git version ")
        .ok_or(LocalError::GitVersionSyntax)?;
    let mut parts = raw.split(['.', ' ', '-']);
    let major = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or(LocalError::GitVersionSyntax)?;
    let minor = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or(LocalError::GitVersionSyntax)?;
    let patch = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or(LocalError::GitVersionSyntax)?;
    Ok((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_path() -> PathBuf {
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let candidate = directory.join(executable_name("git"));
            if candidate.is_file() {
                return candidate.canonicalize().unwrap();
            }
        }
        panic!("Git is required for local process tests")
    }

    #[test]
    fn current_capabilities_satisfy_the_sdk_contract() {
        let current = ExecutableCapabilities::current();
        current.validate_for_sdk().unwrap();
        assert_eq!(
            serde_json::from_str::<ExecutableCapabilities>(CURRENT_CAPABILITIES_JSON).unwrap(),
            current
        );
    }

    #[test]
    fn incompatible_protocol_is_rejected() {
        let mut capabilities = ExecutableCapabilities::current();
        capabilities.local_workflow_protocol += 1;
        assert!(matches!(
            capabilities.validate_for_sdk(),
            Err(CapabilityError::Protocol { .. })
        ));
    }

    #[test]
    fn filter_command_quotes_spaces_quotes_and_unicode() {
        let values = filter_driver_values(Path::new("/opt/Crab tools/海'蟹/crab")).unwrap();
        assert_eq!(
            values[0],
            (
                "filter.crab.process",
                "'/opt/Crab tools/海'\\''蟹/crab' filter-process".to_owned()
            )
        );
        assert_eq!(values[3], ("filter.crab.required", "true".to_owned()));
    }

    #[tokio::test]
    async fn untrusted_processes_disable_configured_executable_drivers() {
        let root = tempfile::tempdir().unwrap();
        let git = git_path();
        let tools = LocalTools::new(&git, &git).unwrap();
        let cancel = CancellationToken::new();
        tools
            .run_git(Some(root.path()), ["init"], true, &cancel)
            .await
            .unwrap();
        tools
            .run_git(
                Some(root.path()),
                [
                    "config",
                    "filter.untrusted.clean",
                    "printf executed > driver-ran; while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
                ],
                true,
                &cancel,
            )
            .await
            .unwrap();
        std::fs::write(
            root.path().join(".gitattributes"),
            "*.txt filter=untrusted\n",
        )
        .unwrap();
        std::fs::write(root.path().join("first.txt"), "first\n").unwrap();

        tools
            .run_git(Some(root.path()), ["add", "first.txt"], false, &cancel)
            .await
            .unwrap();
        assert!(!root.path().join("driver-ran").exists());

        std::fs::write(root.path().join("second.txt"), "second\n").unwrap();
        tools
            .run_git(Some(root.path()), ["add", "second.txt"], true, &cancel)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("driver-ran")).unwrap(),
            b"executed"
        );
    }
}
