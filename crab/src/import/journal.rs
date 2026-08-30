//! Resume journal for `crab import`.
//!
//! Backed by a SQLite database at
//! `{into}/.crab/import-journal.db` (WAL mode). The journal owns
//! two tables:
//!
//! - `plan` — a single-row record of the import plan (source/target
//!   URLs, filter globs, versioning mode, time window, history
//!   bounds, and a blake3 checksum over the canonicalized inputs).
//!   Task 18 (resume) matches this checksum against the provided
//!   CLI arguments to reject drifted re-runs.
//! - `entries` — one row per `(relative_path, version_id)` across
//!   the imported object set. Flat-mode rows use `version_id = ''`;
//!   versioned rows carry their cloud-assigned id. Rows move
//!   through the `Pending → Staged | Failed | Skipped` state
//!   machine as the ingest workers claim and process them.
//! - `lfs_resolutions` — one row per staged LFS pointer that was
//!   resolved into Crab-native content. Resume uses this to reject
//!   stale staged rows when the source pointer changed between runs.
//!
//! # Canonical plan form
//!
//! The plan checksum inputs are, in order:
//!
//! 1. `source_url`, `target_url` (as-is, user-provided).
//! 2. `source_prefix`, `target_prefix`, `dest_prefix` (empty if absent).
//! 3. `source_mode` tag (`Flat`, `Versioned`, `SingleSnapshot`) as
//!    a stable integer (0, 1, 2) — same encoding the SQL column uses.
//! 4. `window_secs` (-1 if absent).
//! 5. `snapshot_at` epoch seconds (-1 if absent).
//! 6. `since_epoch`, `until_epoch` (-1 each if absent).
//! 7. `branch`.
//! 8. `include`, `exclude`, `track` globs, each **sorted**
//!    lexicographically so command-line ordering does not affect
//!    the checksum.
//! 9. `lfs_source` and `lfs_objects`, after CLI alias resolution.
//!
//! Each field is length-prefixed (u32 little-endian) so no
//! delimiter collision exists between values. The blake3 output is
//! stored as a 32-byte blob in `plan.plan_checksum`.
//!
//! Keep the canonical form here in one place — Task 18's resume
//! path reuses `PlanInputs::checksum` to compare the stored plan
//! against a fresh invocation.
//!
//! # Concurrency
//!
//! `Journal` owns a `rusqlite::Connection` directly. rusqlite is
//! synchronous; async callers wrap the journal in a
//! `tokio::sync::Mutex` and invoke methods from `spawn_blocking`
//! (or run on the current thread for ingest coordinators where
//! the SQLite work is short). This mirrors the pattern in
//! `crab_staging::index`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Canonical on-disk schema identifier.
pub const IMPORT_JOURNAL_SCHEMA_VERSION: &str = "1";
const SCHEMA_DESCRIPTOR: &str = "plan-v1;entries-v1;lfs-resolutions-v1;journal-meta-v1";

/// Integer encoding of [`SourceModeTag`] for the `plan.source_mode`
/// column. Kept stable across releases; new modes append.
#[repr(i64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceModeTag {
    Flat = 0,
    Versioned = 1,
    SingleSnapshot = 2,
}

impl SourceModeTag {
    fn from_i64(raw: i64) -> Result<Self> {
        match raw {
            0 => Ok(Self::Flat),
            1 => Ok(Self::Versioned),
            2 => Ok(Self::SingleSnapshot),
            other => Err(CrabError::Internal(format!(
                "unknown source_mode tag {other} in import journal"
            ))),
        }
    }

    fn as_i64(self) -> i64 {
        self as i64
    }
}

/// Why an entry was skipped (serialized to `entries.skip_reason`
/// as a short stable slug).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Source object is a Git LFS pointer — out of scope for V1.
    LfsPointer,
    /// Path contains characters git rejects in tree entries.
    InvalidGitPath,
    /// Zero-byte `.../` placeholder from the object-store UX.
    ZeroByteDirectoryKey,
    /// Version's `last_modified` is outside `--since`/`--until`.
    OutsideHistoryWindow,
}

impl SkipReason {
    fn as_slug(&self) -> &'static str {
        match self {
            Self::LfsPointer => "lfs-pointer",
            Self::InvalidGitPath => "invalid-git-path",
            Self::ZeroByteDirectoryKey => "zero-byte-directory-key",
            Self::OutsideHistoryWindow => "outside-history-window",
        }
    }

    fn from_slug(raw: &str) -> Result<Self> {
        match raw {
            "lfs-pointer" => Ok(Self::LfsPointer),
            "invalid-git-path" => Ok(Self::InvalidGitPath),
            "zero-byte-directory-key" => Ok(Self::ZeroByteDirectoryKey),
            "outside-history-window" => Ok(Self::OutsideHistoryWindow),
            other => Err(CrabError::Internal(format!(
                "unknown skip_reason slug {other:?} in import journal"
            ))),
        }
    }
}

/// Lifecycle state of a single entry.
///
/// The `InProgress` variant is owned by `claim_next_pending` — the
/// database marks a Pending row InProgress atomically so workers
/// never pick up the same entry twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    Pending,
    InProgress,
    Staged { file_hash: [u8; 32] },
    Failed { message: String },
    Skipped { reason: SkipReason },
}

impl EntryState {
    const TAG_PENDING: i64 = 0;
    const TAG_STAGED: i64 = 1;
    const TAG_FAILED: i64 = 2;
    const TAG_SKIPPED: i64 = 3;
    const TAG_IN_PROGRESS: i64 = 4;
}

/// Row stored in / loaded from the `entries` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportEntry {
    pub relative_path: String,
    /// Empty string for flat buckets; cloud-assigned id otherwise.
    pub version_id: String,
    pub size: u64,
    pub etag: Option<String>,
    /// Epoch seconds (UTC). Stored as INTEGER.
    pub last_modified: i64,
    pub is_delete_marker: bool,
    pub state: EntryState,
}

/// LFS pointer identity recorded for a staged resolved entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsResolution {
    pub oid: [u8; 32],
    pub size: u64,
}

/// Staged entry plus the LFS pointer identity that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedLfsResolution {
    pub relative_path: String,
    pub version_id: String,
    pub file_hash: [u8; 32],
    pub resolution: LfsResolution,
}

/// Canonical inputs that define an import plan's identity.
///
/// Callers fill this in once before hashing. The canonicalization
/// (sorting, length-prefix framing) lives in
/// [`PlanInputs::checksum`] so every caller produces a byte-for-
/// byte identical checksum for equivalent runs. Task 18's resume
/// path reuses this function to compare against the stored plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInputs {
    pub source_url: String,
    pub target_url: String,
    pub source_prefix: String,
    pub target_prefix: String,
    pub dest_prefix: String,
    pub source_mode: SourceModeTag,
    /// Window width in seconds; `None` for flat / single-snapshot.
    pub window_secs: Option<u64>,
    /// Epoch seconds; `None` unless SingleSnapshot mode.
    pub snapshot_at: Option<i64>,
    pub since_epoch: Option<i64>,
    pub until_epoch: Option<i64>,
    pub branch: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub track: Vec<String>,
    pub lfs_source: String,
    pub lfs_objects: String,
}

