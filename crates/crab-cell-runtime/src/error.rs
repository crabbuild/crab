/// Result of a Cell runtime contract operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Identity, control-codec and schema failures with original causes retained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Cell identity: {0}")]
    Identity(&'static str),
    #[error("invalid Cell control record: {0}")]
    Control(&'static str),
    #[error("invalid Cell catalog: {0}")]
    Catalog(&'static str),
    #[error("invalid compiled Cell registry: {0}")]
    Registry(&'static str),
    #[error("invalid Cell application release: {0}")]
    Release(&'static str),
    #[error("invalid Cell backup pin: {0}")]
    Backup(&'static str),
    #[error("invalid Cell retention operation: {0}")]
    Retention(&'static str),
    #[error("Cell retention scratch storage failed")]
    RetentionIo(#[source] std::io::Error),
    #[error("failed to join Cell retention scratch worker")]
    RetentionWorkerJoin(#[source] tokio::task::JoinError),
    #[error("Cell ID collides with a different catalog entry")]
    CatalogCollision,
    #[error("Cell catalog shard reached its 65,536-entry limit")]
    CatalogFull,
    #[error("Cell runtime JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("Cell wire codec failed")]
    Codec(#[from] crate::codec::CodecError),
    #[error("invalid Cell peer protocol: {0}")]
    Peer(&'static str),
    #[error("Cell peer Protobuf decoding failed")]
    PeerDecode(#[from] prost::DecodeError),
    #[error("Cell peer transport failed: {context}")]
    PeerTransport {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Cell peer transport may have accepted the operation: {context}")]
    PeerTransportUnknown {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Cell peer signature verification failed")]
    PeerSignature(#[source] ed25519_dalek::SignatureError),
    #[error("Cell peer authorization denied: {0}")]
    PeerAuthorization(&'static str),
    #[error("invalid Cell node advertisement: {0}")]
    Node(&'static str),
    #[error("Cell follower storage failed")]
    FollowerIo(#[from] std::io::Error),
    #[error("failed to join Cell follower storage worker")]
    FollowerWorkerJoin(#[source] tokio::task::JoinError),
    #[error("Cell runtime SQLite schema failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Cell SQL returned invalid UTF-8 text")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Cell authority storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("Cell LTX publication failed")]
    Ltx(#[from] crab_ltx::CrabError),
    #[error("invalid Cell command: {0}")]
    Command(&'static str),
    #[error("request ID was already used for different command bytes")]
    RequestConflict,
    #[error("Cell has a local commit awaiting durable publication")]
    PendingPublication,
    #[error("Cell executor is fenced pending origin recovery")]
    Fenced,
    #[error("Cell runtime worker pool is closed")]
    RuntimeClosed,
    #[error("Cell state stream was cancelled")]
    StreamCancelled,
    #[error("Cell is not active on its assigned worker")]
    CellNotActive,
    #[error("Cell is already active on its assigned worker")]
    CellAlreadyActive,
    #[error("Cell is draining and no longer accepts commands")]
    CellDraining,
    #[error("Cell SQLite operation exceeded its wall deadline")]
    Deadline,
    #[error("accepted Cell command outcome is unknown")]
    OutcomeUnknown {
        request_id: crate::identity::RequestId,
        operation_digest: crate::Digest,
        #[source]
        source: Box<Error>,
    },
    #[error("accepted Cell effect outcome is unknown")]
    EffectOutcomeUnknown {
        effect_id: [u8; 32],
        operation_digest: crate::Digest,
        #[source]
        source: Box<Error>,
    },
    #[error("Cell effect expired before delivery")]
    EffectExpired,
    #[error("Cell runtime capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("failed to start Cell SQL worker")]
    WorkerStart(#[source] Box<std::io::Error>),
    #[error("failed to join Cell SQL worker supervisor")]
    WorkerJoin(#[source] tokio::task::JoinError),
    #[error("a Cell SQL worker panicked during shutdown")]
    WorkerPanic,
    #[error("a native Cell callback panicked; its activation was fenced")]
    NativePanic,
    #[error("failed to start Cell blocking activity worker")]
    ActivityWorkerStart(#[source] Box<std::io::Error>),
    #[error("failed to join Cell blocking activity worker supervisor")]
    ActivityWorkerJoin(#[source] tokio::task::JoinError),
    #[error("a Cell blocking activity worker panicked during shutdown")]
    ActivityWorkerPanic,
    #[error("a native Cell blocking activity handler panicked")]
    ActivityPanic,
    #[error("Cell runtime requires an active Tokio runtime")]
    RuntimeStart(#[source] tokio::runtime::TryCurrentError),
    #[error("Cell node facility failed: {name}")]
    Facility {
        name: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
