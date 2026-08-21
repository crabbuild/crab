//! Compiled-in error catalog mapping `CRAB-E####` codes to long-form
//! explanations. Used by `crab errors {code}` and the `render()`
//! function for user-facing error output.

use crate::core::error::{CrabError, MetaDbError};

/// Long-form explanation for a single error code.
pub struct ErrorExplanation {
    /// The stable error code (e.g. `CRAB-E0001`).
    pub code: &'static str,
    /// One-line summary of the error.
    pub summary: &'static str,
    /// Common causes (newline-separated bullet points).
    pub causes: &'static str,
    /// Suggested remediation steps.
    pub remediation: &'static str,
}

/// User-friendly rendered error message.
pub struct UserMessage {
    /// The formatted message ready for display.
    pub text: String,
}

impl std::fmt::Display for UserMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// All known error codes, sorted by code number.
/// This is the single source of truth for the error catalog.
pub const ALL_CODES: &[&str] = &[
    "CRAB-E0001",
    "CRAB-E0002",
    "CRAB-E0010",
    "CRAB-E0011",
    "CRAB-E0012",
    "CRAB-E0017",
    "CRAB-E0020",
    "CRAB-E0021",
    "CRAB-E0030",
    "CRAB-E0031",
    "CRAB-E0040",
    "CRAB-E0041",
    "CRAB-E0042",
    "CRAB-E0043",
    "CRAB-E0050",
    "CRAB-E0051",
    "CRAB-E0052",
    "CRAB-E0060",
    "CRAB-E0070",
    "CRAB-E0071",
    "CRAB-E0080",
    "CRAB-E0081",
    "CRAB-E0082",
    "CRAB-E0083",
    "CRAB-E0084",
    "CRAB-E0085",
    "CRAB-E0086",
    "CRAB-E0087",
    "CRAB-E0088",
    "CRAB-E0089",
    "CRAB-E0090",
    "CRAB-E0091",
    "CRAB-E0092",
    "CRAB-E0093",
    "CRAB-E0094",
    "CRAB-E0095",
    "CRAB-E0096",
    "CRAB-E0097",
    "CRAB-E0099",
    "CRAB-E0106",
    "CRAB-E0110",
    "CRAB-E0111",
    "CRAB-E0112",
    "CRAB-E0113",
    "CRAB-E0114",
    "CRAB-E0115",
    "CRAB-E0116",
    "CRAB-E0117",
    "CRAB-E0118",
    "CRAB-E0119",
    "CRAB-E0120",
    "CRAB-E0121",
    "CRAB-E0122",
    "CRAB-E0123",
    "CRAB-E0130",
    "CRAB-E0131",
    "CRAB-E0132",
    "CRAB-E0133",
    "CRAB-E0134",
    "CRAB-E0135",
    "CRAB-E0140",
    "CRAB-E0239",
    "CRAB-E0240",
    "CRAB-E0241",
    "CRAB-E0242",
    "CRAB-E0243",
    "CRAB-E0244",
    "CRAB-E0245",
    "CRAB-E0248",
    "CRAB-E0300",
    "CRAB-E0301",
    "CRAB-E0302",
    "CRAB-E0310",
    "CRAB-E0311",
    "CRAB-E0312",
    "CRAB-E0320",
    "CRAB-E0321",
    "CRAB-E0330",
    "CRAB-E0331",
    "CRAB-E0332",
    "CRAB-E0333",
    "CRAB-E0340",
    "CRAB-E0341",
    "CRAB-E0400",
    "CRAB-E0401",
    "CRAB-E0402",
    "CRAB-E0410",
    "CRAB-E0500",
    "CRAB-E0501",
    "CRAB-E0502",
    "CRAB-E0503",
    // CRAB-E0504 reserved (formerly NotBootstrapped; never shipped in the
    // SlateDB-backed metadata layer because fresh repos auto-initialize).
    "CRAB-E0505",
    "CRAB-E0506",
    "CRAB-E0507",
    "CRAB-E0508",
    "CRAB-E0509",
    "CRAB-E050A",
    "CRAB-E050B",
];

