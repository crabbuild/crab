use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use prost::Message;

use crate::{
    ApplicationId, CellTarget, Digest, Error, IncarnationId, NamespaceId, Result, SessionId,
    TenantId,
};

mod dispatch;
mod protobuf;
mod transport;

pub use dispatch::{PeerAuthorizer, PeerCellResolver, PeerDispatcher};
pub(crate) use transport::PeerClientTransport;
pub use transport::PeerRoundTrip;
pub use transport::{EffectPeerClient, MigrationPeerClient};

use protobuf::{
    MessageKind, field_payload, oneof_payload, require_fields, validate_message, validate_operation,
};

const PROTOCOL_VERSION: u32 = 1;
const MAX_AUTHORIZATION_BYTES: usize = 16 * 1024;
const MAX_OPERATION_BYTES: usize = 1024 * 1024;
pub const MAX_PEER_REQUEST_BYTES: usize = MAX_AUTHORIZATION_BYTES + MAX_OPERATION_BYTES + 128;
const MAX_ACTIONS: usize = 128;
const MAX_PRINCIPAL_BYTES: usize = 512;
const MAX_AUTH_LIFETIME_MS: i64 = 60_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const MAX_MUTATION_LIFETIME_MS: i64 = 24 * 60 * 60_000;
const MAX_EFFECT_LIFETIME_MS: i64 = 7 * 24 * 60 * 60_000;

/// Generated private peer messages. They are not a public service or application API.
pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/crab.cell.peer.v1.rs"));
}

/// One peer operation currently executable by the typed Cell client.
#[derive(Clone)]
pub enum PeerOperation {
    Mutate(wire::MutationRequest),
    Read(wire::ReadRequest),
    Resolve(wire::ResolveRequest),
    DeliverEffect(wire::EffectRequest),
    ResolveEffect(wire::EffectResolveRequest),
    Migrate(wire::MigrationRequest),
}

impl PeerOperation {
    fn tag(&self) -> u32 {
        match self {
            Self::Mutate(_) => 10,
            Self::Read(_) => 11,
            Self::Resolve(_) => 12,
            Self::DeliverEffect(_) => 13,
            Self::ResolveEffect(_) => 14,
            Self::Migrate(_) => 15,
        }
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Mutate(value) => value.encode_to_vec(),
            Self::Read(value) => value.encode_to_vec(),
            Self::Resolve(value) => value.encode_to_vec(),
            Self::DeliverEffect(value) => value.encode_to_vec(),
            Self::ResolveEffect(value) => value.encode_to_vec(),
            Self::Migrate(value) => value.encode_to_vec(),
        }
    }

    fn validate(&self, now_ms: i64) -> Result<()> {
        match self {
            Self::Mutate(value) => validate_mutation(value, now_ms),
            Self::Read(value) => validate_read(value),
            Self::Resolve(value) => validate_resolve(value, now_ms),
            Self::DeliverEffect(value) => validate_effect(value, now_ms),
            Self::ResolveEffect(value) => validate_effect_resolve(value, now_ms),
            Self::Migrate(value) => validate_migration(value),
        }
    }
}

/// Original authorized principal delegated across one private peer hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPrincipal {
    pub issuer: String,
    pub subject: String,
    pub actions: Vec<String>,
}

/// Boot-session signer used only after node enrollment has bound its public key.
pub struct PeerSigner {
    session: SessionId,
    release: Digest,
    key: SigningKey,
}

impl PeerSigner {
    #[must_use]
    pub fn new(session: SessionId, release: Digest, key: SigningKey) -> Self {
        Self {
            session,
            release,
            key,
        }
    }

    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Encodes and signs one bounded private request without changing its operation bytes.
    pub fn sign(
        &self,
        principal: PeerPrincipal,
        issued_at_ms: i64,
        expires_at_ms: i64,
        remaining_ms: u32,
        operation: PeerOperation,
    ) -> Result<Vec<u8>> {
        validate_principal(&principal)?;
        validate_time_bounds(issued_at_ms, expires_at_ms, issued_at_ms, remaining_ms)?;
        operation.validate(issued_at_ms)?;
        let tag = operation.tag();
        let payload = operation.encode();
        if payload.len() > MAX_OPERATION_BYTES {
            return Err(Error::Peer("operation exceeds one MiB"));
        }
        validate_operation(tag, &payload)?;
        let payload_digest = blake3::hash(&payload);
        let mut authorization = wire::PeerAuthorization {
            origin_session: self.session.as_bytes().to_vec(),
            principal_issuer: principal.issuer,
            principal_subject: principal.subject,
            actions: principal.actions,
            release_digest: self.release.as_bytes().to_vec(),
            issued_at_ms,
            expires_at_ms,
            payload_digest: payload_digest.as_bytes().to_vec(),
            signature: Vec::new(),
        };
        let signing_bytes = signing_bytes(tag, &authorization)?;
        authorization.signature = self.key.sign(&signing_bytes).to_bytes().to_vec();
        encode_request(authorization, 1, remaining_ms, tag, &payload)
    }
}

