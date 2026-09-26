CREATE TABLE capacity_reservations (
    reservation_key BLOB PRIMARY KEY CHECK (length(reservation_key) BETWEEN 1 AND 128),
    pages INTEGER NOT NULL CHECK (pages > 0)
) STRICT, WITHOUT ROWID;
CREATE TABLE capacity_total (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    pages INTEGER NOT NULL CHECK (pages >= 0)
) STRICT;
INSERT INTO capacity_total(singleton, pages) VALUES (1, 0);