/// Look up the long-form explanation for an error code.
#[must_use]
pub fn lookup(code: &str) -> Option<ErrorExplanation> {
    match code {
        "CRAB-E0001" => Some(ErrorExplanation {
            code: "CRAB-E0001",
            summary: "Network transient error",
            causes: "\
  - Temporary network connectivity loss\n\
  - DNS resolution failure\n\
  - Object store endpoint returned a 5xx status",
            remediation: "\
  The operation will be retried automatically with exponential backoff.\n\
  If the error persists, check your network connection and verify the\n\
  object store endpoint is reachable.",
        }),
        "CRAB-E0002" => Some(ErrorExplanation {
            code: "CRAB-E0002",
            summary: "Request throttled by the object store",
            causes: "\
  - Too many concurrent requests to the storage backend\n\
  - Account-level rate limits exceeded",
            remediation: "\
  The operation will be retried after honoring the server's Retry-After\n\
  header. If throttling is frequent, reduce upload/download concurrency\n\
  in your crab config.",
        }),
        "CRAB-E0010" => Some(ErrorExplanation {
            code: "CRAB-E0010",
            summary: "CAS (compare-and-swap) conflict",
            causes: "\
  - Another client updated the same manifest or ref concurrently\n\
  - A parallel push or GC run modified the target path",
            remediation: "\
  The operation will be retried automatically with state refresh.\n\
  If conflicts persist, coordinate with other users pushing to the\n\
  same repository.",
        }),
        "CRAB-E0011" => Some(ErrorExplanation {
            code: "CRAB-E0011",
            summary: "Ref already exists",
            causes: "\
  - Attempted to create a ref that another client already created\n\
  - Race condition during concurrent init or push operations",
            remediation: "\
  Fetch the latest refs and retry your operation. If initializing a\n\
  new repository, verify the remote URL is not already in use.",
        }),
        "CRAB-E0012" => Some(ErrorExplanation {
            code: "CRAB-E0012",
            summary: "Push lock held by another push",
            causes: "\
  - Another crab push is currently in progress for the same ref\n\
  - A previous push is still finalizing uploads and ref commit",
            remediation: "\
  Wait for the other push to complete, then retry. Push locks have a\n\
  short TTL and are reclaimed automatically if the holder crashes.\n\
  If the lock appears stuck, run `crab fsck` to reclaim expired locks.",
        }),
        "CRAB-E0017" => Some(ErrorExplanation {
            code: "CRAB-E0017",
            summary: "Non-fast-forward push rejected",
            causes: "\
  - Another collaborator pushed to the same branch\n\
  - A CI job updated the ref between your fetch and push",
            remediation: "\
  Pull the latest changes and rebase before pushing:\n\
    git pull --rebase && git push",
        }),
        "CRAB-E0020" => Some(ErrorExplanation {
            code: "CRAB-E0020",
            summary: "Corrupt object detected",
            causes: "\
  - Data corruption during transfer or storage\n\
  - Bit rot in the object store\n\
  - Incomplete upload left a partial object",
            remediation: "\
  Run `crab fsck` to identify all corrupt objects. If the object\n\
  exists in another clone, re-push it. One automatic retry is attempted\n\
  before this error surfaces.",
        }),
        "CRAB-E0021" => Some(ErrorExplanation {
            code: "CRAB-E0021",
            summary: "Chunk not found in storage",
            causes: "\
  - The chunk was garbage-collected before it could be fetched\n\
  - A concurrent GC run deleted the chunk during a fetch\n\
  - The shard or xorb containing the chunk is missing",
            remediation: "\
  Run `crab fsck` to check repository integrity. If chunks are\n\
  missing, the data may need to be re-pushed from another clone.",
        }),
        "CRAB-E0030" => Some(ErrorExplanation {
            code: "CRAB-E0030",
            summary: "Object not found in storage",
            causes: "\
  - The requested path does not exist in the object store\n\
  - The repository has not been initialized at this URL\n\
  - A typo in the remote URL",
            remediation: "\
  Verify the remote URL is correct. If the repository should exist,\n\
  check that you have the right bucket and prefix. Run `crab init`\n\
  to create a new repository.",
        }),
        "CRAB-E0031" => Some(ErrorExplanation {
            code: "CRAB-E0031",
            summary: "Access forbidden",
            causes: "\
  - IAM policy denies access to the requested path\n\
  - Bucket policy restricts your credentials\n\
  - The object store requires different permissions",
            remediation: "\
  Check your IAM policies and bucket permissions. Ensure your\n\
  credentials have read/write access to the repository prefix.",
        }),
        "CRAB-E0040" => Some(ErrorExplanation {
            code: "CRAB-E0040",
            summary: "No credentials available",
            causes: "\
  - AWS credentials not configured (no env vars, config file, or instance role)\n\
  - Credential provider chain exhausted without finding valid credentials",
            remediation: "\
  Configure credentials via one of:\n\
    - AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY env vars\n\
    - ~/.aws/credentials file\n\
    - IAM instance role (EC2, ECS, Lambda)\n\
    - AWS SSO: `aws sso login`",
        }),
        "CRAB-E0041" => Some(ErrorExplanation {
            code: "CRAB-E0041",
            summary: "Insufficient local disk space",
            causes: "\
  - The local filesystem does not have enough free space for staging\n\
  - Cache directory is on a full partition",
            remediation: "\
  Free up disk space or move the crab cache/staging directory to a\n\
  larger partition. Run `crab staging clean` to purge stale data\n\
  and `crab cache clean` to clear cached chunks.",
        }),
        "CRAB-E0042" => Some(ErrorExplanation {
            code: "CRAB-E0042",
            summary: "Authentication failed",
            causes: "\
  - Credentials were rejected by the object store\n\
  - Access key or secret key is incorrect\n\
  - SSO session has expired",
            remediation: "\
  Verify your credentials are correct. If using AWS SSO, run\n\
  `aws sso login` to refresh your session.",
        }),
        "CRAB-E0043" => Some(ErrorExplanation {
            code: "CRAB-E0043",
            summary: "Credentials expired",
            causes: "\
  - Temporary credentials (STS) have passed their expiration time\n\
  - SSO session token has expired",
            remediation: "\
  Refresh your credentials:\n\
    - For SSO: `aws sso login`\n\
    - For STS: request new temporary credentials\n\
    - For instance roles: credentials should auto-refresh",
        }),
        "CRAB-E0050" => Some(ErrorExplanation {
            code: "CRAB-E0050",
            summary: "Configuration error",
            causes: "\
  - Malformed TOML in ~/.config/crab/config.toml or .crab/config.toml\n\
  - Invalid value for a configuration key\n\
  - Unrecognized configuration key in a config file",
            remediation: "\
  Check the identified config file and key for typos or invalid values.\n\
  Refer to `crab --help` or the documentation for valid configuration\n\
  options and their expected formats.",
        }),
        "CRAB-E0051" => Some(ErrorExplanation {
            code: "CRAB-E0051",
            summary: "Incompatible format version",
            causes: "\
  - The remote repository requires a newer crab CLI version\n\
  - The `required_cli_version` in remote config is higher than the running binary",
            remediation: "\
  Upgrade crab to the version required by the remote repository.\n\
  Check the current version with `crab version` and install the\n\
  latest release.",
        }),
        "CRAB-E0052" => Some(ErrorExplanation {
            code: "CRAB-E0052",
            summary: "Invalid glob pattern",
            causes: "\
  - A glob pattern in --include or --exclude has invalid syntax\n\
  - Unmatched brackets or braces in the pattern\n\
  - Invalid character class in the pattern",
            remediation: "\
  Check the glob pattern syntax. Supported wildcards: `*` matches\n\
  within a path component, `**` matches across path components,\n\
  `?` matches a single character. Example: `crab hydrate \"*.bin\"`.",
        }),
        "CRAB-E0060" => Some(ErrorExplanation {
            code: "CRAB-E0060",
            summary: "Remote helper protocol error",
            causes: "\
  - Git sent an unexpected command to the remote helper\n\
  - Protocol framing mismatch between git and crab\n\
  - Corrupted pipe between git and the remote helper process",
            remediation: "\
  Ensure your git version is compatible with crab. Try running the\n\
  git command again. If the error persists, check that the crab\n\
  binary is correctly installed and accessible in your PATH.",
        }),
        "CRAB-E0070" => Some(ErrorExplanation {
            code: "CRAB-E0070",
            summary: "I/O error",
            causes: "\
  - Local filesystem read/write failure\n\
  - Disk full or permission denied on local paths\n\
  - Broken pipe when communicating with git",
            remediation: "\
  Check local filesystem permissions and available disk space.\n\
  Ensure the .git directory and crab staging area are writable.",
        }),
        "CRAB-E0071" => Some(ErrorExplanation {
            code: "CRAB-E0071",
            summary: "Object store error",
            causes: "\
  - Unclassified error from the storage backend (S3, GCS, Azure)\n\
  - The error did not match a more specific category",
            remediation: "\
  Check the inner error message for details. Verify your object store\n\
  configuration, network connectivity, and credentials.",
        }),
        "CRAB-E0080" => Some(ErrorExplanation {
            code: "CRAB-E0080",
            summary: "Staging area corrupt",
            causes: "\
  - The local staging database was corrupted by a crash or disk error\n\
  - Concurrent processes wrote conflicting staging data",
            remediation: "\
  Run `crab staging clean` to purge and rebuild the staging area.\n\
  If the error persists, delete the .crab/staging directory and\n\
  retry your operation.",
        }),
        "CRAB-E0081" => Some(ErrorExplanation {
            code: "CRAB-E0081",
            summary: "Staging area locked by another process",
            causes: "\
  - Another crab process is currently writing to the staging area\n\
  - A previous process crashed without releasing the lock",
            remediation: "\
  Wait a moment and retry — the lock includes automatic retry with\n\
  backoff. If the error persists, check for zombie crab processes\n\
  with `ps aux | grep crab`. To break a stale lock from a dead\n\
  process, run `crab staging clean --force`.",
        }),
        "CRAB-E0082" => Some(ErrorExplanation {
            code: "CRAB-E0082",
            summary: "Chunk hash mismatch",
            causes: "\
  - Data corruption during transfer\n\
  - The chunk content does not match its content-addressed hash\n\
  - Storage backend returned different data than what was written",
            remediation: "\
  Run `crab fsck` to identify all integrity issues. The chunk may\n\
  need to be re-pushed from a healthy clone.",
        }),
        "CRAB-E0083" => Some(ErrorExplanation {
            code: "CRAB-E0083",
            summary: "Segment CRC mismatch",
            causes: "\
  - Data corruption in a staging segment file\n\
  - Disk error or incomplete write to the staging area",
            remediation: "\
  Run `crab staging clean` to purge corrupt segments and retry\n\
  the operation. If the error recurs, check your disk for errors.",
        }),
        "CRAB-E0084" => Some(ErrorExplanation {
            code: "CRAB-E0084",
            summary: "Pack integrity check failed",
            causes: "\
  - The pack file's computed hash does not match the expected hash\n\
  - Data corruption during upload or download\n\
  - Partial pack write due to a crash",
            remediation: "\
  Run `crab fsck --repair` to attempt recovery. If the pack is\n\
  corrupt beyond repair, it may need to be re-pushed from another clone.",
        }),
        "CRAB-E0085" => Some(ErrorExplanation {
            code: "CRAB-E0085",
            summary: "Shard reconstruction incomplete",
            causes: "\
  - At push time: the push pipeline detected a gap in the merged chunk\n\
    placement map (a packer regression or a stale three-tier dedup\n\
    lookup result). Observed on large files (roughly 5,000+ CDC chunks)\n\
    where this fail-loud guard triggered instead of a push completing\n\
    with corrupt shards that would only surface at hydrate time.\n\
  - At hydrate time: a previously-pushed shard's reconstruction terms\n\
    cover fewer bytes than the pointer's declared size — the remote\n\
    is missing chunks for this file. The push that wrote the shard\n\
    was incomplete, and the missing xorbs have not been uploaded.",
            remediation: "\
  Push-time triggers: file a bug report with the error details (file_hash,\n\
  example_chunk_index, example_chunk_hash). Avoid re-pushing the same\n\
  file until the packer-level root cause is fixed — repeated pushes\n\
  will hit the same guard. Run `crab fsck` on the repository to check\n\
  for related integrity issues.\n\n\
  Hydrate-time triggers: the file cannot be reconstructed from the\n\
  remote alone (its chunks were never fully uploaded). If you still\n\
  have the original file locally, recover with:\n\
    `crab hydrate --recover-from <path-to-original> <pointer-path>`\n\
  This verifies the local file's blake3 hash matches the pointer and\n\
  copies it into place. After recovery, repair the remote with:\n\
    `crab add <path> && git add <path> && git commit --amend --no-edit \\\n\
       && git push --force`\n\
  to regenerate a complete shard.",
        }),
        "CRAB-E0086" => Some(ErrorExplanation {
            code: "CRAB-E0086",
            summary: "Pointer has no staged chunks",
            causes: "\
  - A commit reachable from the pushed ref contains a pointer blob\n\
    whose file content was never chunked into the local staging area\n\
    (`.crab/staging/`)\n\
  - Commonly seen after cloning a repo, running `crab gc`, or\n\
    deleting `.crab/staging/` — the committed pointers survive but\n\
    the chunk bytes needed to push them do not\n\
  - Also triggered when a file is committed via a non-crab tool or\n\
    on a branch where the clean filter never ran",
            remediation: "\
  Re-run `crab add <path>` for each affected file so the clean\n\
  filter records chunk offsets in `.crab/staging/`. The pointer\n\
  blob in the working tree is already correct — this only refills\n\
  the local staging index.\n\
  Use `crab doctor` to list every committed pointer whose chunks\n\
  are missing locally.",
        }),
        "CRAB-E0087" => Some(ErrorExplanation {
            code: "CRAB-E0087",
            summary: "Push connectivity check failed",
            causes: "\
  - An object (commit, tree, or blob) reachable from the ref tip\n\
    being pushed is missing from the local git object database\n\
  - The pack generated for this push does not cover the full\n\
    reachable set, which would leave the remote ref pointing at\n\
    history the fetcher can't reconstruct\n\
  - Commonly indicates local ODB corruption (`.git/objects/`\n\
    missing files) or a pack-generation edge case where an\n\
    exclusion list filtered a legitimate object",
            remediation: "\
  Verify the missing OID reported in the error exists locally:\n\
    git cat-file -e <oid>\n\
  If the object is absent, restore it from a backup or fetch from\n\
  another remote (`git fetch origin`). If all copies are gone,\n\
  force-push from a reference clone that still has the object.\n\
  If the object IS present locally, run `git fsck --full` to\n\
  surface related integrity issues and `crab fsck` for the\n\
  content-addressed side. Re-run the push once connectivity is\n\
  restored; crab never commits a ref that would orphan reachable\n\
  history.",
        }),
        "CRAB-E0088" => Some(ErrorExplanation {
            code: "CRAB-E0088",
            summary: "Push pipeline failed with partial outcomes",
            causes: "\
  - One or more refs in a multi-ref push batch failed, but other\n\
    refs in the same batch had already been decided (pre-flight\n\
    checks passed or a specific rejection reason was produced)\n\
  - The pipeline wraps the partial outcomes so the remote helper\n\
    can surface per-ref results instead of collapsing the whole\n\
    batch to a single error string\n\
  - Common triggers: connectivity check failure on one ref while\n\
    sibling refs passed, or a CAS retry loop exhausted with one\n\
    ref still in a non-fast-forward state",
            remediation: "\
  Inspect the per-ref outcomes in the response: refs marked `ok`\n\
  would have committed if the failing ref had not aborted the\n\
  batch; refs with `Rejected(reason)` carry a structured reason\n\
  (non-fast-forward, missing-object, lock-contention, and so on).\n\
  Fix the failing ref (rebase for non-fast-forward, restore the\n\
  missing object for connectivity, wait for lock-contention) and\n\
  re-push. The surviving refs in the batch are not yet on the\n\
  remote — the whole batch rolls back when the pipeline aborts.",
        }),
        "CRAB-E0089" => Some(ErrorExplanation {
            code: "CRAB-E0089",
            summary: "File changed during staging",
            causes: "\
  - Another process rewrote the file while `crab add` or `crab adopt`\n\
    was reading and chunking it\n\
  - A model checkpoint, export job, or data-generation process was\n\
    still writing to the target path during staging\n\
  - The first streaming pass observed different bytes than the second\n\
    streaming pass",
            remediation: "\
  Ensure the file is no longer being written, then rerun the command.\n\
  Crab discards any partial staged rows for the stale file hash before\n\
  surfacing this error, so a clean rerun will rebuild the staging entry.\n\
  For generated artifacts, write to a temporary path and atomically\n\
  rename into place before running `crab add` or `crab adopt`.",
        }),
        "CRAB-E0090" => Some(ErrorExplanation {
            code: "CRAB-E0090",
            summary: "Operation cancelled",
            causes: "\
  - User sent SIGINT (Ctrl+C) or SIGTERM\n\
  - Graceful shutdown was triggered by a signal handler",
            remediation: "\
  This is expected when you cancel an operation. Any held locks have\n\
  been released and staging state has been flushed. You can safely\n\
  retry the operation.",
        }),
        "CRAB-E0091" => Some(ErrorExplanation {
            code: "CRAB-E0091",
            summary: "Commit beyond shallow boundary",
            causes: "\
  - The repository was cloned with --depth N and the requested commit\n\
    is older than the shallow boundary\n\
  - A git operation tried to access history that was not fetched",
            remediation: "\
  Run `git fetch --deepen=N` to extend the shallow history by N commits,\n\
  or `git fetch --unshallow` to convert to a full clone with all history.",
        }),
        "CRAB-E0092" => Some(ErrorExplanation {
            code: "CRAB-E0092",
            summary: "Pack too large",
            causes: "\
  - One Git object cannot fit in a pack bounded by `receive.maxInputSize`\n\
    (default 2 GiB, mirroring git's server-side cap)\n\
  - A hostile or broken client attempted to upload an oversized pack\n\
  - A legitimate individual object exceeds a limit the repo admin has not\n\
    raised",
            remediation: "\
  Crab automatically splits aggregate history into multiple bounded packs,\n\
  so splitting the push does not fix an oversized individual object. Admins\n\
  can raise or disable the cap by setting `crab.receive.maxInputSize`\n\
  in the repo config — `0` disables the check entirely for trusted\n\
  internal repos.",
        }),
        "CRAB-E0093" => Some(ErrorExplanation {
            code: "CRAB-E0093",
            summary: "Malformed object rejected by push fsck",
            causes: "\
  - A commit or tree in the push is not in git's canonical encoding:\n\
    out-of-order tree entries, an invalid file mode, a missing\n\
    required commit header (tree/author/committer), or a\n\
    malformed timestamp\n\
  - Crab always validates object bodies while indexing each generated\n\
    pack before publication",
            remediation: "\
  Fix the local history so the offending object parses cleanly —\n\
  `git fsck --strict` will surface the same violations. Semantic\n\
  validation is mandatory before Crab publishes a manifest.",
        }),
        "CRAB-E0094" => Some(ErrorExplanation {
            code: "CRAB-E0094",
            summary: "Fetch not allowed",
            causes: "\
  - The server's `uploadpack.allowAnySHA1InWant`,\n\
    `uploadpack.allowTipSHA1InWant`, and\n\
    `uploadpack.allowReachableSHA1InWant` config determines which\n\
    SHAs clients may request directly in `want` lines\n\
  - The client asked for a SHA that does not satisfy the currently\n\
    active upload-pack policy (e.g. an interior commit when only\n\
    ref tips are allowed, or a hidden-ref target)",
            remediation: "\
  Push the commit first so it becomes a ref tip, ask the repository\n\
  admin to enable `uploadpack.allowReachableSHA1InWant` or\n\
  `uploadpack.allowAnySHA1InWant`, or fetch by advertised ref name\n\
  instead of raw SHA.",
        }),
        "CRAB-E0095" => Some(ErrorExplanation {
            code: "CRAB-E0095",
            summary: "Fetch too large",
            causes: "\
  - The server enforces `uploadpack.maxEgressBytes` to bound\n\
    per-fetch transfer cost\n\
  - The pack set the fetch was about to install exceeded that\n\
    limit, so the fetch was cancelled before the full payload\n\
    downloaded",
            remediation: "\
  Narrow the fetch with `--depth=N` or `--filter=blob:none`, ask\n\
  the repository admin to raise `uploadpack.maxEgressBytes`, or\n\
  use `crab hydrate` for selective partial checkout after a\n\
  shallow clone.",
        }),
        "CRAB-E0096" => Some(ErrorExplanation {
            code: "CRAB-E0096",
            summary: "Fetched manifest inventory is incomplete",
            causes: "\
  - An advertised ref tip is absent after every pack in the committed\n\
    manifest inventory was installed\n\
  - A pack or index was missing, corrupt, or inconsistent with the\n\
    publication manifest",
            remediation: "\
  Every newly installed pack in the rejected batch was rolled back, so the\n\
  working tree is safe. Ask the remote repository admin to fix the\n\
  committed pack inventory and republish it. `git fsck --strict` on an\n\
  independently reconstructed repository can identify the missing edge.",
        }),
        "CRAB-E0097" => Some(ErrorExplanation {
            code: "CRAB-E0097",
            summary: "Push integration rebase failed",
            causes: "\
  - `crab push --rebase-on-non-fast-forward` tried to run\n\
    `git pull --rebase --autostash` after a same-branch\n\
    non-fast-forward race\n\
  - Git could not apply the local commit on top of the remote tip,\n\
    usually because another agent edited the same lines or files\n\
  - This is a normal collaboration conflict, not object-store\n\
    corruption and not a retryable Crab transport failure",
            remediation: "\
  Inspect the local rebase state with `git status`. Resolve the\n\
  conflicts, run `git add` for resolved paths, then `git rebase --continue`\n\
  and push again. To abandon the local integration attempt, run\n\
  `git rebase --abort`. Agents should not blindly retry the same push\n\
  until the semantic conflict is resolved.",
        }),
        "CRAB-E0099" => Some(ErrorExplanation {
            code: "CRAB-E0099",
            summary: "Internal error (bug)",
            causes: "\
  - An unexpected condition occurred inside crab\n\
  - This indicates a bug in the crab implementation",
            remediation: "\
  Please report this error with the full output and reproduction steps\n\
  at the crab issue tracker. Include the error message and any\n\
  relevant context (repository size, operation being performed).",
        }),
        "CRAB-E0106" => Some(ErrorExplanation {
            code: "CRAB-E0106",
            summary: "LFS command is not yet safely supported",
            causes: "\
  - The requested LFS command needs object-store verification that is not wired yet\n\
  - Continuing would make the command look successful without proving data safety\n\
  - Another Crab or Git LFS flow may support the same high-level workflow safely",
            remediation: "\
  Use the supported Git LFS transfer-agent flow or `crab lfs migrate` where applicable.\n\
  Do not rely on this command until Crab implements the missing verification contract.",
        }),
        "CRAB-E0110" => Some(ErrorExplanation {
            code: "CRAB-E0110",
            summary: "Cache service error",
            causes: "\
  - The cache service is unreachable (connection refused, DNS failure)\n\
  - The cache service timed out\n\
  - The cache service returned an HTTP error (4xx/5xx)",
            remediation: "\
  The client will fall back to the origin object store automatically.\n\
  Check that the cache service URL in [cache] config is correct and\n\
  the service is running. This error is non-fatal for normal operations.",
        }),
        "CRAB-E0114" => Some(ErrorExplanation {
            code: "CRAB-E0114",
            summary: "Import plan mismatch",
            causes: "\
  - `crab import --resume` was passed arguments that disagree with\n\
    the plan recorded in the resume journal\n\
  - The source or target URL, include/exclude globs, branch,\n\
    versioning mode, window, or history range has changed between\n\
    the original run and the resume attempt",
            remediation: "\
  Either re-run `crab import --resume` from the same working\n\
  directory without conflicting flags (resume reads the plan from\n\
  the journal), or start a fresh import into a new `--into`\n\
  directory. Delete `{into}/.crab/import-journal.db` only if you\n\
  are certain you want to discard previous progress.",
        }),
        "CRAB-E0115" => Some(ErrorExplanation {
            code: "CRAB-E0115",
            summary: "Import journal missing",
            causes: "\
  - `crab import --resume` was run against an --into directory\n\
    that has no `.crab/import-journal.db`\n\
  - A previous successful run already cleaned up the journal\n\
  - The wrong --into directory was passed",
            remediation: "\
  Start a fresh run without --resume (pass --from / --to), or point\n\
  --into at the directory where the interrupted run's journal lives.\n\
  Journals are preserved across failures and removed only after a\n\
  full pipeline success.",
        }),
        "CRAB-E0118" => Some(ErrorExplanation {
            code: "CRAB-E0118",
            summary: "Import source must be a raw cloud URL",
            causes: "\
  - `crab import --from` was given a crab:// URL\n\
  - The user meant to clone an existing Crab repo rather than import raw objects",
            remediation: "\
  Pass a raw cloud URL to --from (one of s3://, gs://, az://, azure://,\n\
  or file:///absolute/path). If the source is already a Crab repo,\n\
  use `crab clone` instead of `crab import`.",
        }),
        "CRAB-E0119" => Some(ErrorExplanation {
            code: "CRAB-E0119",
            summary: "Import source and target schemes disagree",
            causes: "\
  - --from and --to named different raw cloud schemes (e.g. s3:// vs az://)\n\
  - A cross-cloud import was attempted without opting in via crab:// on --to",
            remediation: "\
  Either make --from and --to use the same raw scheme, or write the\n\
  target as crab://<bucket>/<prefix> to make the cross-cloud intent\n\
  explicit. The crab:// form carries the provider via config rather\n\
  than the URL.",
        }),
        "CRAB-E0120" => Some(ErrorExplanation {
            code: "CRAB-E0120",
            summary: "Import versioning unavailable",
            causes: "\
  - `crab import --versions on` was invoked against a source bucket\n\
    that does not have object versioning enabled\n\
  - The source prefix's version sample showed no duplicates and no\n\
    delete markers, so there is no version history to import",
            remediation: "\
  Either enable object versioning on the source bucket and retry, or\n\
  drop `--versions on` and use `--versions auto` (default) to import\n\
  the current state as a single commit. Pass `--versions off` to\n\
  force flat-mode import even if the bucket is versioned.",
        }),
        "CRAB-E0121" => Some(ErrorExplanation {
            code: "CRAB-E0121",
            summary: "Import commit ceiling exceeded",
            causes: "\
  - The window planner produced more commits than --max-commits allows\n\
  - A very narrow --window against a dense version history fans out\n\
    into an unmanageable git history (e.g. --window 1s on a bucket\n\
    with millions of versions)",
            remediation: "\
  Widen --window (the default is 1 hour) so fewer, larger commits\n\
  cover the same history. For example, --window 24h collapses a day's\n\
  versions into one commit. If the resulting count is still valid,\n\
  raise --max-commits (default 100000) to explicitly accept the\n\
  larger history.",
        }),
        "CRAB-E0122" => Some(ErrorExplanation {
            code: "CRAB-E0122",
            summary: "Import history range is invalid",
            causes: "\
  - `--since` was specified after `--until`\n\
  - The requested history window is empty by construction",
            remediation: "\
  Swap the two bounds so --since is earlier than --until, or drop one\n\
  of the flags entirely. Both arguments accept RFC3339 timestamps\n\
  (for example 2025-01-01T00:00:00Z).",
        }),
        "CRAB-E0123" => Some(ErrorExplanation {
            code: "CRAB-E0123",
            summary: "Import target directory is not empty",
            causes: "\
  - The directory passed to --into already contains files\n\
  - A previous crab import left partial state in the directory\n\
  - The user aimed --into at a non-empty working tree by mistake",
            remediation: "\
  Either pick an empty --into directory, remove the existing contents,\n\
  or pass --force to accept overwriting the directory. An empty,\n\
  freshly-initialized git repo (zero commits, empty working tree) is\n\
  always accepted.",
        }),
        "CRAB-E0116" => Some(ErrorExplanation {
            code: "CRAB-E0116",
            summary: "Git identity not configured for import",
            causes: "\
  - `user.name` or `user.email` is empty in the git config\n\
  - Import needs a configured identity to sign commits; git commit\n\
    would otherwise fail late in the pipeline with an opaque message",
            remediation: "\
  Configure git with:\n\
    git config --global user.name 'Your Name'\n\
    git config --global user.email 'you@example.com'\n\
  Or pass `--author-template` to set a per-commit author without\n\
  touching the global identity (the committer still comes from the\n\
  configured identity).",
        }),
        "CRAB-E0117" => Some(ErrorExplanation {
            code: "CRAB-E0117",
            summary: "Import source prefix collides with target layout",
            causes: "\
  - `--from` and `--to` resolve to the same bucket AND the source\n\
    prefix overlaps the target `.crab/` layout\n\
  - The CDC pipeline would read from the very xorbs it's about to\n\
    write — the resulting history would never be coherent",
            remediation: "\
  Point `--from` and `--to` at non-overlapping prefixes within the\n\
  same bucket, or move one of them to a sibling prefix that doesn't\n\
  share an ancestor with the target `.crab/` layout. There is no\n\
  `--force` override for this error.",
        }),
        "CRAB-E0111" => Some(ErrorExplanation {
            code: "CRAB-E0111",
            summary: "Import target repo already has an origin remote",
            causes: "\
  - The target `--into` directory is already a git repo with an\n\
    `origin` remote pointing somewhere else\n\
  - A partial prior import configured origin and was interrupted",
            remediation: "\
  Pass `--force` to overwrite the existing origin URL with the import\n\
  target URL, or remove the existing remote manually with\n\
  `git remote remove origin` before retrying the import.",
        }),
        "CRAB-E0140" => Some(ErrorExplanation {
            code: "CRAB-E0140",
            summary: "Managed repository request failed",
            causes: "\
  - The managed locator or installed service profile is invalid\n\
  - Login is required or repository access is denied\n\
  - The service API is incompatible or temporarily unavailable\n\
  - A short-lived transfer grant expired",
            remediation: "\
  Follow the action in the error message. Login failures should be fixed with\n\
  `crab login https://<authority>`. Compatibility failures require upgrading\n\
  Crab or the service. Retry only service-unavailable or expired-grant errors.",
        }),
        "CRAB-E0112" => Some(ErrorExplanation {
            code: "CRAB-E0112",
            summary: "Import source prefix is already a Crab repo",
            causes: "\
  - `--from` points at a bucket/prefix that already hosts a\n\
    Crab-backed repo (refs/HEAD or manifests/ present)\n\
  - The user meant to `crab clone` rather than `crab import`",
            remediation: "\
  Use `crab clone <url>` to clone an existing Crab repo.\n\
  If you truly want to re-import the raw bytes under that prefix,\n\
  pass `--force`.",
        }),
        "CRAB-E0113" => Some(ErrorExplanation {
            code: "CRAB-E0113",
            summary: "Import source uses Git LFS format",
            causes: "\
  - The source prefix contains a `.gitattributes` file that\n\
    declares `filter=lfs` for some paths\n\
  - The objects under the prefix are LFS pointer blobs, not the\n\
    underlying binary content",
            remediation: "\
  Use `crab import --lfs-source resolve --lfs-objects <URL>` to\n\
  rehydrate pointers through the companion LFS object store during\n\
  import. Use `--lfs-source skip` only when you intentionally want\n\
  LFS pointer paths omitted from the imported Crab repository.",
        }),
        "CRAB-E0130" => Some(ErrorExplanation {
            code: "CRAB-E0130",
            summary: "Git pull encountered merge conflicts",
            causes: "\
  - The remote branch has changes that conflict with local modifications\n\
  - A concurrent push modified the same files you have locally",
            remediation: "\
  Resolve the merge conflicts in the listed files, then run\n\
  `crab hydrate` to materialize any newly-fetched pointer blobs.\n\
  Use `git status` to see which files need attention.",
        }),
        "CRAB-E0131" => Some(ErrorExplanation {
            code: "CRAB-E0131",
            summary: "Remote unreachable during pull",
            causes: "\
  - The configured git remote is not reachable (DNS failure, network down)\n\
  - The remote URL is misconfigured\n\
  - SSH key or HTTPS credentials are rejected by the remote host",
            remediation: "\
  Check your network connection and verify the remote URL with\n\
  `git remote -v`. If using SSH, ensure your key is loaded\n\
  (`ssh-add -l`). Retry after connectivity is restored.",
        }),
        "CRAB-E0132" => Some(ErrorExplanation {
            code: "CRAB-E0132",
            summary: "Unadopt chunks missing from staging",
            causes: "\
  - The file was adopted but the staging area has since been cleaned\n\
    or compacted, removing the original chunks\n\
  - The staging area was deleted or corrupted\n\
  - The file was adopted in a different working tree or clone",
            remediation: "\
  Use `git checkout -- <file>` to restore the file from the last committed\n\
  version. If the file was never committed, the original content may be\n\
  unrecoverable from crab's staging area.",
        }),
        "CRAB-E0133" => Some(ErrorExplanation {
            code: "CRAB-E0133",
            summary: "Nothing to undo",
            causes: "\
  - No crab operation (adopt, add) was detected in the current git\n\
    staged changes\n\
  - The staged changes do not contain any pointer files\n\
  - A previous undo already reversed the operation",
            remediation: "\
  Use `crab unadopt --pattern <glob>` to explicitly specify files to\n\
  restore, or check `git status` to verify which files are staged.",
        }),
        "CRAB-E0134" => Some(ErrorExplanation {
            code: "CRAB-E0134",
            summary: "Invalid config key",
            causes: "\
  - The key passed to `crab config get` or `crab config set` is not\n\
    in the list of recognized configuration keys\n\
  - A typo in the dotted key name (e.g. `remot.url` instead of `remote.url`)",
            remediation: "\
  Check the key name against the valid keys listed in the error message.\n\
  Use `crab config get --help` to see available configuration keys.",
        }),
        "CRAB-E0135" => Some(ErrorExplanation {
            code: "CRAB-E0135",
            summary: "Unsupported shell for completions",
            causes: "\
  - The shell name passed to `crab completions` is not recognized\n\
  - Supported shells: bash, zsh, fish, powershell",
            remediation: "\
  Use one of the supported shell names: bash, zsh, fish, powershell.\n\
  Example: `crab completions bash`.",
        }),
        "CRAB-E0300" => Some(ErrorExplanation {
            code: "CRAB-E0300",
            summary: "Lifecycle rule conflict",
            causes: "\
  - An existing lifecycle rule on the bucket has the same prefix or ID\n\
    as a Crab-managed rule but with different settings\n\
  - A concurrent `tier plan --apply` modified the rules between read and write",
            remediation: "\
  Use `--merge` to replace only Crab-managed rules (those with IDs\n\
  prefixed `crab-`) while preserving user-managed rules. Or remove\n\
  the conflicting rule manually via the provider console.",
        }),
        "CRAB-E0301" => Some(ErrorExplanation {
            code: "CRAB-E0301",
            summary: "Not authorized to apply lifecycle rules",
            causes: "\
  - The current credentials lack the IAM permission needed to write\n\
    lifecycle configuration\n\
  - S3: missing `s3:PutLifecycleConfiguration`\n\
  - GCS: missing `storage.buckets.update`\n\
  - Azure: missing `Microsoft.Storage/storageAccounts/managementPolicies/write`",
            remediation: "\
  Grant the listed IAM permission to your credentials. See\n\
  `docs/guides/crab-tier.md#iam` for the minimum permission set\n\
  per provider.",
        }),
        "CRAB-E0302" => Some(ErrorExplanation {
            code: "CRAB-E0302",
            summary: "Provider not supported for tiering",
            causes: "\
  - The bucket URL scheme does not match a supported provider\n\
  - V1 supports S3, GCS, and Azure Blob Storage",
            remediation: "\
  Ensure your bucket URL uses one of: s3://, gs://, az://.\n\
  Other providers are not yet supported for lifecycle tiering.",
        }),
        "CRAB-E0310" => Some(ErrorExplanation {
            code: "CRAB-E0310",
            summary: "Archive restore required before read",
            causes: "\
  - The xorb is stored in an archive storage class (Glacier, Deep Archive,\n\
    Azure Archive) and must be restored before it can be read\n\
  - `hydrate.auto_restore` is disabled or `--no-restore` was passed",
            remediation: "\
  Enable auto-restore in config (`hydrate.auto_restore = true`) or\n\
  pass `--restore` on the command line. Alternatively, issue a manual\n\
  restore via the provider console and retry after the restore completes.",
        }),
        "CRAB-E0311" => Some(ErrorExplanation {
            code: "CRAB-E0311",
            summary: "Archive restore timed out",
            causes: "\
  - The restore request was submitted but did not complete within\n\
    `hydrate.restore_timeout_secs` (default 6 hours)\n\
  - Deep Archive restores can take up to 48 hours for Bulk tier",
            remediation: "\
  The restore continues provider-side. Retry the hydrate later when\n\
  the restore has completed. Consider using a faster restore tier\n\
  (e.g. `--restore-tier=standard` instead of `bulk`).",
        }),
        "CRAB-E0312" => Some(ErrorExplanation {
            code: "CRAB-E0312",
            summary: "Restore tier not supported for storage class",
            causes: "\
  - The requested restore tier is not valid for the object's storage class\n\
  - Example: `Expedited` is not available for Glacier Deep Archive\n\
  - Example: `Bulk` is not available for Azure Archive",
            remediation: "\
  Use a supported restore tier for the storage class. See the\n\
  provider-tier matrix in `docs/guides/crab-tier.md` for valid\n\
  combinations.",
        }),
        "CRAB-E0320" => Some(ErrorExplanation {
            code: "CRAB-E0320",
            summary: "GC early delete blocked by minimum retention",
            causes: "\
  - The object has not yet reached the minimum retention period for\n\
    its storage class\n\
  - Deleting it now would incur an early-deletion penalty from the\n\
    provider",
            remediation: "\
  Wait until the object reaches the minimum retention age for its\n\
  class. To proceed anyway, pass `--force-early-delete --yes-really`\n\
  (the estimated penalty is shown in the error message).",
        }),
        "CRAB-E0321" => Some(ErrorExplanation {
            code: "CRAB-E0321",
            summary: "Object locked by retention policy",
            causes: "\
  - The object is under an object-lock retention policy\n\
  - The retention period has not yet expired\n\
  - This cannot be overridden even with `--force-early-delete`",
            remediation: "\
  Wait until the retention period expires (the expiry timestamp is\n\
  shown in the error message). Object-lock retention cannot be\n\
  bypassed.",
        }),
        "CRAB-E0330" => Some(ErrorExplanation {
            code: "CRAB-E0330",
            summary: "Xorb optimization profile target size out of range",
            causes: "\
  - `target_xorb_bytes` is outside the allowed range of 4 MiB to 2 GiB\n\
  - A custom profile in `[restripe.profiles.<name>]` has an invalid value",
            remediation: "\
  Set `target_xorb_bytes` to a value between 4194304 (4 MiB) and\n\
  2147483648 (2 GiB). Use one of the built-in profiles (`ml`,\n\
  `dataset`, `code`) as a starting point.",
        }),
        "CRAB-E0331" => Some(ErrorExplanation {
            code: "CRAB-E0331",
            summary: "Xorb optimization source xorb is corrupt",
            causes: "\
  - The source xorb's content hash does not match its expected hash\n\
  - Data corruption in the object store",
            remediation: "\
  The corrupt xorb is skipped and xorb optimization continues. Run\n\
  `crab fsck` after `crab optimize xorbs` completes to identify and\n\
  remediate all corrupt objects.",
        }),
        "CRAB-E0332" => Some(ErrorExplanation {
            code: "CRAB-E0332",
            summary: "Xorb optimization already in progress",
            causes: "\
  - Another `crab optimize xorbs` process holds the exclusive lock on the journal\n\
  - A previous xorb optimization crashed and left a stale lock",
            remediation: "\
  Wait for the other xorb optimization to finish. If the process is dead,\n\
  use `crab optimize xorbs --drop-journal --yes-really` to clear the\n\
  stale lock and start fresh.",
        }),
        "CRAB-E0333" => Some(ErrorExplanation {
            code: "CRAB-E0333",
            summary: "Concurrent maintenance operation detected",
            causes: "\
  - GC and `crab optimize xorbs` cannot run at the same time\n\
  - The other operation's lock or journal is active",
            remediation: "\
  Wait for the other maintenance operation to complete before\n\
  starting this one. GC and xorb optimization are mutually exclusive to\n\
  prevent data races.",
        }),
        "CRAB-E0340" => Some(ErrorExplanation {
            code: "CRAB-E0340",
            summary: "Pricing data missing for provider/region",
            causes: "\
  - The embedded price table does not include the requested\n\
    provider/region combination\n\
  - No user override file covers the missing region",
            remediation: "\
  Supply a `--pricing-file` with pricing data for the missing\n\
  region. See `docs/guides/crab-cost.md` for the override\n\
  file format.",
        }),
        "CRAB-E0341" => Some(ErrorExplanation {
            code: "CRAB-E0341",
            summary: "Inventory report is stale",
            causes: "\
  - The provider-side inventory report is older than\n\
    `cost.report_max_staleness_hours` (default 48 hours)\n\
  - The report may not reflect recent uploads or deletions",
            remediation: "\
  Regenerate the inventory report via the provider console, or\n\
  switch to live inventory with `--inventory-source=live`.",
        }),
        "CRAB-E0400" => Some(ErrorExplanation {
            code: "CRAB-E0400",
            summary: "Manifest parse error",
            causes: "\
  - The manifest file contains a malformed line\n\
  - An invalid glob pattern was encountered",
            remediation: "\
  Check the manifest file for syntax errors at the reported line.",
        }),
        "CRAB-E0401" => Some(ErrorExplanation {
            code: "CRAB-E0401",
            summary: "Prefetch config error",
            causes: "\
  - `.crab/prefetch.toml` is malformed or contains invalid globs",
            remediation: "\
  Validate the TOML syntax and glob patterns in `.crab/prefetch.toml`.",
        }),
        "CRAB-E0402" => Some(ErrorExplanation {
            code: "CRAB-E0402",
            summary: "Prefetch profile not found",
            causes: "\
  - The requested profile name does not exist in `.crab/prefetch.toml`",
            remediation: "\
  Check available profile names in `.crab/prefetch.toml` and retry\n\
  with a valid `--profile=<name>`.",
        }),
        "CRAB-E0410" => Some(ErrorExplanation {
            code: "CRAB-E0410",
            summary: "Speculation database error",
            causes: "\
  - The current worktree's SQLite access database is corrupt or\n\
    inaccessible\n\
  - Disk full or permission denied on the `.crab/worktrees/<id>/` directory",
            remediation: "\
  Run `crab hydrate --clear-speculation` from the affected worktree or\n\
  delete that worktree's `.crab/worktrees/<id>/access.db` file.\n\
  Speculation data is advisory and will be rebuilt from future access\n\
  patterns.",
        }),
        "CRAB-E0500" => Some(ErrorExplanation {
            code: "CRAB-E0500",
            summary: "MetaDB open failed",
            causes: "\
  - SlateDB manifest or WAL segments could not be read from object storage\n\
  - Credentials expired between crab startup and the first metadb access\n\
  - The repo prefix was deleted or moved underneath an in-flight push",
            remediation: "\
  Verify that the object store is reachable and that the repository\n\
  prefix still exists. Re-run the operation; lazy open will retry. If\n\
  the error persists, inspect the metadb path reported in the error.",
        }),
        "CRAB-E0501" => Some(ErrorExplanation {
            code: "CRAB-E0501",
            summary: "MetaDB close failed",
            causes: "\
  - Final WAL flush failed because the object store rejected the write\n\
  - Credentials expired between the last successful write and close",
            remediation: "\
  Re-run the operation. Any partially flushed state is safe — SlateDB's\n\
  manifest-commit model never exposes partial writes to readers.",
        }),
        "CRAB-E0502" => Some(ErrorExplanation {
            code: "CRAB-E0502",
            summary: "MetaDB read failed",
            causes: "\
  - Object store returned an error while fetching an SSTable or WAL segment\n\
  - Transient network failure against the metadb path",
            remediation: "\
  The operation will retry once against transient errors; if the error\n\
  surfaces, check network connectivity and object-store health.",
        }),
        "CRAB-E0503" => Some(ErrorExplanation {
            code: "CRAB-E0503",
            summary: "MetaDB write failed",
            causes: "\
  - Object store rejected a WAL segment or SSTable upload\n\
  - Writer lease lost or conflicting concurrent writer detected",
            remediation: "\
  Retry the operation. If the lease conflict persists, verify no other\n\
  crab process is writing to the same repo and that prior writers\n\
  exited cleanly.",
        }),
        // CRAB-E0504 reserved (formerly NotBootstrapped). Intentionally
        // absent from the lookup table: nothing emits this code anymore
        // because the SlateDB-backed metadata layer has no bootstrap step.
        "CRAB-E0505" => Some(ErrorExplanation {
            code: "CRAB-E0505",
            summary: "MetaDB format version unsupported",
            causes: "\
  - A newer crab wrote `sys:format_version` with a version this client\n\
    does not understand\n\
  - The repo has been upgraded but this workstation's binary has not",
            remediation: "\
  Upgrade crab to a version that supports the reported format\n\
  version. Check `crab version` against the newest release.",
        }),
        "CRAB-E0506" => Some(ErrorExplanation {
            code: "CRAB-E0506",
            summary: "File hash not found in file_index_db",
            causes: "\
  - Hydration encountered a pointer whose `file_hash` has no shard\n\
    mapping in `file_index_db`\n\
  - The pointing commit was pushed before the shard containing the\n\
    file was uploaded successfully (partial push, or corruption)",
            remediation: "\
  Run `crab metadb rebuild --db file_index` to re-read every shard\n\
  under `.crab/shards/` and repopulate the file-index entries. If\n\
  the file was added in a recent push, verify that push finished\n\
  successfully and try re-pushing.",
        }),
        "CRAB-E0507" => Some(ErrorExplanation {
            code: "CRAB-E0507",
            summary: "MetaDB corrupt value",
            causes: "\
  - A serialized `XorbRef` or other value failed to parse on read\n\
  - Byte-level corruption in an SSTable or WAL segment that survived\n\
    checksum verification (rare)",
            remediation: "\
  Run `crab fsck` against the affected metadb. For isolated value\n\
  corruption, `crab metadb rebuild --db {file_index|chunk_index}`\n\
  rebuilds the affected database by re-reading every shard under\n\
  `.crab/shards/` (content-addressed, idempotent).",
        }),
        "CRAB-E0508" => Some(ErrorExplanation {
            code: "CRAB-E0508",
            summary: "MetaDB already closed",
            causes: "\
  - `MetaDbGuard::close` was called twice on the same guard\n\
  - Internal bug in the caller's shutdown sequence",
            remediation: "\
  This is a programming error. File a bug report with the stack trace.",
        }),
        "CRAB-E0509" => Some(ErrorExplanation {
            code: "CRAB-E0509",
            summary: "MetaDB handle is read-only",
            causes: "\
  - A write-path operation (commit, bump_gc_generation, …) was issued\n\
    against a MetaDb session opened in read-only mode\n\
  - Read-only mode is used by hydrate, clone, diff, fsck, and the\n\
    `metadb diagnose` / `doctor --metadb` surfaces so they don't\n\
    fence a concurrent `crab push`",
            remediation: "\
  This is a programming error. If the operation needs to write\n\
  metadata, construct the MetaDb session with\n\
  `MetaDbConfig { read_only: false, .. }` (the default).",
        }),
        "CRAB-E050A" => Some(ErrorExplanation {
            code: "CRAB-E050A",
            summary: "MetaDB read-only open against uninitialized database",
            causes: "\
  - The target SlateDB has never been written to (no manifest on\n\
    object storage yet)\n\
  - Common on a fresh clone before the first push to a given\n\
    bucket has landed",
            remediation: "\
  The read path treats this as \"all lookups miss\" and continues\n\
  normally. No user action is needed. If you expected data to be\n\
  present, confirm that a `crab push` has succeeded for this\n\
  bucket + repo combination.",
        }),
        "CRAB-E050B" => Some(ErrorExplanation {
            code: "CRAB-E050B",
            summary: "MetaDB operation and close both failed",
            causes: "\
  - A SlateDB read or write failed and the required cleanup close also failed\n\
  - Object-store availability or database fencing may have affected both operations",
            remediation: "\
  Retry the command after checking object-store connectivity. Run `crab fsck`\n\
  or the relevant MetaDB repair command before relying on derived indexes.",
        }),
        "CRAB-E0240" => Some(ErrorExplanation {
            code: "CRAB-E0240",
            summary: "Remote cache entry corrupt",
            causes: "\
  - A materialized output file's blake3 hash does not match the hash\n\
    recorded in the remote cache manifest\n\
  - The xorb data was corrupted in transit or at rest\n\
  - A compromised storage backend served tampered content",
            remediation: "\
  The corrupted entry has been rejected and partial files cleaned up.\n\
  The stage will fall through to local execution. Investigate the\n\
  storage backend for data integrity issues. If this recurs, consider\n\
  rotating credentials and auditing bucket access logs.",
        }),
        "CRAB-E0241" => Some(ErrorExplanation {
            code: "CRAB-E0241",
            summary: "Remote cache entry hash mismatch",
            causes: "\
  - The manifest's stage_hash field does not match the locally-computed\n\
    stage hash\n\
  - The manifest was tampered with or placed at the wrong path\n\
  - A storage backend bug served the wrong manifest for the requested key",
            remediation: "\
  The mismatched entry has been rejected. The stage will fall through\n\
  to local execution. Investigate the storage backend for integrity\n\
  issues. If this recurs, audit bucket access logs for unauthorized\n\
  writes.",
        }),
        "CRAB-E0242" => Some(ErrorExplanation {
            code: "CRAB-E0242",
            summary: "Remote cache is read-only",
            causes: "\
  - `[workflow] remote_cache_readonly = true` is set in the config\n\
  - `--cache-push` was invoked but this environment is configured as\n\
    a cache consumer, not a builder",
            remediation: "\
  Remove `remote_cache_readonly = true` from your config if this\n\
  machine should be allowed to push cache entries. This setting is\n\
  intended for CI consumers that only pull from the shared cache\n\
  while designated builder machines push.",
        }),
        "CRAB-E0243" => Some(ErrorExplanation {
            code: "CRAB-E0243",
            summary: "Workflow validation error: invalid field value",
            causes: "\
  - A field in `crab.yaml` has an invalid value (e.g. timeout: \"banana\",\n\
    retry.max_attempts: 0, negative backoff multiplier)",
            remediation: "\
  Check the field named in the error message and provide a valid value.\n\
  Run `crab run --validate` to see all validation errors at once.",
        }),
        "CRAB-E0244" => Some(ErrorExplanation {
            code: "CRAB-E0244",
            summary: "Workflow self-loop: a stage dep is also one of its own outs",
            causes: "\
  - A stage declares a path in both `deps:` and `outs:`, creating a\n\
    circular dependency on itself",
            remediation: "\
  Remove the path from either `deps:` or `outs:` of the stage. A stage\n\
  cannot depend on its own output.",
        }),
        "CRAB-E0239" => Some(ErrorExplanation {
            code: "CRAB-E0239",
            summary: "Stage on_cache_hit hook failed",
            causes: "\
  - The `on_cache_hit` hook command exited with a non-zero status\n\
  - The hook script encountered an error (missing dependency, bad path,\n\
    permission denied)",
            remediation: "\
  Check the hook command in the stage's `on_cache_hit` field. Verify\n\
  the command runs successfully in isolation. The stage has been marked\n\
  as Failed but the cache entry remains valid for future runs.",
        }),
        "CRAB-E0245" => Some(ErrorExplanation {
            code: "CRAB-E0245",
            summary: "Journal disk full during write",
            causes: "\
  - The filesystem hosting the workflow journal (SQLite) ran out of space\n\
  - SQLITE_FULL was returned during a journal write operation",
            remediation: "\
  Free disk space on the partition containing the `.crab/` directory.\n\
  Previously committed stages remain intact. Run `crab run` again\n\
  after freeing space to resume from the last committed state.",
        }),
        "CRAB-E0248" => Some(ErrorExplanation {
            code: "CRAB-E0248",
            summary: "Matrix expansion has an empty value list for a variable",
            causes: "\
  - A `matrix:` stage declares a variable with an empty list `[]`\n\
  - The Cartesian product cannot be computed when any dimension is empty",
            remediation: "\
  Add at least one value to the empty variable list in the `matrix:` block.\n\
  If the variable is intentionally unused, remove it from the matrix.",
        }),
        "CRAB-E0322" => Some(ErrorExplanation {
            code: "CRAB-E0322",
            summary: "Garbage collection completed only part of its cleanup",
            causes: "\
  - One or more object-store DELETE requests failed\n\
  - Post-delete metadata reconciliation failed",
            remediation: "\
  Review the structured failure counts and source error, resolve the storage\n\
  failure, then rerun `crab gc`. Successful deletions are idempotent.",
        }),
        _ => None,
    }
}