/// Enrollment-bound verifier for one currently advertised node session.
pub struct PeerVerifier {
    session: SessionId,
    release: Digest,
    key: VerifyingKey,
}

impl PeerVerifier {
    #[must_use]
    pub fn new(session: SessionId, release: Digest, key: VerifyingKey) -> Self {
        Self {
            session,
            release,
            key,
        }
    }

    /// Strictly decodes and authenticates one request before actor admission.
    pub fn verify(&self, input: &[u8], now_ms: i64) -> Result<VerifiedPeerRequest> {
        if input.len() > MAX_PEER_REQUEST_BYTES {
            return Err(Error::Peer("request exceeds peer byte limit"));
        }
        let fields = validate_message(input, MessageKind::PeerRequest)?;
        require_fields(&fields, &[1, 2, 3, 4])?;
        let (tag, payload) = oneof_payload(input, &fields, &[10, 11, 12, 13, 14, 15])?;
        if payload.len() > MAX_OPERATION_BYTES {
            return Err(Error::Peer("operation exceeds one MiB"));
        }
        validate_operation(tag, payload)?;
        let request = wire::PeerRequest::decode(input)?;
        if request.version != PROTOCOL_VERSION
            || !(1..=2).contains(&request.hop_count)
            || !(1..=60_000).contains(&request.remaining_ms)
        {
            return Err(Error::Peer("unsupported peer version, hop, or deadline"));
        }
        validate_decoded_operation(request.operation.as_ref(), now_ms)?;
        let authorization = request
            .authorization
            .as_ref()
            .ok_or(Error::Peer("authorization is missing"))?;
        let authorization_bytes = field_payload(&fields, 2)?;
        if authorization_bytes.len() > MAX_AUTHORIZATION_BYTES {
            return Err(Error::Peer("authorization exceeds 16 KiB"));
        }
        validate_authorization(authorization, now_ms, request.remaining_ms)?;
        if authorization.origin_session.as_slice() != self.session.as_bytes()
            || authorization.release_digest.as_slice() != self.release.as_bytes()
        {
            return Err(Error::Peer("peer enrollment or release does not match"));
        }
        let expected_digest = blake3::hash(payload);
        if authorization.payload_digest.as_slice() != expected_digest.as_bytes() {
            return Err(Error::Peer("peer payload digest does not match"));
        }
        let signature =
            Signature::from_slice(&authorization.signature).map_err(Error::PeerSignature)?;
        self.key
            .verify_strict(&signing_bytes(tag, authorization)?, &signature)
            .map_err(Error::PeerSignature)?;
        let target = operation_target(request.operation.as_ref())?;
        let principal = PeerPrincipal {
            issuer: authorization.principal_issuer.clone(),
            subject: authorization.principal_subject.clone(),
            actions: authorization.actions.clone(),
        };
        Ok(VerifiedPeerRequest {
            request,
            target,
            principal,
            origin_session: self.session,
            operation_tag: tag,
            operation_bytes: payload.to_vec(),
        })
    }
}

