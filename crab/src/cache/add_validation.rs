//! Per-worktree proof cache for content already verified by `crab add`.
//!
//! A row is only a shortcut for a descriptor-safe content hash. The token
//! binds the literal Git path, indexed pointer identity and bytes, file mode,
//! and every filesystem stat field captured during verification. An exact
//! token can resolve Git's racy-stat ambiguity; a miss must hash the file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::error::{CrabError, Result};

pub(crate) const ADD_VALIDATIONS_FILENAME: &str = "add-validations-v1.sqlite";
const SCHEMA_VERSION: i64 = 1;

pub(crate) struct AddValidationCache {
    connection: Connection,
}

impl AddValidationCache {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        let connection = Connection::open(path)
            .map_err(|error| database_error("open add validation cache", error))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| database_error("configure add validation cache timeout", error))?;
        connection
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(|error| database_error("configure add validation cache", error))?;

        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(|error| database_error("read add validation cache version", error))?;
        if version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE add_validations (
                         path BLOB PRIMARY KEY NOT NULL,
                         token BLOB NOT NULL CHECK(length(token) = 32)
                     ) WITHOUT ROWID;
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(|error| database_error("initialize add validation cache", error))?;
        } else if version != SCHEMA_VERSION {
            return Err(CrabError::Internal(format!(
                "unsupported add validation cache schema {version}; expected v{SCHEMA_VERSION}"
            )));
        }

        Ok(Self { connection })
    }

    pub(crate) fn contains(&self, path: &[u8], token: &[u8; 32]) -> Result<bool> {
        self.connection
            .prepare_cached("SELECT 1 FROM add_validations WHERE path = ?1 AND token = ?2")
            .map_err(|error| database_error("prepare add validation cache query", error))?
            .query_row(params![path, token.as_slice()], |_| Ok(()))
            .optional()
            .map(|row| row.is_some())
            .map_err(|error| database_error("query add validation cache", error))
    }

    pub(crate) fn upsert(&mut self, rows: &[(Vec<u8>, [u8; 32])]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| database_error("begin add validation cache update", error))?;
        {
            let mut statement = transaction
                .prepare_cached(
                    "INSERT INTO add_validations(path, token) VALUES (?1, ?2)
                     ON CONFLICT(path) DO UPDATE SET token = excluded.token",
                )
                .map_err(|error| database_error("prepare add validation cache update", error))?;
            for (path, token) in rows {
                statement
                    .execute(params![path, token.as_slice()])
                    .map_err(|error| database_error("write add validation cache row", error))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| database_error("commit add validation cache update", error))
    }
}

pub(crate) fn cache_path_for_context(context: &crate::git::worktree::WorktreeContext) -> PathBuf {
    context.per_worktree_crab_dir.join(ADD_VALIDATIONS_FILENAME)
}

pub(crate) fn validation_token(
    path: &[u8],
    pointer_oid: &[u8],
    pointer_bytes: &[u8],
    mode: u32,
    stat: &gix_index::entry::Stat,
    len: u64,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab add validation v1\0");
    update_sized(&mut hasher, path);
    update_sized(&mut hasher, pointer_oid);
    update_sized(&mut hasher, pointer_bytes);
    hasher.update(&mode.to_le_bytes());
    hasher.update(&stat.mtime.secs.to_le_bytes());
    hasher.update(&stat.mtime.nsecs.to_le_bytes());
    hasher.update(&stat.ctime.secs.to_le_bytes());
    hasher.update(&stat.ctime.nsecs.to_le_bytes());
    hasher.update(&stat.dev.to_le_bytes());
    hasher.update(&stat.ino.to_le_bytes());
    hasher.update(&stat.uid.to_le_bytes());
    hasher.update(&stat.gid.to_le_bytes());
    hasher.update(&stat.size.to_le_bytes());
    hasher.update(&len.to_le_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(unix)]
pub(crate) fn stat_is_cacheable(stat: &gix_index::entry::Stat) -> bool {
    // Unix ctime changes on content mutation. Requiring non-zero nanoseconds
    // rejects coarse filesystems where a same-second rewrite could alias.
    stat.ctime.nsecs != 0
}

#[cfg(not(unix))]
pub(crate) fn stat_is_cacheable(_stat: &gix_index::entry::Stat) -> bool {
    // Windows exposes creation time in this field, not Unix change time.
    false
}

fn update_sized(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn database_error(action: &str, error: rusqlite::Error) -> CrabError {
    CrabError::Internal(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat() -> gix_index::entry::Stat {
        gix_index::entry::Stat {
            mtime: gix_index::entry::stat::Time { secs: 1, nsecs: 2 },
            ctime: gix_index::entry::stat::Time { secs: 3, nsecs: 4 },
            dev: 5,
            ino: 6,
            uid: 7,
            gid: 8,
            size: 9,
        }
    }

    #[test]
    fn cache_round_trip_preserves_literal_path_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(ADD_VALIDATIONS_FILENAME);
        let literal_path = b"model-\xff.bin".to_vec();
        let token = validation_token(&literal_path, b"oid", b"pointer", 0o100_644, &stat(), 9);
        let mut cache = AddValidationCache::open(&path).unwrap();

        cache.upsert(&[(literal_path.clone(), token)]).unwrap();

        assert!(cache.contains(&literal_path, &token).unwrap());
        assert!(!cache.contains(b"model.bin", &token).unwrap());
    }

    #[test]
    fn validation_token_binds_exact_stat_and_pointer() {
        let base = stat();
        let token = validation_token(b"model.bin", b"oid", b"pointer", 0o100_644, &base, 9);
        let mut changed = base;
        changed.ctime.nsecs += 1;

        assert_ne!(
            token,
            validation_token(b"model.bin", b"oid", b"pointer", 0o100_644, &changed, 9)
        );
        assert_ne!(
            token,
            validation_token(b"model.bin", b"oid", b"other", 0o100_644, &base, 9)
        );
        assert_ne!(
            token,
            validation_token(b"other.bin", b"oid", b"pointer", 0o100_644, &base, 9)
        );
    }

    #[test]
    fn cache_requires_supported_change_time() {
        let mut supported = stat();
        supported.ctime.nsecs = 4;
        let mut coarse = supported;
        coarse.ctime.nsecs = 0;

        #[cfg(unix)]
        {
            assert!(stat_is_cacheable(&supported));
            assert!(!stat_is_cacheable(&coarse));
        }
        #[cfg(not(unix))]
        {
            assert!(!stat_is_cacheable(&supported));
            assert!(!stat_is_cacheable(&coarse));
        }
    }

    #[test]
    fn cache_rejects_non_v1_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(ADD_VALIDATIONS_FILENAME);
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);

        let error = AddValidationCache::open(&path)
            .err()
            .expect("non-v1 cache must be rejected");

        assert!(error.to_string().contains("expected v1"));
    }
}
