CREATE TABLE planning_item_revisions (
 project TEXT NOT NULL,
 item TEXT NOT NULL,
 version INTEGER NOT NULL CHECK(version>0),
 PRIMARY KEY(project,item)
) STRICT, WITHOUT ROWID;
CREATE TABLE planning_history (
 project TEXT NOT NULL,
 version INTEGER NOT NULL CHECK(version>0),
 data TEXT NOT NULL CHECK(json_valid(data)),
 PRIMARY KEY(project,version)
) STRICT, WITHOUT ROWID;
