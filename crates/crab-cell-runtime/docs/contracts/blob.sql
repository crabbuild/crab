CREATE TABLE blob_uploads (
    upload_id BLOB PRIMARY KEY CHECK (length(upload_id) = 16),
    object_key BLOB NOT NULL CHECK (length(object_key) BETWEEN 1 AND 1024),
    request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
    condition INTEGER NOT NULL CHECK (condition BETWEEN 0 AND 2),
    expected_etag BLOB CHECK (expected_etag IS NULL OR length(expected_etag) = 32),
    content_type TEXT CHECK (content_type IS NULL OR length(content_type) BETWEEN 1 AND 256),
    metadata BLOB NOT NULL CHECK (length(metadata) <= 8192),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms),
    completed INTEGER NOT NULL CHECK (completed IN (0, 1)),
    etag BLOB CHECK (etag IS NULL OR length(etag) = 32),
    size INTEGER NOT NULL CHECK (size >= 0),
    part_count INTEGER NOT NULL CHECK (part_count >= 0)
) STRICT, WITHOUT ROWID;
CREATE INDEX blob_upload_expiry ON blob_uploads(expires_at_ms, upload_id);

CREATE TABLE blob_parts (
    upload_id BLOB NOT NULL REFERENCES blob_uploads(upload_id) ON DELETE CASCADE,
    part_number INTEGER NOT NULL CHECK (part_number BETWEEN 1 AND 4096),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    size INTEGER NOT NULL CHECK (size BETWEEN 0 AND 262144),
    byte_offset INTEGER CHECK (byte_offset IS NULL OR byte_offset >= 0),
    PRIMARY KEY (upload_id, part_number)
) STRICT, WITHOUT ROWID;

CREATE TABLE blob_objects (
    object_key BLOB PRIMARY KEY CHECK (length(object_key) BETWEEN 1 AND 1024),
    upload_id BLOB NOT NULL UNIQUE REFERENCES blob_uploads(upload_id),
    etag BLOB NOT NULL CHECK (length(etag) = 32),
    size INTEGER NOT NULL CHECK (size >= 0),
    part_count INTEGER NOT NULL CHECK (part_count BETWEEN 1 AND 4096),
    content_type TEXT,
    metadata BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT, WITHOUT ROWID;