/// Reads the untrusted session claim only after strict structural validation.
///
/// Callers use this value solely to locate an enrollment key. The returned
/// session is not authenticated until [`PeerVerifier::verify`] succeeds.
pub fn claimed_peer_session(input: &[u8]) -> Result<SessionId> {
    if input.len() > MAX_PEER_REQUEST_BYTES {
        return Err(Error::Peer("request exceeds peer byte limit"));
    }
    let fields = validate_message(input, MessageKind::PeerRequest)?;
    require_fields(&fields, &[1, 2, 3, 4])?;
    let authorization_range = field_payload(&fields, 2)?;
    if authorization_range.len() > MAX_AUTHORIZATION_BYTES {
        return Err(Error::Peer("authorization exceeds 16 KiB"));
    }
    let authorization_fields = validate_message(
        &input[authorization_range.clone()],
        MessageKind::Authorization,
    )?;
    require_fields(&authorization_fields, &[1, 2, 3, 5, 6, 7, 8, 9])?;
    let authorization = wire::PeerAuthorization::decode(&input[authorization_range])?;
    SessionId::try_from(authorization.origin_session.as_slice())
}

/// Authenticated request retaining the exact signed nested operation bytes.
pub struct VerifiedPeerRequest {
    request: wire::PeerRequest,
    target: CellTarget,
    principal: PeerPrincipal,
    origin_session: SessionId,
    operation_tag: u32,
    operation_bytes: Vec<u8>,
}

impl VerifiedPeerRequest {
    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }

    #[must_use]
    pub const fn principal(&self) -> &PeerPrincipal {
        &self.principal
    }

    #[must_use]
    pub const fn origin_session(&self) -> SessionId {
        self.origin_session
    }

    #[must_use]
    pub const fn hop_count(&self) -> u32 {
        self.request.hop_count
    }

    #[must_use]
    pub const fn remaining_ms(&self) -> u32 {
        self.request.remaining_ms
    }

    #[must_use]
    pub const fn operation_tag(&self) -> u32 {
        self.operation_tag
    }

    #[must_use]
    pub fn operation(&self) -> Option<&wire::peer_request::Operation> {
        self.request.operation.as_ref()
    }

    #[must_use]
    pub fn permits(&self, action: &str) -> bool {
        self.principal
            .actions
            .binary_search_by(|candidate| candidate.as_str().cmp(action))
            .is_ok()
    }

    /// Preserves signed payload bytes while reducing the deadline for one final hop.
    pub fn forward(&self, remaining_ms: u32) -> Result<Vec<u8>> {
        if self.request.hop_count >= 2 {
            return Err(Error::Peer("peer hop limit reached"));
        }
        if remaining_ms == 0 || remaining_ms > self.request.remaining_ms {
            return Err(Error::Peer("forwarding extended or exhausted the deadline"));
        }
        let authorization = self
            .request
            .authorization
            .clone()
            .ok_or(Error::Peer("authorization is missing"))?;
        encode_request(
            authorization,
            self.request.hop_count + 1,
            remaining_ms,
            self.operation_tag,
            &self.operation_bytes,
        )
    }
}

/// Encodes one bounded canonical reply produced by the private dispatcher.
pub fn encode_peer_reply(reply: &wire::PeerReply) -> Result<Vec<u8>> {
    validate_reply(reply)?;
    let encoded = reply.encode_to_vec();
    if encoded.len() > MAX_PEER_REQUEST_BYTES {
        return Err(Error::Peer("reply exceeds peer byte limit"));
    }
    Ok(encoded)
}

/// Strictly decodes a reply without allowing Prost to discard unknown fields.
pub fn decode_peer_reply(input: &[u8]) -> Result<wire::PeerReply> {
    if input.len() > MAX_PEER_REQUEST_BYTES {
        return Err(Error::Peer("reply exceeds peer byte limit"));
    }
    let fields = validate_message(input, MessageKind::PeerReply)?;
    if !fields.iter().any(|field| matches!(field.tag(), 1..=5)) {
        return Err(Error::Peer("peer reply outcome is missing"));
    }
    let reply = wire::PeerReply::decode(input)?;
    validate_reply(&reply)?;
    Ok(reply)
}

fn validate_reply(reply: &wire::PeerReply) -> Result<()> {
    match reply.outcome.as_ref() {
        Some(wire::peer_reply::Outcome::Mutation(reply)) => validate_mutation_reply(reply),
        Some(wire::peer_reply::Outcome::Read(reply)) => validate_read_reply(reply),
        Some(wire::peer_reply::Outcome::Resolve(reply)) => validate_resolve_reply(reply),
        Some(wire::peer_reply::Outcome::Error(error)) => validate_error(error),
        Some(wire::peer_reply::Outcome::Migration(reply)) => validate_description_wire(
            reply
                .description
                .as_ref()
                .ok_or(Error::Peer("migration reply description is missing"))?,
        ),
        None => Err(Error::Peer("peer reply outcome is missing")),
    }
}

