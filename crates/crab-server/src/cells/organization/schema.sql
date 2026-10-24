CREATE TABLE organization_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    id BLOB NOT NULL CHECK (length(id) = 16),
    format INTEGER NOT NULL CHECK (format = 1),
    app_revision INTEGER NOT NULL DEFAULT 0 CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;
CREATE TABLE organization_details (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT;
CREATE TABLE organization_members (
    actor BLOB PRIMARY KEY CHECK (length(actor) = 16),
    role TEXT NOT NULL CHECK (role IN ('member', 'owner')),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    version INTEGER NOT NULL CHECK (version BETWEEN 1 AND 9007199254740991)
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_active_members ON organization_members(active, actor);
CREATE INDEX organization_owners ON organization_members(role, active, actor);
CREATE TABLE organization_invitations (
    recipient BLOB PRIMARY KEY CHECK (length(recipient) = 16),
    role TEXT NOT NULL CHECK (role IN ('member', 'owner')),
    version INTEGER NOT NULL CHECK (version BETWEEN 1 AND 9007199254740991),
    member_version INTEGER NOT NULL CHECK (member_version BETWEEN 0 AND 9007199254740991),
    state TEXT NOT NULL CHECK (state IN ('pending', 'accepted', 'declined', 'cancelled')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms)
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_pending_invitations ON organization_invitations(state, recipient);
CREATE TABLE organization_bootstrap (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    actor BLOB NOT NULL CHECK (length(actor) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32)
) STRICT;
CREATE TABLE organization_command_receipts (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    expires_at_ms INTEGER NOT NULL,
    reply BLOB NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_command_receipts_expiry ON organization_command_receipts(expires_at_ms);
