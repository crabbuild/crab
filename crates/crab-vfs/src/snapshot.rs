//! Snapshot store: committed tree as a flat SQLite table.
//!
//! Stores the base tree (committed state) as `(generation, path) → BaseNode`
//! entries in a SQLite database. The snapshot builder walks the git tree at
//! HEAD using `gix-traverse` and detects pointer blobs via `is_pointer`.
//!
//! Generations enable atomic snapshot swaps: a new generation is published
//! while the old one remains readable until pruned.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use gix_hash::ObjectId;
use gix_object::bstr::{BStr, BString, ByteSlice};
use gix_object::{Find, FindExt};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use tracing::{debug, info};

use crate::core::error::{CrabError, Result};
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer, is_pointer};

pub const SNAPSHOT_DB_FILE: &str = "snapshot.sqlite";

const STATE_HEAD_OID: &str = "head_oid";
const STATE_REF_NAME: &str = "ref_name";
const STATE_GENERATION: &str = "generation";
const SNAPSHOT_DB_NAME: &str = "snapshot.sqlite";

// --- Public types ---

/// The type of a node in the base tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    File,
    Dir,
    Symlink,
}

/// A node in the base tree (committed state).
///
/// Small files carry an `object_oid` (git blob SHA-1 hex) for content
/// stored in git packs. Large files carry a `Pointer` with the file hash,
/// size, and optional shard hint for crab-tracked content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseNode {
    /// Path relative to the repository root (forward-slash separated).
    pub path: String,
    /// File, Dir, or Symlink.
    pub node_type: NodeType,
    /// Unix file mode (e.g. 0o100644, 0o040000, 0o120000).
    pub mode: u32,
    /// Git blob OID as hex string (for small files stored in packs).
    pub object_oid: Option<String>,
    /// Crab pointer (for large files tracked by crab).
    pub pointer: Option<Pointer>,
    /// Original file size in bytes.
    pub size: u64,
}

/// Snapshot store backed by SQLite.
pub struct SnapshotStore {
    conn: Mutex<Connection>,
}

