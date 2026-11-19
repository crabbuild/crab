use rusqlite::{params, types::Type};
use uuid::Uuid;

use super::{Journal, map_sqlite_err};
use crate::{Result, StageState, WorkflowError};

impl Journal {
    /// Records admitted input identities atomically before execution or cache replay.
    pub(crate) fn record_admission(
        &self,
        run_id: Uuid,
        stage: &str,
        attempt: u32,
        stage_hash: &[u8; 32],
        inputs: impl Iterator<Item = (Option<String>, [u8; 32])>,
    ) -> Result<()> {
        // Each executor owns its journal connection. No transaction is held
        // across await; a failed write rolls back before any child can start.
        let transaction = self
            .conn
            .unchecked_transaction()
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        let updated = transaction
            .execute(
                "UPDATE stage_runs SET stage_hash = ?4
                 WHERE run_id = ?1 AND stage_name = ?2 AND attempt = ?3 AND state = ?5",
                params![
                    run_id.to_string(),
                    stage,
                    i64::from(attempt),
                    stage_hash.as_slice(),
                    i64::from(StageState::CacheChecked.sql_tag())
                ],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        if updated != 1 {
            return Err(WorkflowError::WorkflowJournalCorrupt {
                run_id: run_id.to_string(),
                detail: "admission requires a cache-checked stage attempt".into(),
            });
        }
        transaction
            .execute(
                "DELETE FROM stage_inputs WHERE run_id = ?1 AND stage_name = ?2 AND attempt = ?3",
                params![run_id.to_string(), stage, i64::from(attempt)],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO stage_inputs(run_id, stage_name, attempt, input_index, input_path, hash)
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .map_err(|source| map_sqlite_err(&self.path, source))?;
            for (index, (path, hash)) in inputs.enumerate() {
                let index = i64::try_from(index).map_err(|_| WorkflowError::Configuration {
                    key: "workflow stage input count".into(),
                    origin: "input count exceeds SQLite limits".into(),
                })?;
                insert
                    .execute(params![
                        run_id.to_string(),
                        stage,
                        i64::from(attempt),
                        index,
                        path,
                        hash.as_slice()
                    ])
                    .map_err(|source| map_sqlite_err(&self.path, source))?;
            }
        }
        transaction
            .commit()
            .map_err(|source| map_sqlite_err(&self.path, source))
    }

    /// Reads a bounded set of admitted inputs for one attempt.
    pub(crate) fn admitted_inputs(
        &self,
        run_id: Uuid,
        stage: &str,
        attempt: u32,
        max_inputs: usize,
    ) -> Result<Vec<(Option<String>, [u8; 32])>> {
        let limit = i64::try_from(max_inputs.saturating_add(1)).map_err(|_| {
            WorkflowError::Configuration {
                key: "workflow journal input limit".into(),
                origin: "limit exceeds SQLite limits".into(),
            }
        })?;
        let mut statement = self.conn.prepare(
            "SELECT input_path IS NULL OR length(CAST(input_path AS BLOB)) <= 4096, input_path, hash
             FROM stage_inputs WHERE run_id = ?1 AND stage_name = ?2 AND attempt = ?3
             ORDER BY input_index LIMIT ?4",
        ).map_err(|source| map_sqlite_err(&self.path, source))?;
        let rows = statement
            .query_map(
                params![run_id.to_string(), stage, i64::from(attempt), limit],
                |row| {
                    if !row.get::<_, bool>(0)? {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            1,
                            Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "admitted input path exceeds 4096 bytes",
                            )),
                        ));
                    }
                    let bytes: Vec<u8> = row.get(2)?;
                    let hash = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|source| {
                        rusqlite::Error::FromSqlConversionFailure(2, Type::Blob, Box::new(source))
                    })?;
                    Ok((row.get(1)?, hash))
                },
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        let mut inputs = Vec::with_capacity(max_inputs.min(1024));
        for row in rows {
            if inputs.len() == max_inputs {
                return Err(WorkflowError::Configuration {
                    key: "workflow journal input limit".into(),
                    origin: "admitted input count exceeds the requested bound".into(),
                });
            }
            inputs.push(row.map_err(|source| map_sqlite_err(&self.path, source))?);
        }
        Ok(inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> (tempfile::TempDir, Journal, Uuid) {
        let directory = tempfile::tempdir().unwrap();
        let journal = Journal::open(&directory.path().join("journal.db")).unwrap();
        let id = Uuid::now_v7();
        journal.insert_run_start(id, "test", "host").unwrap();
        journal.insert_stage_start(id, "train").unwrap();
        journal
            .transition(id, "train", 1, StageState::Resolved, "{}")
            .unwrap();
        journal
            .transition(id, "train", 1, StageState::CacheChecked, "{}")
            .unwrap();
        (directory, journal, id)
    }

    #[test]
    fn admission_failure_rolls_back_hash_and_input_replacement() {
        let (_directory, journal, id) = prepared();
        let original = vec![(Some("dataset".into()), [1; 32])];
        journal
            .record_admission(id, "train", 1, &[2; 32], original.clone().into_iter())
            .unwrap();
        journal
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_admission BEFORE INSERT ON stage_inputs
            WHEN NEW.input_path = 'reject' BEGIN SELECT RAISE(ABORT, 'rejected test input'); END;",
            )
            .unwrap();
        let next = vec![
            (Some("changed".into()), [3; 32]),
            (Some("reject".into()), [4; 32]),
        ];
        assert!(
            journal
                .record_admission(id, "train", 1, &[5; 32], next.into_iter())
                .is_err()
        );
        assert_eq!(
            (
                journal
                    .latest_stage_row(id, "train")
                    .unwrap()
                    .unwrap()
                    .stage_hash,
                journal.admitted_inputs(id, "train", 1, 2).unwrap()
            ),
            (Some([2; 32]), original)
        );
    }

    #[test]
    fn replacing_a_stage_attempt_removes_its_admission() {
        let (_directory, journal, id) = prepared();
        journal
            .record_admission(id, "train", 1, &[2; 32], [(None, [1; 32])].into_iter())
            .unwrap();
        journal.insert_stage_start(id, "train").unwrap();
        assert_eq!(
            (
                journal
                    .latest_stage_row(id, "train")
                    .unwrap()
                    .unwrap()
                    .stage_hash,
                journal.admitted_inputs(id, "train", 1, 1).unwrap()
            ),
            (None, Vec::new())
        );
    }

    #[test]
    fn opening_an_existing_v1_journal_adds_observations_without_changing_stage_rows() {
        let (directory, journal, id) = prepared();
        let original = journal.latest_stage_row(id, "train").unwrap().unwrap();
        journal
            .conn
            .execute_batch("DROP TABLE stage_inputs;")
            .unwrap();
        drop(journal);
        let journal = Journal::open(&directory.path().join("journal.db")).unwrap();
        let restored = journal.latest_stage_row(id, "train").unwrap().unwrap();
        assert_eq!(
            (
                journal.schema_version().unwrap(),
                restored.stage_name,
                restored.state,
                restored.payload_json
            ),
            (
                1,
                original.stage_name,
                original.state,
                original.payload_json
            )
        );
        assert!(
            journal
                .admitted_inputs(id, "train", 1, 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn admission_reads_reject_excess_rows_and_locations_before_materialization() {
        let (_directory, journal, id) = prepared();
        journal
            .record_admission(
                id,
                "train",
                1,
                &[1; 32],
                [(None, [2; 32]), (None, [3; 32])].into_iter(),
            )
            .unwrap();
        assert!(journal.admitted_inputs(id, "train", 1, 1).is_err());
        journal
            .record_admission(
                id,
                "train",
                1,
                &[1; 32],
                [(Some("x".repeat(4097)), [2; 32])].into_iter(),
            )
            .unwrap();
        assert!(journal.admitted_inputs(id, "train", 1, 2).is_err());
    }
}