impl PlanInputs {
    /// Compute the canonical blake3 checksum of the plan.
    ///
    /// See the module-level doc for the canonical form. The
    /// returned bytes are what lives in `plan.plan_checksum` and
    /// what [`Journal::verify_plan`] compares against.
    #[must_use]
    pub fn checksum(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        write_str(&mut hasher, &self.source_url);
        write_str(&mut hasher, &self.target_url);
        write_str(&mut hasher, &self.source_prefix);
        write_str(&mut hasher, &self.target_prefix);
        write_str(&mut hasher, &self.dest_prefix);
        write_i64(&mut hasher, self.source_mode.as_i64());
        write_opt_i64(
            &mut hasher,
            self.window_secs
                .map(i64::try_from)
                .transpose()
                .unwrap_or(None),
        );
        write_opt_i64(&mut hasher, self.snapshot_at);
        write_opt_i64(&mut hasher, self.since_epoch);
        write_opt_i64(&mut hasher, self.until_epoch);
        write_str(&mut hasher, &self.branch);
        write_sorted_list(&mut hasher, &self.include);
        write_sorted_list(&mut hasher, &self.exclude);
        write_sorted_list(&mut hasher, &self.track);
        write_str(&mut hasher, &self.lfs_source);
        write_str(&mut hasher, &self.lfs_objects);
        *hasher.finalize().as_bytes()
    }
}

fn write_str(hasher: &mut blake3::Hasher, s: &str) {
    let bytes = s.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    hasher.update(&len.to_le_bytes());
    hasher.update(bytes);
}

fn write_i64(hasher: &mut blake3::Hasher, v: i64) {
    hasher.update(&v.to_le_bytes());
}

fn write_opt_i64(hasher: &mut blake3::Hasher, v: Option<i64>) {
    // Sentinel: present-flag byte + raw value. Absent values still
    // contribute an 8-byte zero payload so field boundaries stay
    // stable regardless of presence.
    let present: u8 = u8::from(v.is_some());
    hasher.update(&[present]);
    hasher.update(&v.unwrap_or(0).to_le_bytes());
}

fn write_sorted_list(hasher: &mut blake3::Hasher, list: &[String]) {
    let mut sorted: Vec<&str> = list.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let len = u32::try_from(sorted.len()).unwrap_or(u32::MAX);
    hasher.update(&len.to_le_bytes());
    for s in sorted {
        write_str(hasher, s);
    }
}

/// Plan row as persisted in the `plan` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub inputs: PlanInputs,
    pub plan_checksum: [u8; 32],
    /// Epoch seconds the plan was first recorded.
    pub created_at: i64,
}

/// Resume journal handle.
///
/// Wraps a synchronous `rusqlite::Connection`. Async callers wrap
/// this in a `tokio::sync::Mutex` and dispatch methods through
/// `spawn_blocking` for long-running work.
pub struct Journal {
    conn: Connection,
    path: PathBuf,
}

impl Journal {
    /// Open (or create) the journal at `{into}/.crab/import-journal.db`.
    ///
    /// Creates the `.crab/` parent directory as needed, enables
    /// WAL mode, and initializes one canonical v1 schema on first open.
    pub fn open(into: &Path) -> Result<Self> {
        let dir = into.join(".crab");
        fs::create_dir_all(&dir).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create journal dir {}: {e}",
                dir.display()
            ))
        })?;
        let path = dir.join("import-journal.db");
        let existed = path.exists();

        if existed {
            let readonly = Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| {
                CrabError::Internal(format!(
                    "failed to inspect import journal at {}: {e}",
                    path.display()
                ))
            })?;
            validate_schema(&readonly, &path)?;
        }

        let conn = Connection::open(&path).map_err(|e| {
            CrabError::Internal(format!(
                "failed to open import journal at {}: {e}",
                path.display()
            ))
        })?;

        // WAL for concurrent readers + a single writer.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| CrabError::Internal(format!("set journal_mode=WAL: {e}")))?;
        // NORMAL is durable for commits under WAL; only a full OS
        // crash risks the trailing transaction.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| CrabError::Internal(format!("set synchronous=NORMAL: {e}")))?;
        conn.pragma_update(None, "busy_timeout", "5000")
            .map_err(|e| CrabError::Internal(format!("set busy_timeout: {e}")))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| CrabError::Internal(format!("enable foreign_keys: {e}")))?;

        let journal = Self { conn, path };
        if existed {
            validate_schema(&journal.conn, &journal.path)?;
        } else {
            journal.initialize_schema()?;
        }
        Ok(journal)
    }

    fn initialize_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE plan (
                    plan_id        INTEGER PRIMARY KEY CHECK (plan_id = 1),
                    source_url     TEXT NOT NULL,
                    target_url     TEXT NOT NULL,
                    source_prefix  TEXT NOT NULL,
                    target_prefix  TEXT NOT NULL,
                    dest_prefix    TEXT NOT NULL DEFAULT '',
                    source_mode    INTEGER NOT NULL,
                    window_secs    INTEGER,
                    snapshot_at    INTEGER,
                    since_epoch    INTEGER,
                    until_epoch    INTEGER,
                    branch         TEXT NOT NULL,
                    include_json   TEXT NOT NULL,
                    exclude_json   TEXT NOT NULL,
                    track_json     TEXT NOT NULL,
                    lfs_source     TEXT NOT NULL DEFAULT 'fail',
                    lfs_objects    TEXT NOT NULL DEFAULT '',
                    plan_checksum  BLOB NOT NULL,
                    created_at     INTEGER NOT NULL
                );

                CREATE TABLE entries (
                    relative_path    TEXT NOT NULL,
                    version_id       TEXT NOT NULL DEFAULT '',
                    size             INTEGER NOT NULL,
                    etag             TEXT,
                    last_modified    INTEGER NOT NULL,
                    is_delete_marker INTEGER NOT NULL DEFAULT 0,
                    state            INTEGER NOT NULL,
                    error            TEXT,
                    skip_reason      TEXT,
                    file_hash        BLOB,
                    updated_at       INTEGER NOT NULL,
                    PRIMARY KEY (relative_path, version_id)
                );

                CREATE INDEX idx_entries_state_size
                    ON entries(state, size DESC);

                CREATE INDEX idx_entries_time
                    ON entries(last_modified, relative_path, version_id);

                CREATE TABLE lfs_resolutions (
                    relative_path TEXT NOT NULL,
                    version_id    TEXT NOT NULL DEFAULT '',
                    oid           BLOB NOT NULL,
                    size          INTEGER NOT NULL,
                    file_hash     BLOB NOT NULL,
                    updated_at    INTEGER NOT NULL,
                    PRIMARY KEY (relative_path, version_id),
                    FOREIGN KEY (relative_path, version_id)
                        REFERENCES entries(relative_path, version_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE journal_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO journal_meta (key, value) VALUES
                    ('schema_version', '1'),
                    ('schema_descriptor', 'plan-v1;entries-v1;lfs-resolutions-v1;journal-meta-v1');",
            )
            .map_err(|e| CrabError::Internal(format!("initialize import journal schema: {e}")))
    }
}

fn validate_schema(conn: &Connection, path: &Path) -> Result<()> {
    let metadata = |key: &str| -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM journal_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| CrabError::Internal(format!("read import journal metadata: {e}")))
    };
    let version = metadata("schema_version")?;
    let descriptor = metadata("schema_descriptor")?;
    let mut statement = conn
        .prepare(
            "SELECT type, name FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(|e| CrabError::Internal(format!("inspect import journal schema: {e}")))?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| CrabError::Internal(format!("inspect import journal schema: {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| CrabError::Internal(format!("inspect import journal schema: {e}")))?;
    let expected = vec![
        ("index".to_owned(), "idx_entries_state_size".to_owned()),
        ("index".to_owned(), "idx_entries_time".to_owned()),
        ("table".to_owned(), "entries".to_owned()),
        ("table".to_owned(), "journal_meta".to_owned()),
        ("table".to_owned(), "lfs_resolutions".to_owned()),
        ("table".to_owned(), "plan".to_owned()),
    ];
    if version.as_deref() != Some(IMPORT_JOURNAL_SCHEMA_VERSION)
        || descriptor.as_deref() != Some(SCHEMA_DESCRIPTOR)
        || objects != expected
    {
        return Err(CrabError::Internal(format!(
            "import journal {} is not canonical v1; remove that journal and restart import",
            path.display()
        )));
    }
    Ok(())
}