fn validate_mutation_reply(reply: &wire::MutationReply) -> Result<()> {
    validate_receipt(
        reply
            .receipt
            .as_ref()
            .ok_or(Error::Peer("mutation reply receipt is missing"))?,
    )?;
    match reply.outcome.as_ref() {
        Some(wire::mutation_reply::Outcome::Result(result)) => match result.result.as_ref() {
            Some(wire::mutation_result::Result::CommandOutput(output))
                if output.len() <= MAX_OPERATION_BYTES =>
            {
                Ok(())
            }
            Some(wire::mutation_result::Result::CommandOutput(_)) => {
                Err(Error::Peer("mutation result exceeds one MiB"))
            }
            None => Err(Error::Peer("mutation result is missing")),
        },
        Some(wire::mutation_reply::Outcome::Error(error)) => validate_error(error),
        None => Err(Error::Peer("mutation reply outcome is missing")),
    }
}

fn validate_read_reply(reply: &wire::ReadReply) -> Result<()> {
    if let Some(receipt) = &reply.receipt {
        validate_receipt(receipt)?;
    }
    match reply.result.as_ref() {
        Some(wire::read_reply::Result::Description(description)) => {
            if reply.receipt.is_some()
                || description.cell_id.len() != 32
                || description.incarnation.len() != 16
                || description.code.len() != 32
                || description.schema == 0
            {
                return Err(Error::Peer("invalid peer Cell description"));
            }
            Ok(())
        }
        Some(wire::read_reply::Result::CommandOutput(output)) => {
            validate_receipt(
                reply
                    .receipt
                    .as_ref()
                    .ok_or(Error::Peer("read reply receipt is missing"))?,
            )?;
            if output.len() > MAX_OPERATION_BYTES {
                return Err(Error::Peer("read result exceeds one MiB"));
            }
            Ok(())
        }
        Some(wire::read_reply::Result::Error(error)) => validate_error(error),
        None => Err(Error::Peer("read reply result is missing")),
    }
}

fn validate_resolve_reply(reply: &wire::ResolveReply) -> Result<()> {
    let state = wire::resolve_reply::State::try_from(reply.state)
        .map_err(|_| Error::Peer("unknown resolve state"))?;
    match state {
        wire::resolve_reply::State::Committed | wire::resolve_reply::State::Rejected => {
            validate_mutation_reply(
                reply
                    .reply
                    .as_ref()
                    .ok_or(Error::Peer("resolved mutation reply is missing"))?,
            )
        }
        wire::resolve_reply::State::Absent
        | wire::resolve_reply::State::Unknown
        | wire::resolve_reply::State::Expired
            if reply.reply.is_none() =>
        {
            Ok(())
        }
        wire::resolve_reply::State::Invalid => Err(Error::Peer("invalid resolve state")),
        _ => Err(Error::Peer("resolve state and reply disagree")),
    }
}

fn validate_receipt(receipt: &wire::Receipt) -> Result<()> {
    if receipt.cell_id.len() != 32 || receipt.incarnation.len() != 16 {
        return Err(Error::Peer("invalid peer receipt identity"));
    }
    Ok(())
}

fn validate_error(error: &wire::Error) -> Result<()> {
    let code = wire::error::Code::try_from(error.code)
        .map_err(|_| Error::Peer("unknown peer error code"))?;
    let outcome = wire::error::Outcome::try_from(error.outcome)
        .map_err(|_| Error::Peer("unknown peer error outcome"))?;
    if code == wire::error::Code::Invalid
        || outcome == wire::error::Outcome::Unspecified
        || error.message.is_empty()
        || error.message.len() > 2_048
        || error.retry_after_ms > 60_000
        || error.application_details.len() > MAX_OPERATION_BYTES
    {
        return Err(Error::Peer("invalid peer error bounds"));
    }
    Ok(())
}