impl SnapshotStore {
    /// Open or create a snapshot store at the given path.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let conn = match open_snapshot_db(path) {
            Ok(conn) => conn,
            Err(e) if should_recreate_snapshot(&e) && path.exists() => {
                info!(path = %path.display(), error = %e, "snapshot SQLite open failed, recreating");
                remove_sqlite_files(path)?;
                open_snapshot_db(path)?
            }
            Err(e) => return Err(e),
        };

        let store = Self {
            conn: Mutex::new(conn),
        };
        Ok(store)
    }

    /// Open an existing snapshot database without schema changes or recovery.
    pub fn open_existing(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(map_sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Atomically publish a new generation of the base tree.
    ///
    /// Inserts all `nodes` under the next generation number, updates the
    /// HEAD OID and ref name in the state table, and prunes old generations
    /// (keeping current and previous).
    pub fn publish_generation(
        &self,
        head_oid: &str,
        ref_name: &str,
        nodes: &[BaseNode],
    ) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        let next_gen = {
            let current = get_state_in_tx(&tx, STATE_GENERATION)?
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|e| CrabError::Internal(format!("bad generation value: {e}")))
                })
                .transpose()?
                .unwrap_or(0);
            current + 1
        };

        {
            let mut node_stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO base_nodes_v1 (generation, path, node)
                     VALUES (?1, ?2, ?3)",
                )
                .map_err(map_sqlite_err)?;
            let mut child_stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO base_children_v1
                     (generation, parent_path, name, path) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite_err)?;
            for node in nodes {
                let value = serialize_base_node(node);
                node_stmt
                    .execute(params![next_gen, node.path.as_str(), value.as_slice()])
                    .map_err(map_sqlite_err)?;
                let (parent_path, name) = parent_and_name(&node.path);
                child_stmt
                    .execute(params![next_gen, parent_path, name, node.path.as_str()])
                    .map_err(map_sqlite_err)?;
            }
        }

        set_state_in_tx(&tx, STATE_HEAD_OID, head_oid)?;
        set_state_in_tx(&tx, STATE_REF_NAME, ref_name)?;
        set_state_in_tx(&tx, STATE_GENERATION, &next_gen.to_string())?;
        tx.commit().map_err(map_sqlite_err)?;
        drop(conn);

        debug!(
            generation = next_gen,
            head_oid,
            ref_name,
            node_count = nodes.len(),
            "published snapshot generation"
        );

        // Prune old generations (keep current and previous).
        self.prune_old_generations(next_gen)?;

        Ok(())
    }

    /// Build and atomically publish a Git tree without materializing all nodes in memory.
    pub fn publish_generation_from_git(
        &self,
        git_dir: &Path,
        head_oid: &str,
        ref_name: &str,
    ) -> Result<usize> {
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        let next_gen = {
            let current = get_state_in_tx(&tx, STATE_GENERATION)?
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|e| CrabError::Internal(format!("bad generation value: {e}")))
                })
                .transpose()?
                .unwrap_or(0);
            current + 1
        };
        let mut node_count = 0usize;

        {
            let mut node_stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO base_nodes_v1 (generation, path, node)
                     VALUES (?1, ?2, ?3)",
                )
                .map_err(map_sqlite_err)?;
            let mut child_stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO base_children_v1
                     (generation, parent_path, name, path) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite_err)?;
            walk_snapshot(git_dir, head_oid, |node| {
                let value = serialize_base_node(&node);
                node_stmt
                    .execute(params![next_gen, node.path.as_str(), value.as_slice()])
                    .map_err(map_sqlite_err)?;
                let (parent_path, name) = parent_and_name(&node.path);
                child_stmt
                    .execute(params![next_gen, parent_path, name, node.path.as_str()])
                    .map_err(map_sqlite_err)?;
                node_count = node_count.saturating_add(1);
                Ok(())
            })?;
        }

        set_state_in_tx(&tx, STATE_HEAD_OID, head_oid)?;
        set_state_in_tx(&tx, STATE_REF_NAME, ref_name)?;
        set_state_in_tx(&tx, STATE_GENERATION, &next_gen.to_string())?;
        tx.commit().map_err(map_sqlite_err)?;
        drop(conn);

        debug!(
            generation = next_gen,
            head_oid, ref_name, node_count, "published streamed snapshot generation"
        );
        self.prune_old_generations(next_gen)?;
        Ok(node_count)
    }

    /// Look up a single node by generation and path.
    pub fn get_node(&self, generation: i64, path: &str) -> Result<Option<BaseNode>> {
        let conn = self.connection()?;
        conn.query_row(
            "SELECT node FROM base_nodes_v1 WHERE generation = ?1 AND path = ?2",
            params![generation, path],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(map_sqlite_err)?
        .map(|bytes| deserialize_base_node(&bytes))
        .transpose()
    }

    /// List immediate children of `parent_path` in the given generation.
    ///
    /// For the root directory, pass an empty string.
    pub fn list_children(&self, generation: i64, parent_path: &str) -> Result<Vec<BaseNode>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT node.node
                 FROM base_children_v1 AS child
                 JOIN base_nodes_v1 AS node
                   ON node.generation = child.generation AND node.path = child.path
                 WHERE child.generation = ?1 AND child.parent_path = ?2
                 ORDER BY child.name",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![generation, parent_path], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(map_sqlite_err)?;
        rows.map(|row| deserialize_base_node(&row.map_err(map_sqlite_err)?))
            .collect()
    }

    /// List at most `limit` immediate children after `after_name`.
    pub fn list_children_page(
        &self,
        generation: i64,
        parent_path: &str,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BaseNode>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT node.node
                 FROM base_children_v1 AS child
                 JOIN base_nodes_v1 AS node
                   ON node.generation = child.generation AND node.path = child.path
                 WHERE child.generation = ?1 AND child.parent_path = ?2 AND child.name > ?3
                 ORDER BY child.name LIMIT ?4",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(
                params![generation, parent_path, after_name.unwrap_or(""), limit],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map_err(map_sqlite_err)?;
        rows.map(|row| deserialize_base_node(&row.map_err(map_sqlite_err)?))
            .collect()
    }

    /// Return the current generation number, or `None` if no generation
    /// has been published yet.
    pub fn current_generation(&self) -> Result<Option<i64>> {
        self.get_state(STATE_GENERATION)?
            .map(|value| {
                value
                    .parse()
                    .map_err(|e| CrabError::Internal(format!("bad generation value: {e}")))
            })
            .transpose()
    }

    /// Return the stored HEAD OID, if any.
    pub fn head_oid(&self) -> Result<Option<String>> {
        self.get_state(STATE_HEAD_OID)
    }

    /// Return the stored ref name, if any.
    pub fn ref_name(&self) -> Result<Option<String>> {
        self.get_state(STATE_REF_NAME)
    }

    /// Update a single node's size in the given generation without a full
    /// republish. Used by the size-backfill callback after hydrating a
    /// small file whose tree entry had an unknown (zero) size.
    pub fn update_node_size(&self, generation: i64, path: &str, size: u64) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        let existing = tx
            .query_row(
                "SELECT node FROM base_nodes_v1 WHERE generation = ?1 AND path = ?2",
                params![generation, path],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let Some(existing) = existing else {
            debug!(
                generation,
                path, "update_node_size: node not found, skipping"
            );
            return Ok(());
        };

        let mut node = deserialize_base_node(&existing)?;
        node.size = size;
        let value = serialize_base_node(&node);
        tx.execute(
            "UPDATE base_nodes_v1 SET node = ?3 WHERE generation = ?1 AND path = ?2",
            params![generation, path, value.as_slice()],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;

        debug!(generation, path, size, "backfilled node size");
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| CrabError::Internal("snapshot SQLite connection poisoned".into()))
    }

    fn get_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connection()?;
        conn.query_row(
            "SELECT value FROM state_v1 WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_err)
    }

    /// Prune generations older than `current - 1`.
    fn prune_old_generations(&self, current: i64) -> Result<()> {
        let keep_min = current.saturating_sub(1);
        if keep_min <= 0 {
            return Ok(());
        }

        let conn = self.connection()?;
        let children_pruned = conn
            .execute(
                "DELETE FROM base_children_v1 WHERE generation < ?1",
                params![keep_min],
            )
            .map_err(map_sqlite_err)?;
        let nodes_pruned = conn
            .execute(
                "DELETE FROM base_nodes_v1 WHERE generation < ?1",
                params![keep_min],
            )
            .map_err(map_sqlite_err)?;
        if nodes_pruned > 0 || children_pruned > 0 {
            debug!(
                nodes_pruned,
                children_pruned, keep_min, "pruned old snapshot generations"
            );
        }

        Ok(())
    }
}