impl Journal {
    // ── Plan ────────────────────────────────────────────────────

    /// Insert or replace the single plan row.
    ///
    /// Computes `plan_checksum` from `inputs` and stores the
    /// canonical glob lists as JSON arrays so they survive a
    /// round-trip without ambiguity.
    pub fn record_plan(&self, inputs: &PlanInputs, created_at: i64) -> Result<Plan> {
        let checksum = inputs.checksum();
        let include_json = serialize_list(&inputs.include);
        let exclude_json = serialize_list(&inputs.exclude);
        let track_json = serialize_list(&inputs.track);

        self.conn
            .execute(
                "INSERT OR REPLACE INTO plan (
                    plan_id, source_url, target_url,
                    source_prefix, target_prefix, dest_prefix,
                    source_mode, window_secs, snapshot_at,
                    since_epoch, until_epoch,
                    branch, include_json, exclude_json, track_json,
                    lfs_source, lfs_objects, plan_checksum, created_at
                 ) VALUES (
                    1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
                 )",
                params![
                    inputs.source_url,
                    inputs.target_url,
                    inputs.source_prefix,
                    inputs.target_prefix,
                    inputs.dest_prefix,
                    inputs.source_mode.as_i64(),
                    inputs
                        .window_secs
                        .map(|s| i64::try_from(s).unwrap_or(i64::MAX)),
                    inputs.snapshot_at,
                    inputs.since_epoch,
                    inputs.until_epoch,
                    inputs.branch,
                    include_json,
                    exclude_json,
                    track_json,
                    inputs.lfs_source,
                    inputs.lfs_objects,
                    &checksum[..],
                    created_at,
                ],
            )
            .map_err(|e| CrabError::Internal(format!("record_plan insert: {e}")))?;

        Ok(Plan {
            inputs: inputs.clone(),
            plan_checksum: checksum,
            created_at,
        })
    }

    /// Load the plan row. Returns `None` if the journal has not
    /// recorded a plan yet.
    pub fn load_plan(&self) -> Result<Option<Plan>> {
        let row = self
            .conn
            .query_row(
                "SELECT source_url, target_url, source_prefix, target_prefix, dest_prefix,
                        source_mode, window_secs, snapshot_at,
                        since_epoch, until_epoch, branch,
                        include_json, exclude_json, track_json,
                        lfs_source, lfs_objects,
                        plan_checksum, created_at
                 FROM plan WHERE plan_id = 1",
                [],
                |row| {
                    let source_url: String = row.get(0)?;
                    let target_url: String = row.get(1)?;
                    let source_prefix: String = row.get(2)?;
                    let target_prefix: String = row.get(3)?;
                    let dest_prefix: String = row.get(4)?;
                    let source_mode_raw: i64 = row.get(5)?;
                    let window_secs: Option<i64> = row.get(6)?;
                    let snapshot_at: Option<i64> = row.get(7)?;
                    let since_epoch: Option<i64> = row.get(8)?;
                    let until_epoch: Option<i64> = row.get(9)?;
                    let branch: String = row.get(10)?;
                    let include_json: String = row.get(11)?;
                    let exclude_json: String = row.get(12)?;
                    let track_json: String = row.get(13)?;
                    let lfs_source: String = row.get(14)?;
                    let lfs_objects: String = row.get(15)?;
                    let checksum_blob: Vec<u8> = row.get(16)?;
                    let created_at: i64 = row.get(17)?;
                    Ok((
                        source_url,
                        target_url,
                        source_prefix,
                        target_prefix,
                        dest_prefix,
                        source_mode_raw,
                        window_secs,
                        snapshot_at,
                        since_epoch,
                        until_epoch,
                        branch,
                        include_json,
                        exclude_json,
                        track_json,
                        lfs_source,
                        lfs_objects,
                        checksum_blob,
                        created_at,
                    ))
                },
            )
            .optional()
            .map_err(|e| CrabError::Internal(format!("load_plan query: {e}")))?;

        let Some((
            source_url,
            target_url,
            source_prefix,
            target_prefix,
            dest_prefix,
            source_mode_raw,
            window_secs,
            snapshot_at,
            since_epoch,
            until_epoch,
            branch,
            include_json,
            exclude_json,
            track_json,
            lfs_source,
            lfs_objects,
            checksum_blob,
            created_at,
        )) = row
        else {
            return Ok(None);
        };

        let source_mode = SourceModeTag::from_i64(source_mode_raw)?;
        let include = deserialize_list(&include_json)?;
        let exclude = deserialize_list(&exclude_json)?;
        let track = deserialize_list(&track_json)?;

        let mut plan_checksum = [0u8; 32];
        if checksum_blob.len() != 32 {
            return Err(CrabError::Internal(format!(
                "plan_checksum has unexpected length {} (want 32)",
                checksum_blob.len()
            )));
        }
        plan_checksum.copy_from_slice(&checksum_blob);

        let window_secs_opt = window_secs.and_then(|v| u64::try_from(v).ok());

        let inputs = PlanInputs {
            source_url,
            target_url,
            source_prefix,
            target_prefix,
            dest_prefix,
            source_mode,
            window_secs: window_secs_opt,
            snapshot_at,
            since_epoch,
            until_epoch,
            branch,
            include,
            exclude,
            track,
            lfs_source,
            lfs_objects,
        };

        Ok(Some(Plan {
            inputs,
            plan_checksum,
            created_at,
        }))
    }

    /// Strictly verify that `provided` matches the recorded plan's
    /// checksum. Returns [`CrabError::ImportPlanMismatch`] on
    /// drift. Returns `Ok(())` when they agree.
    ///
    /// Callers must have recorded a plan first; if the `plan` row
    /// is absent this function returns an `Internal` error — the
    /// missing-journal case is a separate concern (`ImportNoJournal`)
    /// that Task 18 handles.
    pub fn verify_plan(&self, provided: &PlanInputs) -> Result<()> {
        let plan = self.load_plan()?.ok_or_else(|| {
            CrabError::Internal(
                "verify_plan called before record_plan; no plan row in journal".into(),
            )
        })?;

        let fresh = provided.checksum();
        if fresh == plan.plan_checksum {
            return Ok(());
        }

        Err(CrabError::ImportPlanMismatch {
            recorded: hex_checksum(&plan.plan_checksum),
            provided: hex_checksum(&fresh),
        })
    }
}