fn validate_principal(principal: &PeerPrincipal) -> Result<()> {
    if principal.issuer.is_empty()
        || principal.subject.is_empty()
        || principal.issuer.len() > MAX_PRINCIPAL_BYTES
        || principal.subject.len() > MAX_PRINCIPAL_BYTES
        || principal.actions.is_empty()
        || principal.actions.len() > MAX_ACTIONS
    {
        return Err(Error::Peer("invalid peer principal bounds"));
    }
    let mut previous: Option<&str> = None;
    for action in &principal.actions {
        if action.is_empty()
            || action.len() > 128
            || !action.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'-')
            })
            || previous.is_some_and(|previous| previous >= action.as_str())
        {
            return Err(Error::Peer(
                "peer actions must be sorted unique identifiers",
            ));
        }
        previous = Some(action);
    }
    Ok(())
}

fn validate_authorization(
    authorization: &wire::PeerAuthorization,
    now_ms: i64,
    remaining_ms: u32,
) -> Result<()> {
    let principal = PeerPrincipal {
        issuer: authorization.principal_issuer.clone(),
        subject: authorization.principal_subject.clone(),
        actions: authorization.actions.clone(),
    };
    validate_principal(&principal)?;
    if authorization.origin_session.len() != 16
        || authorization.release_digest.len() != 32
        || authorization.payload_digest.len() != 32
        || authorization.signature.len() != 64
    {
        return Err(Error::Peer("invalid peer authorization identity length"));
    }
    validate_time_bounds(
        authorization.issued_at_ms,
        authorization.expires_at_ms,
        now_ms,
        remaining_ms,
    )
}

fn validate_time_bounds(
    issued_at_ms: i64,
    expires_at_ms: i64,
    now_ms: i64,
    remaining_ms: u32,
) -> Result<()> {
    if issued_at_ms < 0
        || expires_at_ms <= issued_at_ms
        || expires_at_ms - issued_at_ms > MAX_AUTH_LIFETIME_MS
        || issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || expires_at_ms <= now_ms
        || !(1..=60_000).contains(&remaining_ms)
        || i64::from(remaining_ms) > expires_at_ms - now_ms
    {
        return Err(Error::Peer("invalid or expired peer authorization time"));
    }
    Ok(())
}

