/// Result of a Cell runtime contract operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Identity, control-codec and schema failures with original causes retained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An identity, digest, or partition argument violates its encoding rules.
    #[error("invalid Cell identity: {0}")]
    Identity(&'static str),
    /// A control record or transition failed validation.
    #[error("invalid Cell control record: {0}")]
    Control(&'static str),
    /// A catalog record, scan, or head failed validation.
    #[error("invalid Cell catalog: {0}")]
    Catalog(&'static str),
    /// A compiled registry rejected a descriptor, binding, or release.
    #[error("invalid compiled Cell registry: {0}")]
    Registry(&'static str),
    /// A Cell application release failed validation.
    #[error("invalid Cell application release: {0}")]
    Release(&'static str),
    /// A backup pin failed validation.
    #[error("invalid Cell backup pin: {0}")]
    Backup(&'static str),
    /// A retention request failed validation.
    #[error("invalid Cell retention operation: {0}")]
    Retention(&'static str),
    /// Retention scratch storage failed locally.
    #[error("Cell retention scratch storage failed")]
    RetentionIo(#[source] std::io::Error),
    /// The retention scratch worker could not be joined.
    #[error("failed to join Cell retention scratch worker")]
    RetentionWorkerJoin(#[source] tokio::task::JoinError),
    /// The Cell ID already names a different catalog entry.
    #[error("Cell ID collides with a different catalog entry")]
    CatalogCollision,
    /// The catalog shard cannot accept another entry.
    #[error("Cell catalog shard reached its 65,536-entry limit")]
    CatalogFull,
    /// A persisted JSON document failed to encode or decode.
    #[error("Cell runtime JSON failed")]
    Json(#[from] serde_json::Error),
    /// The wire codec rejected a value.
    #[error("Cell wire codec failed")]
    Codec(#[from] crate::codec::CodecError),
    /// A peer request or reply violated the peer protocol.
    #[error("invalid Cell peer protocol: {0}")]
    Peer(&'static str),
    /// A peer Protobuf message failed to decode.
    #[error("Cell peer Protobuf decoding failed")]
    PeerDecode(#[from] prost::DecodeError),
    /// A peer transport failed before the operation was accepted.
    #[error("Cell peer transport failed: {context}")]
    PeerTransport {
        /// Facility that failed, for logs and peer replies.
        context: &'static str,
        /// Transport or facility failure that produced this error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The peer transport failed after it may have accepted the operation, so
    /// the caller must resolve the request before retrying it.
    #[error("Cell peer transport may have accepted the operation: {context}")]
    PeerTransportUnknown {
        /// Facility that failed, for logs and peer replies.
        context: &'static str,
        /// Transport or facility failure that produced this error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A peer signature did not verify against the advertised key.
    #[error("Cell peer signature verification failed")]
    PeerSignature(#[source] ed25519_dalek::SignatureError),
    /// The peer is authenticated but not authorized for this operation.
    #[error("Cell peer authorization denied: {0}")]
    PeerAuthorization(&'static str),
    /// A node advertisement or directory record failed validation.
    #[error("invalid Cell node advertisement: {0}")]
    Node(&'static str),
    /// Follower storage failed locally.
    #[error("Cell follower storage failed")]
    FollowerIo(#[from] std::io::Error),
    /// The follower storage worker could not be joined.
    #[error("failed to join Cell follower storage worker")]
    FollowerWorkerJoin(#[source] tokio::task::JoinError),
    /// SQLite rejected a statement, schema, or transaction operation.
    #[error("Cell runtime SQLite schema failed")]
    Sqlite(#[from] rusqlite::Error),
    /// SQL text returned by the engine was not valid UTF-8.
    #[error("Cell SQL returned invalid UTF-8 text")]
    Utf8(#[from] std::str::Utf8Error),
    /// Authoritative control storage failed.
    #[error("Cell authority storage failed")]
    Storage(#[from] crab_storage::StorageError),
    /// LTX capture, verification, or publication failed.
    #[error("Cell LTX publication failed")]
    Ltx(#[from] crab_ltx::CrabError),
    /// A command failed validation or its handler refused it.
    #[error("invalid Cell command: {0}")]
    Command(&'static str),
    /// A read replica has not reached the requested Cell position.
    #[error("Cell read replica is behind the requested receipt")]
    ReplicaBehind {
        /// Commit sequence the replica can currently serve.
        observed_sequence: u64,
        /// Minimum commit sequence required by the caller.
        minimum_sequence: u64,
    },
    /// The request ID was already used with different command bytes.
    #[error("request ID was already used for different command bytes")]
    RequestConflict,
    /// The Cell holds a local commit that must be published durably first.
    #[error("Cell has a local commit awaiting durable publication")]
    PendingPublication,
    /// The executor is fenced until origin recovery completes.
    #[error("Cell executor is fenced pending origin recovery")]
    Fenced,
    /// The runtime is shutting down and refuses new work.
    #[error("Cell runtime worker pool is closed")]
    RuntimeClosed,
    /// The state stream was cancelled before it finished.
    #[error("Cell state stream was cancelled")]
    StreamCancelled,
    /// The Cell is not active on the worker that received the request.
    #[error("Cell is not active on its assigned worker")]
    CellNotActive,
    /// The Cell is already active on this worker.
    #[error("Cell is already active on its assigned worker")]
    CellAlreadyActive,
    /// The Cell is draining and refuses new commands.
    #[error("Cell is draining and no longer accepts commands")]
    CellDraining,
    /// A SQLite operation exceeded its wall-clock deadline.
    #[error("Cell SQLite operation exceeded its wall deadline")]
    Deadline,
    /// An accepted command's outcome is unknown; the caller must resolve the
    /// request ID before retrying it.
    #[error("accepted Cell command outcome is unknown")]
    OutcomeUnknown {
        /// Request whose outcome must be resolved.
        request_id: crate::identity::RequestId,
        /// Digest of the command bytes the request carried.
        operation_digest: crate::Digest,
        /// Failure observed while resolving the outcome.
        #[source]
        source: Box<Error>,
    },
    /// An accepted effect's outcome is unknown; the caller must resolve the
    /// effect ID before retrying it.
    #[error("accepted Cell effect outcome is unknown")]
    EffectOutcomeUnknown {
        /// Effect whose outcome must be resolved.
        effect_id: [u8; 32],
        /// Digest of the operation the effect carried.
        operation_digest: crate::Digest,
        /// Failure observed while resolving the outcome.
        #[source]
        source: Box<Error>,
    },
    /// The effect expired before it could be delivered.
    #[error("Cell effect expired before delivery")]
    EffectExpired,
    /// A bounded runtime resource is exhausted; the message names it.
    #[error("Cell runtime capacity exhausted: {0}")]
    Capacity(&'static str),
    /// The SQL worker could not be started.
    #[error("failed to start Cell SQL worker")]
    WorkerStart(#[source] Box<std::io::Error>),
    /// The SQL worker supervisor could not be joined.
    #[error("failed to join Cell SQL worker supervisor")]
    WorkerJoin(#[source] tokio::task::JoinError),
    /// A SQL worker panicked during shutdown.
    #[error("a Cell SQL worker panicked during shutdown")]
    WorkerPanic,
    /// A native Cell callback panicked; its activation was fenced.
    #[error("a native Cell callback panicked; its activation was fenced")]
    NativePanic,
    /// The blocking activity worker could not be started.
    #[error("failed to start Cell blocking activity worker")]
    ActivityWorkerStart(#[source] Box<std::io::Error>),
    /// The blocking activity worker supervisor could not be joined.
    #[error("failed to join Cell blocking activity worker supervisor")]
    ActivityWorkerJoin(#[source] tokio::task::JoinError),
    /// A blocking activity worker panicked during shutdown.
    #[error("a Cell blocking activity worker panicked during shutdown")]
    ActivityWorkerPanic,
    /// A native blocking-activity handler panicked.
    #[error("a native Cell blocking activity handler panicked")]
    ActivityPanic,
    /// The runtime was started outside an active Tokio runtime.
    #[error("Cell runtime requires an active Tokio runtime")]
    RuntimeStart(#[source] tokio::runtime::TryCurrentError),
    /// A provider-owned node facility failed.
    #[error("Cell node facility failed: {name}")]
    Facility {
        /// Facility that failed, for logs and peer replies.
        name: &'static str,
        /// Transport or facility failure that produced this error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