fn serialize_list(list: &[String]) -> String {
    // Simple, dependency-free JSON-array encoding. Values may
    // contain arbitrary UTF-8; escape backslashes and quotes.
    let mut out = String::from("[");
    for (i, item) in list.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    use std::fmt::Write;
                    // Non-fatal formatting — fall back to a replacement char if
                    // the write fails (it won't for String).
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

fn deserialize_list(raw: &str) -> Result<Vec<String>> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes[0] != b'[' || bytes[bytes.len() - 1] != b']' {
        return Err(CrabError::Internal(format!(
            "malformed list in journal: {raw:?}"
        )));
    }
    let inner = &raw[1..raw.len() - 1];
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        // Skip optional whitespace and leading comma.
        while let Some(&c) = chars.peek() {
            if c == ' ' || c == ',' {
                chars.next();
            } else {
                break;
            }
        }
        match chars.next() {
            Some('"') => {}
            None => break,
            Some(other) => {
                return Err(CrabError::Internal(format!(
                    "expected '\"' in journal list, found {other:?}"
                )));
            }
        }
        let mut item = String::new();
        loop {
            match chars.next() {
                Some('\\') => match chars.next() {
                    Some('\\') => item.push('\\'),
                    Some('"') => item.push('"'),
                    Some('n') => item.push('\n'),
                    Some('r') => item.push('\r'),
                    Some('t') => item.push('\t'),
                    Some('u') => {
                        let mut hex = String::with_capacity(4);
                        for _ in 0..4 {
                            match chars.next() {
                                Some(c) => hex.push(c),
                                None => {
                                    return Err(CrabError::Internal(
                                        "truncated \\u escape in journal list".into(),
                                    ));
                                }
                            }
                        }
                        let cp = u32::from_str_radix(&hex, 16).map_err(|e| {
                            CrabError::Internal(format!("bad \\u escape {hex:?}: {e}"))
                        })?;
                        if let Some(c) = char::from_u32(cp) {
                            item.push(c);
                        } else {
                            return Err(CrabError::Internal(format!(
                                "invalid codepoint U+{cp:04X} in journal list"
                            )));
                        }
                    }
                    Some(other) => {
                        return Err(CrabError::Internal(format!(
                            "unknown escape \\{other:?} in journal list"
                        )));
                    }
                    None => {
                        return Err(CrabError::Internal(
                            "truncated escape in journal list".into(),
                        ));
                    }
                },
                Some('"') => break,
                Some(c) => item.push(c),
                None => {
                    return Err(CrabError::Internal(
                        "unterminated string in journal list".into(),
                    ));
                }
            }
        }
        out.push(item);
    }
    Ok(out)
}