fn operation_target(operation: Option<&wire::peer_request::Operation>) -> Result<CellTarget> {
    let target = match operation {
        Some(wire::peer_request::Operation::Mutate(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Read(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Resolve(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::DeliverEffect(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::ResolveEffect(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Migrate(value)) => value.target.as_ref(),
        None => return Err(Error::Peer("peer operation is missing")),
    }
    .ok_or(Error::Peer("peer target is missing"))?;
    CellTarget::new(
        TenantId::try_from(target.tenant_id.as_slice())?,
        ApplicationId::try_from(target.application_id.as_slice())?,
        NamespaceId::try_from(target.namespace_id.as_slice())?,
        &target.partition,
    )
}

fn validate_decoded_operation(
    operation: Option<&wire::peer_request::Operation>,
    now_ms: i64,
) -> Result<()> {
    match operation {
        Some(wire::peer_request::Operation::Mutate(value)) => validate_mutation(value, now_ms),
        Some(wire::peer_request::Operation::Read(value)) => validate_read(value),
        Some(wire::peer_request::Operation::Resolve(value)) => validate_resolve(value, now_ms),
        Some(wire::peer_request::Operation::DeliverEffect(value)) => validate_effect(value, now_ms),
        Some(wire::peer_request::Operation::ResolveEffect(value)) => {
            validate_effect_resolve(value, now_ms)
        }
        Some(wire::peer_request::Operation::Migrate(value)) => validate_migration(value),
        None => Err(Error::Peer("peer operation is missing")),
    }
}

fn validate_mutation(request: &wire::MutationRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    validate_timeout(request.timeout_ms)?;
    let identity = request
        .identity
        .as_ref()
        .ok_or(Error::Peer("mutation identity is missing"))?;
    validate_mutation_identity(identity, now_ms)?;
    match request.operation.as_ref() {
        Some(wire::mutation_request::Operation::CellCommand(command))
            if command.command_id != 0 && command.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::mutation_request::Operation::CellCommand(_)) => {
            Err(Error::Peer("invalid Cell command identifier"))
        }
        None => Err(Error::Peer("mutation operation is missing")),
    }
}

fn validate_read(request: &wire::ReadRequest) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    validate_timeout(request.timeout_ms)?;
    if let Some(receipt) = &request.minimum
        && (receipt.cell_id.len() != 32 || receipt.incarnation.len() != 16)
    {
        return Err(Error::Peer("invalid minimum receipt identity length"));
    }
    match request.operation.as_ref() {
        Some(wire::read_request::Operation::Describe(true)) => Ok(()),
        Some(wire::read_request::Operation::Describe(false)) => {
            Err(Error::Peer("describe selector must be true"))
        }
        Some(wire::read_request::Operation::CellQuery(query))
            if query.query_id != 0 && query.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::read_request::Operation::CellQuery(_)) => {
            Err(Error::Peer("invalid Cell query identifier"))
        }
        None => Err(Error::Peer("read operation is missing")),
    }
}

fn validate_resolve(request: &wire::ResolveRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    let identity = request
        .identity
        .as_ref()
        .ok_or(Error::Peer("resolve identity is missing"))?;
    validate_mutation_identity(identity, now_ms)?;
    if request.operation_digest.len() != 32 {
        return Err(Error::Peer("invalid operation digest length"));
    }
    Ok(())
}

fn validate_effect(request: &wire::EffectRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.destination_incarnation.len() != 16 {
        return Err(Error::Peer("invalid effect destination incarnation"));
    }
    validate_effect_identity(
        request
            .identity
            .as_ref()
            .ok_or(Error::Peer("effect identity is missing"))?,
        now_ms,
    )?;
    match request.operation.as_ref() {
        Some(wire::effect_request::Operation::CellCommand(command))
            if command.command_id != 0 && command.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::effect_request::Operation::CellCommand(_)) => {
            Err(Error::Peer("invalid effect Cell command identifier"))
        }
        None => Err(Error::Peer("effect operation is missing")),
    }
}

fn validate_effect_resolve(request: &wire::EffectResolveRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.destination_incarnation.len() != 16 || request.operation_digest.len() != 32 {
        return Err(Error::Peer("invalid effect Resolve identity"));
    }
    validate_effect_identity(
        request
            .identity
            .as_ref()
            .ok_or(Error::Peer("effect Resolve identity is missing"))?,
        now_ms,
    )
}

fn validate_migration(request: &wire::MigrationRequest) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.incarnation.len() != 16
        || request.from_code.len() != 32
        || request.to_code.len() != 32
        || request.from_schema == 0
        || request.to_schema == 0
        || request.from_code == request.to_code && request.from_schema == request.to_schema
    {
        return Err(Error::Peer("invalid peer migration versions"));
    }
    Ok(())
}

fn validate_description_wire(description: &wire::CellDescription) -> Result<()> {
    if description.cell_id.len() != 32
        || description.incarnation.len() != 16
        || description.code.len() != 32
        || description.schema == 0
    {
        return Err(Error::Peer("invalid peer Cell description"));
    }
    Ok(())
}

fn validate_effect_identity(identity: &wire::EffectIdentity, now_ms: i64) -> Result<()> {
    let remaining_ms = identity.expires_at_ms.checked_sub(now_ms);
    if identity.effect_id.len() != 32
        || identity.source_cell.len() != 32
        || identity.source_incarnation.len() != 16
        || identity.source_sequence == 0
        || now_ms < 0
        || remaining_ms.is_none_or(|remaining| remaining <= 0 || remaining > MAX_EFFECT_LIFETIME_MS)
    {
        return Err(Error::Peer("invalid or expired effect identity"));
    }
    let source_cell = crate::CellId::try_from(identity.source_cell.as_slice())?;
    let source_incarnation = IncarnationId::try_from(identity.source_incarnation.as_slice())?;
    if identity.effect_id.as_slice()
        != crate::effect_id(
            source_cell,
            source_incarnation,
            identity.source_sequence,
            identity.ordinal,
        )
    {
        return Err(Error::Peer("effect identity derivation does not match"));
    }
    Ok(())
}

fn validate_target_wire(target: Option<&wire::Target>) -> Result<()> {
    let target = target.ok_or(Error::Peer("peer target is missing"))?;
    if target.tenant_id.len() != 16
        || target.application_id.len() != 16
        || target.namespace_id.len() != 16
        || target.partition.len() > 1_024
    {
        return Err(Error::Peer("invalid peer target bounds"));
    }
    Ok(())
}