/// Extract the `CRAB-E####` code from a `CrabError`'s Display output.
#[must_use]
pub fn error_code(err: &CrabError) -> &'static str {
    match err {
        CrabError::NetworkTransient(_) => "CRAB-E0001",
        CrabError::Throttled { .. } => "CRAB-E0002",
        CrabError::CasConflict { .. } => "CRAB-E0010",
        CrabError::RefAlreadyExists { .. } => "CRAB-E0011",
        CrabError::PushLockHeld { .. } => "CRAB-E0012",
        CrabError::NonFastForward { .. } => "CRAB-E0017",
        CrabError::CorruptObject { .. } => "CRAB-E0020",
        CrabError::ChunkNotFound { .. } => "CRAB-E0021",
        CrabError::NotFound { .. } => "CRAB-E0030",
        CrabError::Forbidden { .. } => "CRAB-E0031",
        CrabError::NoCredentials => "CRAB-E0040",
        CrabError::InsufficientSpace { .. } => "CRAB-E0041",
        CrabError::AuthFailed { .. } => "CRAB-E0042",
        CrabError::AuthExpired { .. } => "CRAB-E0043",
        CrabError::Configuration { .. } => "CRAB-E0050",
        CrabError::IncompatibleFormat { .. } => "CRAB-E0051",
        CrabError::InvalidPattern(_) => "CRAB-E0052",
        CrabError::Protocol(_) => "CRAB-E0060",
        CrabError::Io(_) => "CRAB-E0070",
        CrabError::Storage(_) => "CRAB-E0071",
        CrabError::StagingCorrupt(_) => "CRAB-E0080",
        CrabError::StagingLocked { .. } => "CRAB-E0081",
        CrabError::HashMismatch { .. } => "CRAB-E0082",
        CrabError::CrcMismatch { .. } => "CRAB-E0083",
        CrabError::PackIntegrity { .. } => "CRAB-E0084",
        CrabError::IncompleteShardReconstruction { .. } => "CRAB-E0085",
        CrabError::PointerMissingStaging { .. } => "CRAB-E0086",
        CrabError::PushConnectivityMissing { .. } => "CRAB-E0087",
        CrabError::PushPartialOutcome { .. } => "CRAB-E0088",
        CrabError::FileChangedDuringStaging { .. } => "CRAB-E0089",
        CrabError::Cancelled => "CRAB-E0090",
        CrabError::BeyondShallowBoundary { .. } => "CRAB-E0091",
        CrabError::PackTooLarge { .. } => "CRAB-E0092",
        CrabError::PushMalformedObject { .. } => "CRAB-E0093",
        CrabError::FetchNotAllowed { .. } => "CRAB-E0094",
        CrabError::FetchTooLarge { .. } => "CRAB-E0095",
        CrabError::FetchMalformedObject { .. } => "CRAB-E0096",
        CrabError::PushIntegrationFailed { .. } => "CRAB-E0097",
        CrabError::Internal(_) => "CRAB-E0099",
        CrabError::InvalidLfsPointer { .. } => "CRAB-E0100",
        CrabError::LfsObjectCorrupt { .. } => "CRAB-E0101",
        CrabError::LfsObjectMissing { .. } => "CRAB-E0102",
        CrabError::LfsLockConflict { .. } => "CRAB-E0103",
        CrabError::LfsTransferProtocol(_) => "CRAB-E0104",
        CrabError::LfsMigrationFailed { .. } => "CRAB-E0105",
        CrabError::LfsUnsupported { .. } => "CRAB-E0106",
        CrabError::CacheService { .. } => "CRAB-E0110",
        CrabError::ManagedRepository { .. } => "CRAB-E0140",
        CrabError::ImportPlanMismatch { .. } => "CRAB-E0114",
        CrabError::ImportNoJournal { .. } => "CRAB-E0115",
        CrabError::ImportSourceMustBeRaw { .. } => "CRAB-E0118",
        CrabError::ImportSchemeMismatch { .. } => "CRAB-E0119",
        CrabError::ImportVersioningUnavailable { .. } => "CRAB-E0120",
        CrabError::ImportCommitCeilingExceeded { .. } => "CRAB-E0121",
        CrabError::ImportInvalidHistoryRange { .. } => "CRAB-E0122",
        CrabError::ImportTargetNotEmpty { .. } => "CRAB-E0123",
        CrabError::PullConflict { .. } => "CRAB-E0130",
        CrabError::PullRemoteUnreachable { .. } => "CRAB-E0131",
        CrabError::UnadoptChunksMissing { .. } => "CRAB-E0132",
        CrabError::NothingToUndo => "CRAB-E0133",
        CrabError::ImportMissingGitIdentity => "CRAB-E0116",
        CrabError::ImportRemoteExists { .. } => "CRAB-E0111",
        CrabError::ImportSourceIsCrabRepo { .. } => "CRAB-E0112",
        CrabError::ImportLfsSourceUnsupported { .. } => "CRAB-E0113",
        CrabError::ImportLfsStoreNotFound { .. } => "CRAB-E0114",
        CrabError::ImportPrefixCollision { .. } => "CRAB-E0117",
        CrabError::WorkflowParse { .. } => "CRAB-E0200",
        CrabError::WorkflowCycle { .. } => "CRAB-E0201",
        CrabError::WorkflowUndefinedOut { .. } => "CRAB-E0202",
        CrabError::WorkflowStageNameInvalid { .. } => "CRAB-E0203",
        CrabError::WorkflowDiscoveryAmbiguous { .. } => "CRAB-E0204",
        CrabError::StageDepMissing { .. } => "CRAB-E0205",
        CrabError::StageDepMalformed { .. } => "CRAB-E0206",
        CrabError::StageOutMalformed { .. } => "CRAB-E0207",
        CrabError::StageOutTooLarge { .. } => "CRAB-E0208",
        CrabError::StageOutCountExceeded { .. } => "CRAB-E0209",
        CrabError::StageEnvMissing { .. } => "CRAB-E0210",
        CrabError::StageExecFailed { .. } => "CRAB-E0211",
        CrabError::StageExecSignaled { .. } => "CRAB-E0212",
        CrabError::StageExecTimeout { .. } => "CRAB-E0213",
        CrabError::StageDiskFull { .. } => "CRAB-E0214",
        CrabError::StageCacheMiss { .. } => "CRAB-E0215",
        CrabError::StageRetryExhausted { .. } => "CRAB-E0216",
        CrabError::StageOverwriteConflict { .. } => "CRAB-E0217",
        CrabError::StageSideEffectsRetryLimit { .. } => "CRAB-E0218",
        CrabError::StageSideEffectHookFailed { .. } => "CRAB-E0239",
        CrabError::LockfileStale { .. } => "CRAB-E0219",
        CrabError::LockfileCanonicalizationFailed { .. } => "CRAB-E0220",
        CrabError::LockfileMergeConflict { .. } => "CRAB-E0221",
        CrabError::ExperimentNotFound { .. } => "CRAB-E0222",
        CrabError::ExperimentCollision { .. } => "CRAB-E0223",
        CrabError::MetricsSchemaMismatch { .. } => "CRAB-E0224",
        CrabError::WorkflowJournalOpen { .. } => "CRAB-E0225",
        CrabError::WorkflowJournalCorrupt { .. } => "CRAB-E0226",
        CrabError::WorkflowJournalSchemaNewer { .. } => "CRAB-E0227",
        CrabError::WorkflowResumeFilesystemDrift { .. } => "CRAB-E0228",
        CrabError::WorkflowStateTransitionIllegal { .. } => "CRAB-E0229",
        CrabError::WorkflowLockTimeout { .. } => "CRAB-E0230",
        CrabError::WorkflowDisabled => "CRAB-E0231",
        CrabError::WorkflowHermeticViolation { .. } => "CRAB-E0232",
        CrabError::CacheEntrySchemaNewer { .. } => "CRAB-E0233",
        CrabError::StageRemoteExecutionUnsupported => "CRAB-E0234",
        CrabError::StageHermeticNotImplemented { .. } => "CRAB-E0235",
        CrabError::WorkflowDuplicateOutput { .. } => "CRAB-E0236",
        CrabError::WorkflowExperimentIdInvalid { .. } => "CRAB-E0237",
        CrabError::WorkflowExperimentMetadataSchemaNewer { .. } => "CRAB-E0238",
        CrabError::CacheEntryCorrupt { .. } => "CRAB-E0240",
        CrabError::CacheEntryHashMismatch { .. } => "CRAB-E0241",
        CrabError::RemoteCacheReadonly => "CRAB-E0242",
        CrabError::WorkflowValidationError { .. } => "CRAB-E0243",
        CrabError::WorkflowSelfLoop { .. } => "CRAB-E0244",
        CrabError::JournalDiskFull { .. } => "CRAB-E0245",
        CrabError::WorkflowTemplateUndefined { .. } => "CRAB-E0246",
        CrabError::WorkflowForeachEmpty { .. } => "CRAB-E0247",
        CrabError::WorkflowMatrixEmpty { .. } => "CRAB-E0248",
        CrabError::TierLifecycleConflict { .. } => "CRAB-E0300",
        CrabError::TierApplyUnauthorized { .. } => "CRAB-E0301",
        CrabError::TierProviderUnsupported { .. } => "CRAB-E0302",
        CrabError::ArchiveRestoreRequired { .. } => "CRAB-E0310",
        CrabError::ArchiveRestoreTimeout { .. } => "CRAB-E0311",
        CrabError::RestoreTierUnsupported { .. } => "CRAB-E0312",
        CrabError::GcEarlyDeleteBlocked { .. } => "CRAB-E0320",
        CrabError::ObjectLockedRetention { .. } => "CRAB-E0321",
        CrabError::GcPartialFailure { .. } => "CRAB-E0322",
        CrabError::RestripeProfileOutOfRange { .. } => "CRAB-E0330",
        CrabError::RestripeCorruptSource { .. } => "CRAB-E0331",
        CrabError::RestripeAlreadyInProgress { .. } => "CRAB-E0332",
        CrabError::ConcurrentMaintenance { .. } => "CRAB-E0333",
        CrabError::CostPricingMissing { .. } => "CRAB-E0340",
        CrabError::CostInventoryReportStale { .. } => "CRAB-E0341",
        CrabError::ManifestParse { .. } => "CRAB-E0400",
        CrabError::PrefetchParse { .. } => "CRAB-E0401",
        CrabError::PrefetchProfileNotFound { .. } => "CRAB-E0402",
        CrabError::SpeculationDb { .. } => "CRAB-E0410",
        CrabError::GixRef(_) => "CRAB-E0600",
        CrabError::GixObject(_) => "CRAB-E0601",
        CrabError::GixPack(_) => "CRAB-E0602",
        CrabError::GixTransport(_) => "CRAB-E0603",
        CrabError::GixProtocol(_) => "CRAB-E0604",
        CrabError::GixFilterHandshake(_) => "CRAB-E0605",
        CrabError::GixFilterRequest(_) => "CRAB-E0606",
        CrabError::GixWorktree(_) => "CRAB-E0607",
        CrabError::GixConfig(_) => "CRAB-E0608",
        CrabError::GixCreds(_) => "CRAB-E0609",
        CrabError::GixStatus(_) => "CRAB-E060A",
        CrabError::GixRevwalk(_) => "CRAB-E060B",
        CrabError::GitTag(_) => "CRAB-E060C",
        CrabError::UnsupportedShell { .. } => "CRAB-E0135",
        CrabError::InvalidConfigKey { .. } => "CRAB-E0134",
        CrabError::MetaDb(inner) => match inner {
            MetaDbError::Open { .. } => "CRAB-E0500",
            MetaDbError::Close { .. } => "CRAB-E0501",
            MetaDbError::Read { .. } => "CRAB-E0502",
            MetaDbError::Write { .. } => "CRAB-E0503",
            // CRAB-E0504 reserved (formerly NotBootstrapped).
            MetaDbError::UnsupportedFormat { .. } => "CRAB-E0505",
            MetaDbError::FileNotFoundInFileIndexDb { .. } => "CRAB-E0506",
            MetaDbError::CorruptValue { .. } => "CRAB-E0507",
            MetaDbError::AlreadyClosed => "CRAB-E0508",
            MetaDbError::ReadOnly { .. } => "CRAB-E0509",
            MetaDbError::ReadOnlyUninitialized { .. } => "CRAB-E050A",
            MetaDbError::OperationAndClose { .. } => "CRAB-E050B",
        },
    }
}