fn hex_checksum(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

impl Journal {
    // ── Entries: upsert ─────────────────────────────────────────

    /// Insert or replace a batch of entries in a single transaction.
    ///
    /// `(relative_path, version_id)` is the composite primary key;
    /// `INSERT OR REPLACE` makes re-enumerating the source (after
    /// a crash or retry) idempotent. Callers batch at roughly
    /// 1 000 rows per call — SQLite's WAL commit amortizes well at
    /// that size and keeps individual transactions bounded.
    pub fn upsert_entry_batch(&self, entries: &[ImportEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CrabError::Internal(format!("upsert_entry_batch begin: {e}")))?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO entries (
                        relative_path, version_id, size, etag,
                        last_modified, is_delete_marker,
                        state, error, skip_reason, file_hash, updated_at
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11
                     )",
                )
                .map_err(|e| CrabError::Internal(format!("upsert_entry_batch prepare: {e}")))?;

            let now = now_epoch_secs();

            for entry in entries {
                let (state_tag, error, skip_reason, file_hash): (
                    i64,
                    Option<&str>,
                    Option<&str>,
                    Option<&[u8]>,
                ) = match &entry.state {
                    EntryState::Pending => (EntryState::TAG_PENDING, None, None, None),
                    EntryState::InProgress => (EntryState::TAG_IN_PROGRESS, None, None, None),
                    EntryState::Staged { file_hash } => {
                        (EntryState::TAG_STAGED, None, None, Some(&file_hash[..]))
                    }
                    EntryState::Failed { message } => {
                        (EntryState::TAG_FAILED, Some(message.as_str()), None, None)
                    }
                    EntryState::Skipped { reason } => {
                        (EntryState::TAG_SKIPPED, None, Some(reason.as_slug()), None)
                    }
                };

                let size_i64 = i64::try_from(entry.size).map_err(|e| {
                    CrabError::Internal(format!("entry size {} out of i64 range: {e}", entry.size))
                })?;

                stmt.execute(params![
                    entry.relative_path,
                    entry.version_id,
                    size_i64,
                    entry.etag,
                    entry.last_modified,
                    i64::from(entry.is_delete_marker),
                    state_tag,
                    error,
                    skip_reason,
                    file_hash,
                    now,
                ])
                .map_err(|e| CrabError::Internal(format!("upsert_entry_batch insert: {e}")))?;
            }
        }

        tx.commit()
            .map_err(|e| CrabError::Internal(format!("upsert_entry_batch commit: {e}")))?;
        Ok(())
    }

    // ── Entries: claim ──────────────────────────────────────────

    /// Atomically pick the largest `size` Pending row and move it
    /// to `InProgress`, returning the claimed entry. Returns `None`
    /// when no Pending rows remain.
    ///
    /// Uses SQLite's `UPDATE ... RETURNING` (requires SQLite ≥ 3.35,
    /// which the bundled rusqlite feature ships).
    pub fn claim_next_pending(&self) -> Result<Option<ImportEntry>> {
        let now = now_epoch_secs();

        // The inner SELECT narrows to the single row we want to
        // claim (largest size first; tiebreak by path for stable
        // ordering). `RETURNING` hands us the full row atomically.
        let mut stmt = self
            .conn
            .prepare_cached(
                "UPDATE entries
                 SET state = ?1, updated_at = ?2
                 WHERE rowid = (
                    SELECT rowid FROM entries
                    WHERE state = ?3
                    ORDER BY size DESC, relative_path ASC, version_id ASC
                    LIMIT 1
                 )
                 RETURNING relative_path, version_id, size, etag,
                           last_modified, is_delete_marker,
                           state, error, skip_reason, file_hash",
            )
            .map_err(|e| CrabError::Internal(format!("claim_next_pending prepare: {e}")))?;

        let row = stmt
            .query_row(
                params![EntryState::TAG_IN_PROGRESS, now, EntryState::TAG_PENDING],
                row_to_entry,
            )
            .optional()
            .map_err(|e| CrabError::Internal(format!("claim_next_pending query: {e}")))?;

        Ok(row)
    }

    // ── Entries: transitions ────────────────────────────────────

    /// Mark the entry at `(relative_path, version_id)` as staged
    /// with the given `file_hash`. Idempotent; a non-existent
    /// entry is an `Internal` error so tests catch plan drift.
    pub fn mark_staged(
        &self,
        relative_path: &str,
        version_id: &str,
        file_hash: [u8; 32],
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CrabError::Internal(format!("mark_staged begin: {e}")))?;

        let rows = mark_staged_row(&tx, relative_path, version_id, file_hash, now_epoch_secs())?;

        if rows == 0 {
            return Err(CrabError::Internal(format!(
                "mark_staged: no entry at ({relative_path:?}, {version_id:?})"
            )));
        }
        clear_lfs_resolution_row(&tx, relative_path, version_id, "mark_staged clear lfs")?;
        tx.commit()
            .map_err(|e| CrabError::Internal(format!("mark_staged commit: {e}")))?;
        Ok(())
    }

    /// Mark the entry as staged with the LFS pointer identity that
    /// produced its Crab-native staged bytes.
    pub fn mark_staged_lfs(
        &self,
        relative_path: &str,
        version_id: &str,
        file_hash: [u8; 32],
        resolution: &LfsResolution,
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CrabError::Internal(format!("mark_staged_lfs begin: {e}")))?;
        let now = now_epoch_secs();

        let rows = mark_staged_row(&tx, relative_path, version_id, file_hash, now)?;
        if rows == 0 {
            return Err(CrabError::Internal(format!(
                "mark_staged_lfs: no entry at ({relative_path:?}, {version_id:?})"
            )));
        }

        let size_i64 = i64::try_from(resolution.size).map_err(|e| {
            CrabError::Internal(format!(
                "lfs pointer size {} out of i64 range: {e}",
                resolution.size
            ))
        })?;
        tx.execute(
            "UPDATE entries
             SET size = ?1
             WHERE relative_path = ?2 AND version_id = ?3",
            params![size_i64, relative_path, version_id],
        )
        .map_err(|e| CrabError::Internal(format!("mark_staged_lfs size update: {e}")))?;
        tx.execute(
            "INSERT OR REPLACE INTO lfs_resolutions (
                relative_path, version_id, oid, size, file_hash, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6
             )",
            params![
                relative_path,
                version_id,
                &resolution.oid[..],
                size_i64,
                &file_hash[..],
                now,
            ],
        )
        .map_err(|e| CrabError::Internal(format!("mark_staged_lfs insert: {e}")))?;

        tx.commit()
            .map_err(|e| CrabError::Internal(format!("mark_staged_lfs commit: {e}")))?;
        Ok(())
    }

    /// Mark the entry as failed with a human-readable message.
    pub fn mark_failed(&self, relative_path: &str, version_id: &str, message: &str) -> Result<()> {
        let now = now_epoch_secs();
        let rows = self
            .conn
            .execute(
                "UPDATE entries
                 SET state = ?1, error = ?2, file_hash = NULL,
                     skip_reason = NULL, updated_at = ?3
                 WHERE relative_path = ?4 AND version_id = ?5",
                params![
                    EntryState::TAG_FAILED,
                    message,
                    now,
                    relative_path,
                    version_id,
                ],
            )
            .map_err(|e| CrabError::Internal(format!("mark_failed update: {e}")))?;

        if rows == 0 {
            return Err(CrabError::Internal(format!(
                "mark_failed: no entry at ({relative_path:?}, {version_id:?})"
            )));
        }
        self.clear_lfs_resolution(relative_path, version_id, "mark_failed clear lfs")?;
        Ok(())
    }

    /// Mark the entry as skipped with a structured reason slug.
    pub fn mark_skipped(
        &self,
        relative_path: &str,
        version_id: &str,
        reason: SkipReason,
    ) -> Result<()> {
        let now = now_epoch_secs();
        let rows = self
            .conn
            .execute(
                "UPDATE entries
                 SET state = ?1, skip_reason = ?2, error = NULL,
                     file_hash = NULL, updated_at = ?3
                 WHERE relative_path = ?4 AND version_id = ?5",
                params![
                    EntryState::TAG_SKIPPED,
                    reason.as_slug(),
                    now,
                    relative_path,
                    version_id,
                ],
            )
            .map_err(|e| CrabError::Internal(format!("mark_skipped update: {e}")))?;

        if rows == 0 {
            return Err(CrabError::Internal(format!(
                "mark_skipped: no entry at ({relative_path:?}, {version_id:?})"
            )));
        }
        self.clear_lfs_resolution(relative_path, version_id, "mark_skipped clear lfs")?;
        Ok(())
    }

    /// Reset one entry to `Pending` so ingest can rebuild its
    /// local staging rows before the next publish attempt.
    pub fn mark_pending(&self, relative_path: &str, version_id: &str) -> Result<()> {
        let now = now_epoch_secs();
        let rows = self
            .conn
            .execute(
                "UPDATE entries
                 SET state = ?1, error = NULL, file_hash = NULL,
                     skip_reason = NULL, updated_at = ?2
                 WHERE relative_path = ?3 AND version_id = ?4",
                params![EntryState::TAG_PENDING, now, relative_path, version_id],
            )
            .map_err(|e| CrabError::Internal(format!("mark_pending update: {e}")))?;

        if rows == 0 {
            return Err(CrabError::Internal(format!(
                "mark_pending: no entry at ({relative_path:?}, {version_id:?})"
            )));
        }
        self.clear_lfs_resolution(relative_path, version_id, "mark_pending clear lfs")?;
        Ok(())
    }

    fn clear_lfs_resolution(
        &self,
        relative_path: &str,
        version_id: &str,
        context: &str,
    ) -> Result<()> {
        clear_lfs_resolution_row(&self.conn, relative_path, version_id, context)
    }

    // ── Entries: iteration ──────────────────────────────────────

    /// Reset retryable rows back to `Pending` so a subsequent
    /// `claim_next_pending` will retry them. Intended for the
    /// `--resume` path after a previous run either marked entries
    /// `Failed` or crashed after claiming them as `InProgress`.
    ///
    /// Returns the number of rows that were flipped.
    pub fn reset_retryable_to_pending(&self) -> Result<u64> {
        let now = now_epoch_secs();
        let rows = self
            .conn
            .execute(
                "UPDATE entries
                 SET state = ?1, error = NULL, file_hash = NULL,
                     skip_reason = NULL, updated_at = ?2
                 WHERE state IN (?3, ?4)",
                params![
                    EntryState::TAG_PENDING,
                    now,
                    EntryState::TAG_FAILED,
                    EntryState::TAG_IN_PROGRESS,
                ],
            )
            .map_err(|e| CrabError::Internal(format!("reset_retryable_to_pending: {e}")))?;
        Ok(u64::try_from(rows).unwrap_or(0))
    }

    /// Stream all entries in `(last_modified, relative_path,
    /// version_id)` order, invoking `f` on each row. Used by the
    /// window-planning pass. The statement is prepared once and
    /// driven by a SQLite cursor so memory stays bounded.
    ///
    /// Returning `Err` from `f` aborts the iteration.
    pub fn iter_entries_sorted_by_time<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(ImportEntry) -> Result<()>,
    {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT relative_path, version_id, size, etag,
                        last_modified, is_delete_marker,
                        state, error, skip_reason, file_hash
                 FROM entries
                 ORDER BY last_modified ASC, relative_path ASC, version_id ASC",
            )
            .map_err(|e| CrabError::Internal(format!("iter_entries prepare: {e}")))?;

        let mut rows = stmt
            .query([])
            .map_err(|e| CrabError::Internal(format!("iter_entries query: {e}")))?;

        loop {
            let row = rows
                .next()
                .map_err(|e| CrabError::Internal(format!("iter_entries next: {e}")))?;
            let Some(row) = row else { break };

            let entry = row_to_entry(row)
                .map_err(|e| CrabError::Internal(format!("iter_entries decode: {e}")))?;
            f(entry)?;
        }
        Ok(())
    }

    /// Stream staged entries that were produced by resolving an
    /// LFS pointer, in the same deterministic order as entries.
    pub fn iter_staged_lfs_resolutions<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(StagedLfsResolution) -> Result<()>,
    {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT e.relative_path, e.version_id, e.file_hash,
                        r.oid, r.size, r.file_hash
                 FROM lfs_resolutions r
                 JOIN entries e
                   ON e.relative_path = r.relative_path
                  AND e.version_id = r.version_id
                 WHERE e.state = ?1
                 ORDER BY e.last_modified ASC, e.relative_path ASC, e.version_id ASC",
            )
            .map_err(|e| {
                CrabError::Internal(format!("iter_staged_lfs_resolutions prepare: {e}"))
            })?;

        let mut rows = stmt
            .query(params![EntryState::TAG_STAGED])
            .map_err(|e| CrabError::Internal(format!("iter_staged_lfs_resolutions query: {e}")))?;

        loop {
            let row = rows.next().map_err(|e| {
                CrabError::Internal(format!("iter_staged_lfs_resolutions next: {e}"))
            })?;
            let Some(row) = row else { break };

            let relative_path: String = row.get(0).map_err(|e| {
                CrabError::Internal(format!("iter_staged_lfs_resolutions path: {e}"))
            })?;
            let version_id: String = row.get(1).map_err(|e| {
                CrabError::Internal(format!("iter_staged_lfs_resolutions version: {e}"))
            })?;
            let entry_file_hash = decode_32_blob(
                row.get(2).map_err(|e| {
                    CrabError::Internal(format!("iter_staged_lfs_resolutions entry hash: {e}"))
                })?,
                "staged lfs entry file_hash",
            )?;
            let oid = decode_32_blob(
                row.get(3).map_err(|e| {
                    CrabError::Internal(format!("iter_staged_lfs_resolutions oid: {e}"))
                })?,
                "staged lfs oid",
            )?;
            let size_i64: i64 = row.get(4).map_err(|e| {
                CrabError::Internal(format!("iter_staged_lfs_resolutions size: {e}"))
            })?;
            let recorded_file_hash = decode_32_blob(
                row.get(5).map_err(|e| {
                    CrabError::Internal(format!("iter_staged_lfs_resolutions recorded hash: {e}"))
                })?,
                "staged lfs recorded file_hash",
            )?;
            if entry_file_hash != recorded_file_hash {
                return Err(CrabError::Internal(format!(
                    "staged lfs resolution for {relative_path:?} version {version_id:?} has file_hash drift"
                )));
            }
            let size = u64::try_from(size_i64).map_err(|e| {
                CrabError::Internal(format!(
                    "staged lfs size for {relative_path:?} version {version_id:?} is invalid: {e}"
                ))
            })?;

            f(StagedLfsResolution {
                relative_path,
                version_id,
                file_hash: entry_file_hash,
                resolution: LfsResolution { oid, size },
            })?;
        }
        Ok(())
    }

    // ── Lifecycle ───────────────────────────────────────────────

    /// Force a WAL checkpoint and drop the connection.
    ///
    /// Call this before [`Journal::drop_file`] on the happy path
    /// so the `.db-wal` / `.db-shm` sidecars are collapsed into
    /// the main database file (where possible) before deletion.
    pub fn close(self) -> Result<()> {
        // TRUNCATE checkpoints the WAL into the main db and
        // shrinks the WAL to zero. It is a no-op if nothing
        // needs writing out.
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(|e| CrabError::Internal(format!("wal_checkpoint on close: {e}")))?;
        drop(self.conn);
        Ok(())
    }

    /// Filesystem path of the underlying database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Delete the database file and its `.db-wal` / `.db-shm`
    /// sidecars. Consumes the journal so the caller cannot
    /// accidentally hold a live handle to a deleted file.
    ///
    /// Missing sidecars are not an error (they only exist while
    /// WAL is active); missing main file is. The checkpoint in
    /// [`Journal::close`] is the recommended predecessor so the
    /// sidecars are typically gone by the time we get here.
    pub fn drop_file(self) -> Result<()> {
        let main = self.path.clone();
        // Drop the connection first so the OS releases the file
        // handles before we unlink.
        drop(self.conn);

        fs::remove_file(&main).map_err(|e| {
            CrabError::Internal(format!("drop_file remove {}: {e}", main.display()))
        })?;

        for suffix in &["-wal", "-shm"] {
            let sibling = sibling_path(&main, suffix);
            match fs::remove_file(&sibling) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(CrabError::Internal(format!(
                        "drop_file remove {}: {e}",
                        sibling.display()
                    )));
                }
            }
        }

        Ok(())
    }
}

