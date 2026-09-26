use crab_ltx::rusqlite::{Connection, ErrorCode};
use crab_ltx::{Db, DiskBudget, Host, Limits, TransactionError, VerifiedPlan, restore_exact};

#[test]
fn automatic_rollback_preserves_committed_cut_and_writer() {
    for (name, sql, code) in [
        (
            "full",
            "INSERT INTO payloads VALUES(2, zeroblob(131072))",
            ErrorCode::DiskFull,
        ),
        (
            "constraint",
            "INSERT OR ROLLBACK INTO payloads VALUES(1, X'00')",
            ErrorCode::ConstraintViolation,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let limits = Limits {
            max_database_bytes: 64 * 1024,
            max_capture_bytes: 128 * 1024,
            ..Limits::default()
        };
        let budget = DiskBudget::new(2 * 1024 * 1024);
        let mut db = Db::open_with_host(
            &directory.path().join("source.sqlite"),
            limits,
            Host::default().with_local_disk_budget(budget.clone()),
        )
        .unwrap();
        db.transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE payloads(id INTEGER PRIMARY KEY, value BLOB); \
             INSERT INTO payloads VALUES(1, X'01')",
            )
        })
        .unwrap();
        // Leave the prior commit uncaptured: automatic rollback must preserve
        // that pending cut while discarding every write in the failed command.
        let reserved = budget.used();
        let failed = db.transaction_with(|tx| {
            tx.execute("UPDATE payloads SET value = X'02' WHERE id = 1", [])?;
            let failure = tx.execute(sql, []).unwrap_err();
            assert!(
                tx.is_autocommit(),
                "{name} must roll back the whole transaction"
            );
            Err::<(), _>(failure)
        });
        assert!(
            matches!(failed, Err(TransactionError::Operation(ref error))
            if error.sqlite_error_code() == Some(code)),
            "{name}: {failed:?}"
        );
        assert_eq!(
            budget.used(),
            reserved,
            "{name}: rollback must refund write admission"
        );
        let first = db.capture().unwrap();
        let plan = VerifiedPlan::new(&first.segments, first.position, limits).unwrap();
        let restored = directory.path().join("before.sqlite");
        restore_exact(&plan, &restored).unwrap();
        let connection = Connection::open(restored).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT hex(value) FROM payloads", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "01",
            "{name}"
        );
        connection.close().unwrap();
        db.transaction(|tx| tx.execute("UPDATE payloads SET value = X'03' WHERE id = 1", []))
            .unwrap();
        let second = db.capture().unwrap();
        let segments = first
            .segments
            .into_iter()
            .chain(second.segments)
            .collect::<Vec<_>>();
        let plan = VerifiedPlan::new(&segments, second.position, limits).unwrap();
        let restored = directory.path().join("after.sqlite");
        restore_exact(&plan, &restored).unwrap();
        let connection = Connection::open(restored).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT hex(value) FROM payloads", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "03",
            "{name}"
        );
        connection.close().unwrap();
        db.close().unwrap();
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn callback_commit_cannot_be_mistaken_for_automatic_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Db::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
        .unwrap();
    db.capture().unwrap();
    // Transaction control violates the callback contract. Autocommit alone
    // cannot prove rollback: the WAL observer must still fence an escaped commit.
    let failed = db.transaction_with(|tx| {
        tx.execute_batch("UPDATE t SET v = 2; COMMIT")?;
        assert!(tx.is_autocommit());
        Err::<(), _>(crab_ltx::rusqlite::Error::InvalidQuery)
    });
    assert!(matches!(failed, Err(TransactionError::Sqlite(_))));
    assert!(matches!(db.capture(), Err(crab_ltx::CrabError::Fenced)));
    db.close().unwrap();
}