fn open_snapshot_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path).map_err(map_sqlite_err)?;
    configure_snapshot_connection(&conn)?;
    initialize_snapshot_schema(&conn)?;
    Ok(conn)
}

fn configure_snapshot_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(map_sqlite_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(map_sqlite_err)?;
    Ok(())
}

fn initialize_snapshot_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS base_nodes_v1 (
            generation INTEGER NOT NULL,
            path TEXT NOT NULL COLLATE BINARY,
            node BLOB NOT NULL,
            PRIMARY KEY (generation, path)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS state_v1 (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS base_children_v1 (
            generation  INTEGER NOT NULL,
            parent_path TEXT NOT NULL COLLATE BINARY,
            name        TEXT NOT NULL COLLATE BINARY,
            path        TEXT NOT NULL COLLATE BINARY,
            PRIMARY KEY (generation, parent_path, name)
        ) WITHOUT ROWID;
        ",
    )
    .map_err(map_sqlite_err)
}

fn get_state_in_tx(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT value FROM state_v1 WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(map_sqlite_err)
}

fn set_state_in_tx(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO state_v1 (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn parent_and_name(path: &str) -> (&str, &str) {
    path.rsplit_once('/')
        .map_or(("", path), |(parent, name)| (parent, name))
}

fn remove_sqlite_files(path: &Path) -> Result<()> {
    for candidate in [path.to_path_buf(), wal_path(path), shm_path(path)] {
        if let Err(e) = std::fs::remove_file(&candidate)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(CrabError::Io(e));
        }
    }
    Ok(())
}

fn wal_path(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-wal", path.display()))
}

fn shm_path(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-shm", path.display()))
}

fn should_recreate_snapshot(e: &CrabError) -> bool {
    let CrabError::Internal(message) = e else {
        return false;
    };
    message.contains("not a database")
        || message.contains("database disk image is malformed")
        || message.contains("file is not a database")
        || message.contains("unable to open database file")
}

fn map_sqlite_err(e: rusqlite::Error) -> CrabError {
    match &e {
        rusqlite::Error::SqliteFailure(err, _)
            if matches!(
                err.code,
                ErrorCode::NotADatabase
                    | ErrorCode::DatabaseCorrupt
                    | ErrorCode::CannotOpen
                    | ErrorCode::SchemaChanged
            ) =>
        {
            CrabError::Internal(e.to_string())
        }
        _ => CrabError::Internal(format!("snapshot SQLite error: {e}")),
    }
}

// --- Snapshot builder (task 44.3) ---

/// Walk the git tree at `head_oid` and produce a flat list of `BaseNode`s.
pub fn build_snapshot(git_dir: &Path, head_oid_hex: &str) -> Result<Vec<BaseNode>> {
    let mut nodes = Vec::new();
    walk_snapshot(git_dir, head_oid_hex, |node| {
        nodes.push(node);
        Ok(())
    })?;
    debug!(node_count = nodes.len(), "snapshot build complete");
    Ok(nodes)
}

fn walk_snapshot(
    git_dir: &Path,
    head_oid_hex: &str,
    mut visit_node: impl FnMut(BaseNode) -> Result<()>,
) -> Result<()> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git objects directory not found: {}", objects_dir.display()),
        )));
    }

    let odb = gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git ODB at {}: {e}",
            objects_dir.display()
        ))
    })?;

    let head_oid = ObjectId::from_hex(head_oid_hex.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid HEAD OID {head_oid_hex}: {e}")))?;

    let tree_id = {
        let mut buf = Vec::new();
        let mut commit_iter = odb
            .find_commit_iter(&head_oid, &mut buf)
            .map_err(|e| CrabError::Internal(format!("failed to read commit {head_oid}: {e}")))?;
        commit_iter.tree_id().map_err(|e| {
            CrabError::Internal(format!("failed to parse tree from commit {head_oid}: {e}"))
        })?
    };

    let mut visitor = SnapshotTreeVisitor::new(|path, oid, mode| {
        if let Some(node) = snapshot_node_from_entry(&odb, path, oid, mode)? {
            visit_node(node)?;
        }
        Ok(())
    });
    let mut state = gix_traverse::tree::depthfirst::State::default();
    gix_traverse::tree::depthfirst(tree_id, &mut state, &odb, &mut visitor)
        .map_err(|e| CrabError::Internal(format!("tree walk error: {e}")))?;
    if let Some(error) = visitor.error {
        return Err(error);
    }
    Ok(())
}