fn validate_mutation_identity(identity: &wire::MutationIdentity, now_ms: i64) -> Result<()> {
    if identity.request_id.len() != 16
        || identity.incarnation.len() != 16
        || identity.issued_at_ms < 0
        || identity.expires_at_ms <= identity.issued_at_ms
        || identity.expires_at_ms - identity.issued_at_ms > MAX_MUTATION_LIFETIME_MS
        || identity.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || identity.expires_at_ms <= now_ms
    {
        return Err(Error::Peer("invalid or expired mutation identity"));
    }
    Ok(())
}

fn validate_timeout(timeout_ms: u32) -> Result<()> {
    if timeout_ms > 60_000 {
        return Err(Error::Peer("peer operation timeout exceeds 60 seconds"));
    }
    Ok(())
}

fn signing_bytes(tag: u32, authorization: &wire::PeerAuthorization) -> Result<Vec<u8>> {
    let tag = u16::try_from(tag).map_err(|_| Error::Peer("operation tag overflow"))?;
    let mut output = Vec::with_capacity(256);
    output.extend_from_slice(b"crab.peer.v1\0");
    output.extend_from_slice(&tag.to_be_bytes());
    append_bytes(&mut output, &authorization.origin_session)?;
    append_bytes(&mut output, authorization.principal_issuer.as_bytes())?;
    append_bytes(&mut output, authorization.principal_subject.as_bytes())?;
    append_u32(&mut output, authorization.actions.len())?;
    for action in &authorization.actions {
        append_bytes(&mut output, action.as_bytes())?;
    }
    append_bytes(&mut output, &authorization.release_digest)?;
    output.extend_from_slice(&authorization.issued_at_ms.to_be_bytes());
    output.extend_from_slice(&authorization.expires_at_ms.to_be_bytes());
    append_bytes(&mut output, &authorization.payload_digest)?;
    if output.len() > MAX_AUTHORIZATION_BYTES {
        return Err(Error::Peer("canonical authorization exceeds 16 KiB"));
    }
    Ok(output)
}

fn append_u32(output: &mut Vec<u8>, value: usize) -> Result<()> {
    output.extend_from_slice(
        &u32::try_from(value)
            .map_err(|_| Error::Peer("canonical value exceeds u32"))?
            .to_be_bytes(),
    );
    Ok(())
}

fn append_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    append_u32(output, value.len())?;
    output.extend_from_slice(value);
    Ok(())
}

fn encode_request(
    authorization: wire::PeerAuthorization,
    hop_count: u32,
    remaining_ms: u32,
    operation_tag: u32,
    operation: &[u8],
) -> Result<Vec<u8>> {
    let authorization = authorization.encode_to_vec();
    if authorization.len() > MAX_AUTHORIZATION_BYTES || operation.len() > MAX_OPERATION_BYTES {
        return Err(Error::Peer("peer request component exceeds limit"));
    }
    let mut output = Vec::with_capacity(authorization.len() + operation.len() + 32);
    encode_varint_field(&mut output, 1, u64::from(PROTOCOL_VERSION));
    encode_bytes_field(&mut output, 2, &authorization)?;
    encode_varint_field(&mut output, 3, u64::from(hop_count));
    encode_varint_field(&mut output, 4, u64::from(remaining_ms));
    encode_bytes_field(&mut output, operation_tag, operation)?;
    if output.len() > MAX_PEER_REQUEST_BYTES {
        return Err(Error::Peer("request exceeds peer byte limit"));
    }
    Ok(output)
}

fn encode_varint_field(output: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(output, u64::from(field) << 3);
    encode_varint(output, value);
}

fn encode_bytes_field(output: &mut Vec<u8>, field: u32, value: &[u8]) -> Result<()> {
    encode_varint(output, (u64::from(field) << 3) | 2);
    encode_varint(
        output,
        u64::try_from(value.len()).map_err(|_| Error::Peer("peer field length overflow"))?,
    );
    output.extend_from_slice(value);
    Ok(())
}

fn encode_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

#[cfg(test)]
mod tests;
