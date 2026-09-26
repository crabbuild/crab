CREATE TABLE ddb_transaction_payloads (
    transaction_id BLOB NOT NULL,
    position INTEGER NOT NULL,
    chunk INTEGER NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (transaction_id, position, chunk)
) WITHOUT ROWID;