fn snapshot_node_from_entry<O>(
    odb: &O,
    path: BString,
    oid: ObjectId,
    entry_mode: gix_object::tree::EntryMode,
) -> Result<Option<BaseNode>>
where
    O: Find + gix_odb::Header,
{
    let mode = u32::from(entry_mode.value());
    let path = path.to_str_lossy().into_owned();
    let node_type = match entry_mode.kind() {
        gix_object::tree::EntryKind::Tree => NodeType::Dir,
        gix_object::tree::EntryKind::Link => NodeType::Symlink,
        gix_object::tree::EntryKind::Commit => return Ok(None),
        _ => NodeType::File,
    };
    if node_type == NodeType::Dir {
        return Ok(Some(BaseNode {
            path,
            node_type,
            mode,
            object_oid: None,
            pointer: None,
            size: 0,
        }));
    }

    let oid_hex = oid.to_string();
    let Some(header) = gix_odb::Header::try_header(odb, &oid)
        .map_err(|e| CrabError::Internal(format!("failed to read blob header {oid_hex}: {e}")))?
    else {
        return Ok(Some(BaseNode {
            path,
            node_type,
            mode,
            object_oid: Some(oid_hex),
            pointer: None,
            size: 0,
        }));
    };
    if header.kind() != gix_object::Kind::Blob {
        return Err(CrabError::Internal(format!(
            "tree entry {path} references non-blob object {oid_hex}"
        )));
    }

    let mut size = header.size();
    let mut pointer = None;
    if size <= MAX_POINTER_SIZE as u64 {
        let mut blob_buf = Vec::with_capacity(MAX_POINTER_SIZE);
        let data = odb
            .try_find(&oid, &mut blob_buf)
            .map_err(|e| CrabError::Internal(format!("failed to read blob {oid_hex}: {e}")))?
            .ok_or_else(|| {
                CrabError::Internal(format!("blob {oid_hex} disappeared during snapshot build"))
            })?;
        if data.kind != gix_object::Kind::Blob {
            return Err(CrabError::Internal(format!(
                "tree entry {path} references non-blob object {oid_hex}"
            )));
        }
        if is_pointer(data.data) {
            let parsed = Pointer::parse(data.data)
                .map_err(|e| CrabError::Internal(format!("invalid Crab pointer at {path}: {e}")))?;
            size = parsed.size;
            pointer = Some(parsed);
        }
    }

    Ok(Some(BaseNode {
        path,
        node_type,
        mode,
        object_oid: Some(oid_hex),
        pointer,
        size,
    }))
}

struct SnapshotTreeVisitor<F> {
    path_deque: VecDeque<BString>,
    path: BString,
    visit: F,
    error: Option<CrabError>,
}