fn sibling_path(main: &Path, suffix: &str) -> PathBuf {
    let mut os = main.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

fn now_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn mark_staged_row(
    conn: &Connection,
    relative_path: &str,
    version_id: &str,
    file_hash: [u8; 32],
    now: i64,
) -> Result<usize> {
    conn.execute(
        "UPDATE entries
         SET state = ?1, file_hash = ?2, error = NULL,
             skip_reason = NULL, updated_at = ?3
         WHERE relative_path = ?4 AND version_id = ?5",
        params![
            EntryState::TAG_STAGED,
            &file_hash[..],
            now,
            relative_path,
            version_id,
        ],
    )
    .map_err(|e| CrabError::Internal(format!("mark_staged update: {e}")))
}

fn clear_lfs_resolution_row(
    conn: &Connection,
    relative_path: &str,
    version_id: &str,
    context: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM lfs_resolutions WHERE relative_path = ?1 AND version_id = ?2",
        params![relative_path, version_id],
    )
    .map_err(|e| CrabError::Internal(format!("{context}: {e}")))?;
    Ok(())
}

fn decode_32_blob(blob: Vec<u8>, context: &str) -> Result<[u8; 32]> {
    if blob.len() != 32 {
        return Err(CrabError::Internal(format!(
            "{context} has unexpected length {}",
            blob.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&blob);
    Ok(out)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportEntry> {
    let relative_path: String = row.get(0)?;
    let version_id: String = row.get(1)?;
    let size_i64: i64 = row.get(2)?;
    let etag: Option<String> = row.get(3)?;
    let last_modified: i64 = row.get(4)?;
    let is_delete_marker_raw: i64 = row.get(5)?;
    let state_tag: i64 = row.get(6)?;
    let error: Option<String> = row.get(7)?;
    let skip_reason_slug: Option<String> = row.get(8)?;
    let file_hash_blob: Option<Vec<u8>> = row.get(9)?;

    let size = u64::try_from(size_i64).unwrap_or(0);
    let is_delete_marker = is_delete_marker_raw != 0;

    let state = decode_state(state_tag, error, skip_reason_slug, file_hash_blob).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Null, Box::new(e))
    })?;

    Ok(ImportEntry {
        relative_path,
        version_id,
        size,
        etag,
        last_modified,
        is_delete_marker,
        state,
    })
}

#[derive(Debug)]
struct DecodeError(String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

fn decode_state(
    tag: i64,
    error: Option<String>,
    skip_reason_slug: Option<String>,
    file_hash_blob: Option<Vec<u8>>,
) -> std::result::Result<EntryState, DecodeError> {
    match tag {
        t if t == EntryState::TAG_PENDING => Ok(EntryState::Pending),
        t if t == EntryState::TAG_IN_PROGRESS => Ok(EntryState::InProgress),
        t if t == EntryState::TAG_STAGED => {
            let blob = file_hash_blob
                .ok_or_else(|| DecodeError("Staged row missing file_hash blob".into()))?;
            if blob.len() != 32 {
                return Err(DecodeError(format!(
                    "Staged row file_hash has unexpected length {}",
                    blob.len()
                )));
            }
            let mut fh = [0u8; 32];
            fh.copy_from_slice(&blob);
            Ok(EntryState::Staged { file_hash: fh })
        }
        t if t == EntryState::TAG_FAILED => {
            let message = error.unwrap_or_else(|| "<no message recorded>".to_string());
            Ok(EntryState::Failed { message })
        }
        t if t == EntryState::TAG_SKIPPED => {
            let slug = skip_reason_slug
                .ok_or_else(|| DecodeError("Skipped row missing skip_reason".into()))?;
            let reason = SkipReason::from_slug(&slug).map_err(|e| DecodeError(format!("{e}")))?;
            Ok(EntryState::Skipped { reason })
        }
        other => Err(DecodeError(format!("unknown state tag {other}"))),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_inputs() -> PlanInputs {
        PlanInputs {
            source_url: "s3://src/data".into(),
            target_url: "crab://dst/repo".into(),
            source_prefix: "data/".into(),
            target_prefix: "repo/".into(),
            dest_prefix: "imported/data".into(),
            source_mode: SourceModeTag::Versioned,
            window_secs: Some(3600),
            snapshot_at: None,
            since_epoch: Some(1_700_000_000),
            until_epoch: Some(1_800_000_000),
            branch: "main".into(),
            include: vec!["*.bin".into(), "models/**".into()],
            exclude: vec!["*.tmp".into()],
            track: vec!["*.safetensors".into()],
            lfs_source: "resolve".into(),
            lfs_objects: "s3://src/lfs".into(),
        }
    }

    fn entry(
        path: &str,
        version_id: &str,
        size: u64,
        last_modified: i64,
        state: EntryState,
    ) -> ImportEntry {
        ImportEntry {
            relative_path: path.into(),
            version_id: version_id.into(),
            size,
            etag: None,
            last_modified,
            is_delete_marker: false,
            state,
        }
    }

    #[test]
    fn open_creates_journal_dir_and_schema() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        assert_eq!(journal.path(), tmp.path().join(".crab/import-journal.db"));
        assert!(journal.path().exists());
        // Reopening the same path should be a no-op on schema.
        drop(journal);
        let _ = Journal::open(tmp.path()).unwrap();
    }

    #[test]
    fn record_plan_round_trips_through_load_plan() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        let inputs = sample_inputs();
        let recorded = journal.record_plan(&inputs, 1_750_000_000).unwrap();

        let loaded = journal.load_plan().unwrap().expect("plan row");
        assert_eq!(loaded.inputs, recorded.inputs);
        assert_eq!(loaded.plan_checksum, recorded.plan_checksum);
        assert_eq!(loaded.inputs, inputs);
        assert_eq!(loaded.plan_checksum, inputs.checksum());
    }

    #[test]
    fn open_rejects_retired_v1_shape_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        let db_path = crab_dir.join("import-journal.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE plan (
                plan_id        INTEGER PRIMARY KEY CHECK (plan_id = 1),
                source_url     TEXT NOT NULL,
                target_url     TEXT NOT NULL,
                source_prefix  TEXT NOT NULL,
                target_prefix  TEXT NOT NULL,
                source_mode    INTEGER NOT NULL,
                window_secs    INTEGER,
                snapshot_at    INTEGER,
                since_epoch    INTEGER,
                until_epoch    INTEGER,
                branch         TEXT NOT NULL,
                include_json   TEXT NOT NULL,
                exclude_json   TEXT NOT NULL,
                track_json     TEXT NOT NULL,
                plan_checksum  BLOB NOT NULL,
                created_at     INTEGER NOT NULL
            );
            CREATE TABLE journal_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT INTO journal_meta (key, value) VALUES ('schema_version', '1');",
        )
        .unwrap();

        let mut inputs = sample_inputs();
        inputs.dest_prefix.clear();
        inputs.lfs_source = "fail".into();
        inputs.lfs_objects.clear();
        conn.execute(
            "INSERT INTO plan (
                plan_id, source_url, target_url, source_prefix, target_prefix,
                source_mode, window_secs, snapshot_at, since_epoch, until_epoch,
                branch, include_json, exclude_json, track_json, plan_checksum, created_at
            ) VALUES (
                1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15
            )",
            rusqlite::params![
                &inputs.source_url,
                &inputs.target_url,
                &inputs.source_prefix,
                &inputs.target_prefix,
                inputs.source_mode.as_i64(),
                inputs.window_secs.map(|s| i64::try_from(s).unwrap()),
                inputs.snapshot_at,
                inputs.since_epoch,
                inputs.until_epoch,
                &inputs.branch,
                serialize_list(&inputs.include),
                serialize_list(&inputs.exclude),
                serialize_list(&inputs.track),
                vec![0_u8; 32],
                1_750_000_000_i64,
            ],
        )
        .unwrap();
        drop(conn);

        assert!(Journal::open(tmp.path()).is_err());
        let conn = Connection::open(&db_path).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(plan)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!columns.iter().any(|column| column == "dest_prefix"));
    }

    #[test]
    fn verify_plan_accepts_matching_inputs() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let inputs = sample_inputs();
        journal.record_plan(&inputs, 0).unwrap();

        // Same inputs with include globs in a different order must
        // still verify thanks to the sort in `checksum()`.
        let mut shuffled = inputs.clone();
        shuffled.include.reverse();
        shuffled.exclude.reverse();
        shuffled.track.reverse();
        journal.verify_plan(&shuffled).unwrap();
    }

    #[test]
    fn verify_plan_errors_on_drift() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let inputs = sample_inputs();
        journal.record_plan(&inputs, 0).unwrap();

        let mut drifted = inputs.clone();
        drifted.branch = "release".into();

        let err = journal.verify_plan(&drifted).unwrap_err();
        assert!(
            matches!(err, CrabError::ImportPlanMismatch { .. }),
            "expected ImportPlanMismatch, got {err:?}"
        );
    }

    #[test]
    fn verify_plan_errors_on_lfs_policy_drift() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let inputs = sample_inputs();
        journal.record_plan(&inputs, 0).unwrap();

        let mut drifted = inputs.clone();
        drifted.lfs_source = "skip".into();

        let err = journal.verify_plan(&drifted).unwrap_err();
        assert!(
            matches!(err, CrabError::ImportPlanMismatch { .. }),
            "expected ImportPlanMismatch, got {err:?}"
        );
    }

    #[test]
    fn upsert_entry_batch_stores_multiple_rows() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        let batch = vec![
            entry("a.bin", "", 10, 100, EntryState::Pending),
            entry("b.bin", "", 20, 200, EntryState::Pending),
            entry("c.bin", "", 30, 300, EntryState::Pending),
        ];
        journal.upsert_entry_batch(&batch).unwrap();

        // A second batch of the same keys with new sizes replaces.
        let batch2 = vec![entry("b.bin", "", 999, 200, EntryState::Pending)];
        journal.upsert_entry_batch(&batch2).unwrap();

        let mut collected: Vec<ImportEntry> = Vec::new();
        journal
            .iter_entries_sorted_by_time(|e| {
                collected.push(e);
                Ok(())
            })
            .unwrap();
        assert_eq!(collected.len(), 3);
        let b = collected
            .iter()
            .find(|e| e.relative_path == "b.bin")
            .unwrap();
        assert_eq!(b.size, 999);
    }

    #[test]
    fn claim_next_pending_returns_largest_first_then_none() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        let batch = vec![
            entry("small.bin", "", 10, 100, EntryState::Pending),
            entry("large.bin", "", 1000, 200, EntryState::Pending),
            entry("mid.bin", "", 500, 300, EntryState::Pending),
        ];
        journal.upsert_entry_batch(&batch).unwrap();

        let claimed1 = journal.claim_next_pending().unwrap().unwrap();
        assert_eq!(claimed1.relative_path, "large.bin");
        assert_eq!(claimed1.state, EntryState::InProgress);

        let claimed2 = journal.claim_next_pending().unwrap().unwrap();
        assert_eq!(claimed2.relative_path, "mid.bin");

        let claimed3 = journal.claim_next_pending().unwrap().unwrap();
        assert_eq!(claimed3.relative_path, "small.bin");

        assert!(journal.claim_next_pending().unwrap().is_none());
    }

    #[test]
    fn reset_retryable_to_pending_retries_failed_and_in_progress() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        let mut hash = [0u8; 32];
        hash[0] = 7;
        let batch = vec![
            entry("pending.bin", "", 10, 100, EntryState::Pending),
            entry("claimed.bin", "", 20, 200, EntryState::InProgress),
            entry(
                "failed.bin",
                "",
                30,
                300,
                EntryState::Failed {
                    message: "transient read error".into(),
                },
            ),
            entry(
                "staged.bin",
                "",
                40,
                400,
                EntryState::Staged { file_hash: hash },
            ),
        ];
        journal.upsert_entry_batch(&batch).unwrap();

        assert_eq!(journal.reset_retryable_to_pending().unwrap(), 2);

        let mut states = std::collections::BTreeMap::new();
        journal
            .iter_entries_sorted_by_time(|entry| {
                states.insert(entry.relative_path, entry.state);
                Ok(())
            })
            .unwrap();

        assert_eq!(states["pending.bin"], EntryState::Pending);
        assert_eq!(states["claimed.bin"], EntryState::Pending);
        assert_eq!(states["failed.bin"], EntryState::Pending);
        assert!(matches!(
            states["staged.bin"],
            EntryState::Staged { file_hash: _ }
        ));
    }

    #[test]
    fn mark_staged_transitions_state_and_stores_file_hash() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "v1", 10, 0, EntryState::Pending)])
            .unwrap();

        let mut hash = [0u8; 32];
        hash[0] = 0xAB;
        hash[31] = 0xCD;
        journal.mark_staged("a.bin", "v1", hash).unwrap();

        let mut got: Option<ImportEntry> = None;
        journal
            .iter_entries_sorted_by_time(|e| {
                got = Some(e);
                Ok(())
            })
            .unwrap();
        let got = got.unwrap();
        assert_eq!(got.state, EntryState::Staged { file_hash: hash });
    }

    #[test]
    fn mark_staged_lfs_roundtrips_resolution_state() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "v1", 10, 0, EntryState::Pending)])
            .unwrap();

        let mut file_hash = [0u8; 32];
        file_hash[0] = 0xAB;
        let mut oid = [0u8; 32];
        oid[31] = 0xCD;
        let resolution = LfsResolution { oid, size: 123 };

        journal
            .mark_staged_lfs("a.bin", "v1", file_hash, &resolution)
            .unwrap();

        let mut got = Vec::new();
        journal
            .iter_staged_lfs_resolutions(|row| {
                got.push(row);
                Ok(())
            })
            .unwrap();

        assert_eq!(
            got,
            vec![StagedLfsResolution {
                relative_path: "a.bin".into(),
                version_id: "v1".into(),
                file_hash,
                resolution,
            }]
        );

        let mut entries = Vec::new();
        journal
            .iter_entries_sorted_by_time(|entry| {
                entries.push(entry);
                Ok(())
            })
            .unwrap();
        assert_eq!(entries[0].size, 123);
    }

    #[test]
    fn plain_mark_staged_clears_previous_lfs_resolution_state() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "", 10, 0, EntryState::Pending)])
            .unwrap();

        let lfs_hash = [1u8; 32];
        let resolution = LfsResolution {
            oid: [2u8; 32],
            size: 42,
        };
        journal
            .mark_staged_lfs("a.bin", "", lfs_hash, &resolution)
            .unwrap();

        let plain_hash = [3u8; 32];
        journal.mark_staged("a.bin", "", plain_hash).unwrap();

        let mut got = Vec::new();
        journal
            .iter_staged_lfs_resolutions(|row| {
                got.push(row);
                Ok(())
            })
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn mark_failed_transitions_state_and_stores_message() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "", 10, 0, EntryState::Pending)])
            .unwrap();

        journal
            .mark_failed("a.bin", "", "connection reset")
            .unwrap();
        let mut got: Option<ImportEntry> = None;
        journal
            .iter_entries_sorted_by_time(|e| {
                got = Some(e);
                Ok(())
            })
            .unwrap();
        match got.unwrap().state {
            EntryState::Failed { message } => assert_eq!(message, "connection reset"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn mark_skipped_stores_reason_slug() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("p.bin", "", 10, 0, EntryState::Pending)])
            .unwrap();

        journal
            .mark_skipped("p.bin", "", SkipReason::LfsPointer)
            .unwrap();

        let mut got: Option<ImportEntry> = None;
        journal
            .iter_entries_sorted_by_time(|e| {
                got = Some(e);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            got.unwrap().state,
            EntryState::Skipped {
                reason: SkipReason::LfsPointer
            }
        );
    }

    #[test]
    fn versioned_composite_pk_allows_two_versions_of_same_path() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        let batch = vec![
            entry("model.safetensors", "v1", 100, 100, EntryState::Pending),
            entry("model.safetensors", "v2", 200, 200, EntryState::Pending),
        ];
        journal.upsert_entry_batch(&batch).unwrap();

        let mut collected = Vec::new();
        journal
            .iter_entries_sorted_by_time(|e| {
                collected.push((e.relative_path.clone(), e.version_id.clone(), e.size));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            collected,
            vec![
                ("model.safetensors".to_string(), "v1".to_string(), 100),
                ("model.safetensors".to_string(), "v2".to_string(), 200),
            ]
        );

        // Re-upserting (path, v1) replaces; (path, v2) stays put.
        let repl = vec![entry(
            "model.safetensors",
            "v1",
            111,
            100,
            EntryState::Pending,
        )];
        journal.upsert_entry_batch(&repl).unwrap();

        let mut after = Vec::new();
        journal
            .iter_entries_sorted_by_time(|e| {
                after.push((e.version_id.clone(), e.size));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            after,
            vec![("v1".to_string(), 111), ("v2".to_string(), 200)]
        );
    }

    #[test]
    fn iter_entries_sorted_by_time_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let batch = vec![
            entry("c.bin", "", 1, 300, EntryState::Pending),
            entry("a.bin", "", 1, 100, EntryState::Pending),
            entry("b.bin", "", 1, 200, EntryState::Pending),
            // Two rows at the same timestamp resolve by path then version_id.
            entry("same-a.bin", "v2", 1, 200, EntryState::Pending),
            entry("same-a.bin", "v1", 1, 200, EntryState::Pending),
        ];
        journal.upsert_entry_batch(&batch).unwrap();

        let mut ordered = Vec::new();
        journal
            .iter_entries_sorted_by_time(|e| {
                ordered.push((
                    e.last_modified,
                    e.relative_path.clone(),
                    e.version_id.clone(),
                ));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            ordered,
            vec![
                (100, "a.bin".to_string(), "".to_string()),
                (200, "b.bin".to_string(), "".to_string()),
                (200, "same-a.bin".to_string(), "v1".to_string()),
                (200, "same-a.bin".to_string(), "v2".to_string()),
                (300, "c.bin".to_string(), "".to_string()),
            ]
        );
    }

    #[test]
    fn drop_file_deletes_main_and_sidecars() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "", 10, 0, EntryState::Pending)])
            .unwrap();

        let main = journal.path().to_path_buf();
        let wal = sibling_path(&main, "-wal");
        let shm = sibling_path(&main, "-shm");

        // WAL mode should have produced the sidecars by the time we
        // land here (the writes above go through the WAL first).
        assert!(main.exists(), "main db should exist");
        // -wal/-shm exist while WAL is live. They may vanish if the
        // OS already checkpointed, which is fine — the test only
        // asserts they're gone at the end.

        journal.drop_file().unwrap();

        assert!(!main.exists(), "main db should be removed");
        assert!(!wal.exists(), "wal sidecar should be removed");
        assert!(!shm.exists(), "shm sidecar should be removed");
    }

    #[test]
    fn close_checkpoints_and_releases_connection() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .upsert_entry_batch(&[entry("a.bin", "", 10, 0, EntryState::Pending)])
            .unwrap();

        let path = journal.path().to_path_buf();
        journal.close().unwrap();

        // After close, the file remains on disk (close is not drop_file).
        assert!(path.exists());
        // And reopening must succeed (schema marker check passes).
        let _ = Journal::open(tmp.path()).unwrap();
    }

    #[test]
    fn canonical_checksum_is_stable_across_list_orderings() {
        let mut a = sample_inputs();
        let mut b = sample_inputs();
        b.include.reverse();
        b.exclude.reverse();
        b.track.reverse();
        assert_eq!(a.checksum(), b.checksum());

        // Mutating any canonical field changes the checksum.
        a.branch = "dev".into();
        assert_ne!(a.checksum(), b.checksum());
    }
}
