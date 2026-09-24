//! Peer operation envelopes, signing, and transport authorization.
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use prost::Message;

use crate::identity::IncarnationId;
use crate::identity::{ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId};
use crate::{Error, Result};

mod dispatch;
mod protobuf;
mod transport;
mod validation;

pub use dispatch::{PeerAuthorizer, PeerCellResolver, PeerDispatcher};
pub(crate) use transport::PeerClientTransport;
pub use transport::PeerRoundTrip;
pub use transport::{EffectPeerClient, MigrationPeerClient};

use protobuf::{
    MessageKind, field_payload, oneof_payload, require_fields, validate_message, validate_operation,
};

use validation::*;

const PROTOCOL_VERSION: u32 = 1;
const MAX_AUTHORIZATION_BYTES: usize = 16 * 1024;
const MAX_OPERATION_BYTES: usize = crate::codec::MAX_WIRE_BYTES;
pub const MAX_PEER_REQUEST_BYTES: usize = MAX_AUTHORIZATION_BYTES + MAX_OPERATION_BYTES + 128;
const MAX_ACTIONS: usize = 128;
const MAX_PRINCIPAL_BYTES: usize = 512;
const MAX_AUTH_LIFETIME_MS: i64 = 60_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const MAX_MUTATION_LIFETIME_MS: i64 = 24 * 60 * 60_000;
const MAX_EFFECT_LIFETIME_MS: i64 = 7 * 24 * 60 * 60_000;

/// Generated private peer messages. They are not a public service or
/// application API.
///
/// The schema is documented once in `docs/contracts/peer.proto`; the generated
/// types and fields deliberately carry no Rust doc comments of their own.
#[allow(missing_docs)]
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
            return Err(Error::Peer("operation exceeds wire limit"));
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
            return Err(Error::Peer("operation exceeds wire limit"));
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