impl<F> SnapshotTreeVisitor<F> {
    fn new(visit: F) -> Self {
        Self {
            path_deque: VecDeque::new(),
            path: BString::default(),
            visit,
            error: None,
        }
    }

    fn push_element(&mut self, component: &BStr) {
        if component.is_empty() {
            return;
        }
        if !self.path.is_empty() {
            self.path.push(b'/');
        }
        self.path.extend_from_slice(component);
    }

    fn pop_element(&mut self) {
        if let Some(position) = self.path.rfind_byte(b'/') {
            self.path.resize(position, 0);
        } else {
            self.path.clear();
        }
    }

    fn visit_entry(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool>
    where
        F: FnMut(BString, ObjectId, gix_object::tree::EntryMode) -> Result<()>,
    {
        match (self.visit)(self.path.clone(), entry.oid.to_owned(), entry.mode) {
            Ok(()) => std::ops::ControlFlow::Continue(true),
            Err(error) => {
                self.error = Some(error);
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

impl<F> gix_traverse::tree::Visit for SnapshotTreeVisitor<F>
where
    F: FnMut(BString, ObjectId, gix_object::tree::EntryMode) -> Result<()>,
{
    fn pop_back_tracked_path_and_set_current(&mut self) {
        self.path = self.path_deque.pop_back().unwrap_or_default();
    }

    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.path = self.path_deque.pop_front().unwrap_or_default();
    }

    fn push_back_tracked_path_component(&mut self, component: &BStr) {
        self.push_element(component);
        self.path_deque.push_back(self.path.clone());
    }

    fn push_path_component(&mut self, component: &BStr) {
        self.push_element(component);
    }

    fn pop_path_component(&mut self) {
        self.pop_element();
    }

    fn visit_tree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> gix_traverse::tree::visit::Action {
        self.visit_entry(entry)
    }

    fn visit_nontree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> gix_traverse::tree::visit::Action {
        self.visit_entry(entry)
    }
}

// --- Serialization ---
//
// BaseNode is serialized as a simple binary format:
//   [1 byte: node_type] [4 bytes LE: mode] [8 bytes LE: size]
//   [2 bytes LE: path_len] [path_bytes]
//   [1 byte: has_oid] [if has_oid: 2 bytes LE oid_len, oid_bytes]
//   [1 byte: has_pointer] [if has_pointer: pointer_bytes (serialize())]

fn serialize_base_node(node: &BaseNode) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // node_type
    buf.push(match node.node_type {
        NodeType::File => 0,
        NodeType::Dir => 1,
        NodeType::Symlink => 2,
    });

    // mode
    buf.extend_from_slice(&node.mode.to_le_bytes());

    // size
    buf.extend_from_slice(&node.size.to_le_bytes());

    // path
    let path_bytes = node.path.as_bytes();
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);

    // object_oid
    match &node.object_oid {
        Some(oid) => {
            buf.push(1);
            let oid_bytes = oid.as_bytes();
            buf.extend_from_slice(&(oid_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(oid_bytes);
        }
        None => buf.push(0),
    }

    // pointer
    match &node.pointer {
        Some(ptr) => {
            buf.push(1);
            let ptr_bytes = ptr.serialize();
            buf.extend_from_slice(&(ptr_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(&ptr_bytes);
        }
        None => buf.push(0),
    }

    buf
}

fn deserialize_base_node(data: &[u8]) -> Result<BaseNode> {
    let err = || CrabError::CorruptObject {
        path: SNAPSHOT_DB_NAME.into(),
        reason: "truncated BaseNode".into(),
    };

    if data.len() < 13 {
        return Err(err());
    }

    let mut pos = 0;

    // node_type
    let node_type = match data[pos] {
        0 => NodeType::File,
        1 => NodeType::Dir,
        2 => NodeType::Symlink,
        other => {
            return Err(CrabError::CorruptObject {
                path: SNAPSHOT_DB_NAME.into(),
                reason: format!("unknown node_type: {other}"),
            });
        }
    };
    pos += 1;

    // mode
    let mode = u32::from_le_bytes(data[pos..pos + 4].try_into().map_err(|_| err())?);
    pos += 4;

    // size
    let size = u64::from_le_bytes(data[pos..pos + 8].try_into().map_err(|_| err())?);
    pos += 8;

    // path
    if pos + 2 > data.len() {
        return Err(err());
    }
    let path_len = u16::from_le_bytes(data[pos..pos + 2].try_into().map_err(|_| err())?) as usize;
    pos += 2;
    if pos + path_len > data.len() {
        return Err(err());
    }
    let path = std::str::from_utf8(&data[pos..pos + path_len])
        .map_err(|e| CrabError::CorruptObject {
            path: SNAPSHOT_DB_NAME.into(),
            reason: format!("invalid UTF-8 path: {e}"),
        })?
        .to_owned();
    pos += path_len;

    // object_oid
    if pos >= data.len() {
        return Err(err());
    }
    let object_oid = if data[pos] == 1 {
        pos += 1;
        if pos + 2 > data.len() {
            return Err(err());
        }
        let oid_len =
            u16::from_le_bytes(data[pos..pos + 2].try_into().map_err(|_| err())?) as usize;
        pos += 2;
        if pos + oid_len > data.len() {
            return Err(err());
        }
        let oid = std::str::from_utf8(&data[pos..pos + oid_len])
            .map_err(|e| CrabError::CorruptObject {
                path: SNAPSHOT_DB_NAME.into(),
                reason: format!("invalid UTF-8 OID: {e}"),
            })?
            .to_owned();
        pos += oid_len;
        Some(oid)
    } else {
        pos += 1;
        None
    };

    // pointer
    if pos >= data.len() {
        return Err(err());
    }
    let pointer = if data[pos] == 1 {
        pos += 1;
        if pos + 2 > data.len() {
            return Err(err());
        }
        let ptr_len =
            u16::from_le_bytes(data[pos..pos + 2].try_into().map_err(|_| err())?) as usize;
        pos += 2;
        if pos + ptr_len > data.len() {
            return Err(err());
        }
        let ptr = Pointer::parse(&data[pos..pos + ptr_len])?;
        Some(ptr)
    } else {
        None
    };

    Ok(BaseNode {
        path,
        node_type,
        mode,
        object_oid,
        pointer,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pointer() -> Pointer {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        Pointer {
            file_hash: h,
            size: 1_048_576,
            shard_hint: None,
        }
    }

    fn make_file_node(path: &str, size: u64) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("abcd1234".to_owned()),
            pointer: None,
            size,
        }
    }

    fn make_dir_node(path: &str) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::Dir,
            mode: 0o040000,
            object_oid: None,
            pointer: None,
            size: 0,
        }
    }

    fn make_pointer_node(path: &str) -> BaseNode {
        let ptr = sample_pointer();
        let size = ptr.size;
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("deadbeef".to_owned()),
            pointer: Some(ptr),
            size,
        }
    }

    fn temp_store() -> (tempfile::TempDir, SnapshotStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_DB_FILE);
        let store = SnapshotStore::open_or_create(&path).unwrap();
        (dir, store)
    }

    // --- BaseNode serialization round-trip ---

    #[test]
    fn base_node_file_round_trip() {
        let node = make_file_node("src/main.rs", 4096);
        let bytes = serialize_base_node(&node);
        let decoded = deserialize_base_node(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn base_node_dir_round_trip() {
        let node = make_dir_node("src");
        let bytes = serialize_base_node(&node);
        let decoded = deserialize_base_node(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn base_node_symlink_round_trip() {
        let node = BaseNode {
            path: "link".to_owned(),
            node_type: NodeType::Symlink,
            mode: 0o120000,
            object_oid: Some("cafe0000".to_owned()),
            pointer: None,
            size: 10,
        };
        let bytes = serialize_base_node(&node);
        let decoded = deserialize_base_node(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn base_node_pointer_round_trip() {
        let node = make_pointer_node("data/model.safetensors");
        let bytes = serialize_base_node(&node);
        let decoded = deserialize_base_node(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    // --- SnapshotStore tests ---

    #[test]
    fn empty_store_has_no_generation() {
        let (_dir, store) = temp_store();
        assert_eq!(store.current_generation().unwrap(), None);
        assert_eq!(store.head_oid().unwrap(), None);
        assert_eq!(store.ref_name().unwrap(), None);
    }

    #[test]
    fn publish_and_query_generation() {
        let (_dir, store) = temp_store();
        let nodes = vec![
            make_dir_node("src"),
            make_file_node("src/main.rs", 100),
            make_file_node("README.md", 50),
        ];

        store
            .publish_generation("aabbccdd", "refs/heads/main", &nodes)
            .unwrap();

        assert_eq!(store.current_generation().unwrap(), Some(1));
        assert_eq!(store.head_oid().unwrap().as_deref(), Some("aabbccdd"));
        assert_eq!(
            store.ref_name().unwrap().as_deref(),
            Some("refs/heads/main")
        );

        // Query individual nodes.
        let node = store.get_node(1, "src/main.rs").unwrap().unwrap();
        assert_eq!(node.size, 100);
        assert_eq!(node.node_type, NodeType::File);

        let dir = store.get_node(1, "src").unwrap().unwrap();
        assert_eq!(dir.node_type, NodeType::Dir);

        // Missing node.
        assert!(store.get_node(1, "nonexistent").unwrap().is_none());
        // Wrong generation.
        assert!(store.get_node(99, "src/main.rs").unwrap().is_none());
    }

    #[test]
    fn list_children_root() {
        let (_dir, store) = temp_store();
        let nodes = vec![
            make_dir_node("src"),
            make_file_node("src/main.rs", 100),
            make_file_node("README.md", 50),
            make_dir_node("docs"),
            make_file_node("docs/guide.md", 200),
        ];

        store
            .publish_generation("aabb", "refs/heads/main", &nodes)
            .unwrap();

        let root_children = store.list_children(1, "").unwrap();
        let paths: Vec<&str> = root_children.iter().map(|n| n.path.as_str()).collect();
        assert!(paths.contains(&"src"));
        assert!(paths.contains(&"README.md"));
        assert!(paths.contains(&"docs"));
        // Nested entries should not appear at root level.
        assert!(!paths.contains(&"src/main.rs"));
        assert!(!paths.contains(&"docs/guide.md"));
    }

    #[test]
    fn list_children_subdir() {
        let (_dir, store) = temp_store();
        let nodes = vec![
            make_dir_node("src"),
            make_file_node("src/main.rs", 100),
            make_file_node("src/lib.rs", 200),
            make_dir_node("src/utils"),
            make_file_node("src/utils/helper.rs", 50),
        ];

        store
            .publish_generation("aabb", "refs/heads/main", &nodes)
            .unwrap();

        let src_children = store.list_children(1, "src").unwrap();
        let paths: Vec<&str> = src_children.iter().map(|n| n.path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"src/utils"));
        // Nested entries should not appear.
        assert!(!paths.contains(&"src/utils/helper.rs"));
    }

    #[test]
    fn list_children_page_uses_name_cursor() {
        let (_dir, store) = temp_store();
        let nodes = (0..10)
            .map(|index| make_file_node(&format!("data/file-{index:02}.bin"), 1))
            .collect::<Vec<_>>();
        store
            .publish_generation("aabb", "refs/heads/main", &nodes)
            .unwrap();

        let page = store
            .list_children_page(1, "data", Some("file-04.bin"), 3)
            .unwrap();
        assert_eq!(
            page.iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["data/file-05.bin", "data/file-06.bin", "data/file-07.bin"]
        );
    }

    #[test]
    fn multiple_generations_and_prune() {
        let (_dir, store) = temp_store();

        // Generation 1.
        store
            .publish_generation("oid1", "refs/heads/main", &[make_file_node("a.txt", 10)])
            .unwrap();
        assert_eq!(store.current_generation().unwrap(), Some(1));

        // Generation 2.
        store
            .publish_generation("oid2", "refs/heads/main", &[make_file_node("b.txt", 20)])
            .unwrap();
        assert_eq!(store.current_generation().unwrap(), Some(2));

        // Generation 1 should still be queryable (keep current and previous).
        assert!(store.get_node(1, "a.txt").unwrap().is_some());
        assert!(store.get_node(2, "b.txt").unwrap().is_some());

        // Generation 3 — should prune generation 1.
        store
            .publish_generation("oid3", "refs/heads/main", &[make_file_node("c.txt", 30)])
            .unwrap();
        assert_eq!(store.current_generation().unwrap(), Some(3));

        // Generation 1 should be pruned.
        assert!(store.get_node(1, "a.txt").unwrap().is_none());
        // Generation 2 still available.
        assert!(store.get_node(2, "b.txt").unwrap().is_some());
        // Generation 3 available.
        assert!(store.get_node(3, "c.txt").unwrap().is_some());
    }

    #[test]
    fn pointer_node_stored_and_retrieved() {
        let (_dir, store) = temp_store();
        let node = make_pointer_node("model.bin");
        store
            .publish_generation("oid1", "refs/heads/main", &[node.clone()])
            .unwrap();

        let retrieved = store.get_node(1, "model.bin").unwrap().unwrap();
        assert_eq!(retrieved.pointer, node.pointer);
        assert_eq!(retrieved.size, 1_048_576);
    }

    // --- update_node_size tests ---

    #[test]
    fn update_node_size_backfills_zero_size() {
        let (_dir, store) = temp_store();
        let node = make_file_node("readme.md", 0);
        store
            .publish_generation("aabb", "refs/heads/main", &[node])
            .unwrap();

        // Size starts at 0.
        let before = store.get_node(1, "readme.md").unwrap().unwrap();
        assert_eq!(before.size, 0);

        // Backfill with actual size.
        store.update_node_size(1, "readme.md", 1234).unwrap();

        let after = store.get_node(1, "readme.md").unwrap().unwrap();
        assert_eq!(after.size, 1234);
        // Other fields unchanged.
        assert_eq!(after.node_type, NodeType::File);
        assert_eq!(after.mode, 0o100644);
        assert_eq!(after.object_oid.as_deref(), Some("abcd1234"));
    }

    #[test]
    fn update_node_size_missing_node_is_noop() {
        let (_dir, store) = temp_store();
        store
            .publish_generation("aabb", "refs/heads/main", &[make_file_node("a.txt", 10)])
            .unwrap();

        // Updating a non-existent path succeeds silently.
        store.update_node_size(1, "nonexistent.txt", 999).unwrap();

        // Original node unchanged.
        let node = store.get_node(1, "a.txt").unwrap().unwrap();
        assert_eq!(node.size, 10);
    }

    #[test]
    fn update_node_size_wrong_generation_is_noop() {
        let (_dir, store) = temp_store();
        store
            .publish_generation("aabb", "refs/heads/main", &[make_file_node("a.txt", 0)])
            .unwrap();

        // Wrong generation — no update.
        store.update_node_size(99, "a.txt", 500).unwrap();

        let node = store.get_node(1, "a.txt").unwrap().unwrap();
        assert_eq!(node.size, 0);
    }

    // --- Snapshot builder tests (require git) ---

    #[test]
    fn build_snapshot_on_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status();

        // Create files: a regular file and a pointer file.
        std::fs::write(repo_dir.join("hello.txt"), b"hello world\n").unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/main.rs"), b"fn main() {}\n").unwrap();

        let pointer_content = format!(
            "version https://crab.dev/spec/v1\nfile-hash {}\nsize 999999\n",
            "ab".repeat(32)
        );
        std::fs::write(repo_dir.join("model.bin"), pointer_content.as_bytes()).unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();

        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let git_dir = repo_dir.join(".git");
        let nodes = build_snapshot(&git_dir, &head_sha).unwrap();

        // Should have: src (dir), hello.txt, src/main.rs, model.bin
        assert!(
            nodes.len() >= 4,
            "expected at least 4 nodes, got {}",
            nodes.len()
        );

        let hello = nodes.iter().find(|n| n.path == "hello.txt");
        assert!(hello.is_some(), "hello.txt not found");
        let hello = hello.unwrap();
        assert_eq!(hello.node_type, NodeType::File);
        assert_eq!(hello.size, 12); // "hello world\n"
        assert!(hello.pointer.is_none());
        assert!(hello.object_oid.is_some());

        let model = nodes.iter().find(|n| n.path == "model.bin");
        assert!(model.is_some(), "model.bin not found");
        let model = model.unwrap();
        assert_eq!(model.node_type, NodeType::File);
        assert!(model.pointer.is_some());
        assert_eq!(model.size, 999_999); // from pointer, not blob size

        let src_dir = nodes.iter().find(|n| n.path == "src");
        assert!(src_dir.is_some(), "src dir not found");
        assert_eq!(src_dir.unwrap().node_type, NodeType::Dir);

        let main_rs = nodes.iter().find(|n| n.path == "src/main.rs");
        assert!(main_rs.is_some(), "src/main.rs not found");
        assert_eq!(main_rs.unwrap().node_type, NodeType::File);

        let streamed = SnapshotStore::open_or_create(&repo_dir.join("streamed.sqlite")).unwrap();
        let streamed_count = streamed
            .publish_generation_from_git(&git_dir, &head_sha, "refs/heads/main")
            .unwrap();
        assert_eq!(streamed_count, nodes.len());
        assert_eq!(
            streamed.get_node(1, "model.bin").unwrap().unwrap(),
            model.clone()
        );
    }

    #[test]
    fn build_snapshot_errors_on_missing_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        // No objects/ directory.

        let result = build_snapshot(&git_dir, &"a".repeat(40));
        assert!(result.is_err());
    }
}
