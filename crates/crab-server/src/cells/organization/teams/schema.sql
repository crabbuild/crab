CREATE TABLE organization_team_settings (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    member_creation INTEGER NOT NULL CHECK(member_creation IN (0,1)),
    version INTEGER NOT NULL CHECK(version BETWEEN 1 AND 9007199254740991)
) STRICT;
INSERT INTO organization_team_settings VALUES(1,1,1);
CREATE TABLE organization_membership_epochs (
    actor BLOB PRIMARY KEY REFERENCES organization_members(actor),
    epoch INTEGER NOT NULL CHECK(epoch BETWEEN 1 AND 9007199254740991)
) STRICT, WITHOUT ROWID;
CREATE TABLE organization_teams (
    id BLOB PRIMARY KEY CHECK(length(id)=16),
    name TEXT NOT NULL,
    name_key TEXT NOT NULL,
    description TEXT NOT NULL,
    visibility TEXT NOT NULL CHECK(visibility IN ('closed','secret')),
    version INTEGER NOT NULL CHECK(version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms>=created_at_ms)
) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX organization_team_names ON organization_teams(name_key) WHERE deleted_at_ms IS NULL;
CREATE INDEX organization_live_teams ON organization_teams(deleted_at_ms,id);
CREATE TABLE organization_team_members (
    team BLOB NOT NULL REFERENCES organization_teams(id),
    actor BLOB NOT NULL REFERENCES organization_members(actor),
    membership_epoch INTEGER NOT NULL CHECK(membership_epoch BETWEEN 1 AND 9007199254740991),
    role TEXT CHECK(role IS NULL OR role IN ('member','maintainer')),
    version INTEGER NOT NULL CHECK(version BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY(team,actor)
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_actor_teams ON organization_team_members(actor,team);
CREATE TABLE organization_team_submissions (
    request_id BLOB PRIMARY KEY CHECK(length(request_id)=16),
    actor BLOB NOT NULL CHECK(length(actor)=16),
    digest BLOB NOT NULL CHECK(length(digest)=32),
    outcome BLOB NOT NULL
) STRICT, WITHOUT ROWID;