/// Render a `CrabError` into a user-friendly message with the error code,
/// description, and a remediation hint.
#[must_use]
pub fn render(err: &CrabError) -> UserMessage {
    let code = error_code(err);
    let mut text = format!("ERROR [{code}]: {err}");

    if let Some(explanation) = lookup(code) {
        text.push_str("\n\n");
        text.push_str("  Hint: ");
        text.push_str(explanation.remediation.trim());
    }

    UserMessage { text }
}

/// Print the full explanation for a single error code.
///
/// Returns `true` if the code was found, `false` otherwise.
pub fn print_explanation(code: &str) -> bool {
    if let Some(exp) = lookup(code) {
        println!("{}: {}\n", exp.code, exp.summary);
        println!("Common causes:");
        println!("{}\n", exp.causes);
        println!("To resolve:");
        println!("{}", exp.remediation);
        true
    } else {
        false
    }
}

/// Print a table of all known error codes.
pub fn print_all_codes() {
    println!("Crab Error Codes\n");
    for code in ALL_CODES {
        if let Some(exp) = lookup(code) {
            println!("  {}: {}", exp.code, exp.summary);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_explanation() {
        for code in ALL_CODES {
            assert!(lookup(code).is_some(), "missing explanation for {code}");
        }
    }

    #[test]
    fn explanations_are_non_empty() {
        for code in ALL_CODES {
            let exp = lookup(code).unwrap();
            assert!(!exp.summary.is_empty(), "{code} has empty summary");
            assert!(!exp.causes.is_empty(), "{code} has empty causes");
            assert!(!exp.remediation.is_empty(), "{code} has empty remediation");
        }
    }

    #[test]
    fn unknown_code_returns_none() {
        assert!(lookup("CRAB-E9999").is_none());
    }

    #[test]
    fn error_code_matches_all_variants() {
        // Verify error_code() returns a code that exists in ALL_CODES
        // for a representative sample of variants.
        let samples: Vec<CrabError> = vec![
            CrabError::Cancelled,
            CrabError::Internal("test".into()),
            CrabError::Protocol("test".into()),
            CrabError::NoCredentials,
            CrabError::StagingLocked { holder_pid: None },
        ];
        for err in &samples {
            let code = error_code(err);
            assert!(
                ALL_CODES.contains(&code),
                "error_code returned {code} which is not in ALL_CODES"
            );
        }
    }

    #[test]
    fn render_includes_code_and_hint() {
        let err = CrabError::Cancelled;
        let msg = render(&err);
        assert!(
            msg.text.contains("CRAB-E0090"),
            "missing code in render output"
        );
        assert!(msg.text.contains("Hint:"), "missing hint in render output");
    }
}
