use std::{collections::HashSet, ops::Range};

use crate::{Error, Result};

#[derive(Clone, Copy)]
pub(super) enum MessageKind {
    PeerRequest,
    Authorization,
    MutationRequest,
    ReadRequest,
    ResolveRequest,
    EffectRequest,
    EffectResolveRequest,
    MigrationRequest,
    EffectIdentity,
    Target,
    MutationIdentity,
    CellCommand,
    CellQuery,
    Receipt,
    PeerReply,
    MutationReply,
    MutationResult,
    ReadReply,
    ResolveReply,
    Error,
    CellDescription,
    MigrationReply,
}

#[derive(Clone, Copy)]
struct FieldRule {
    tag: u32,
    wire: u8,
    nested: Option<MessageKind>,
    repeated: bool,
    oneof: u8,
}

const fn scalar(tag: u32, wire: u8) -> FieldRule {
    FieldRule {
        tag,
        wire,
        nested: None,
        repeated: false,
        oneof: 0,
    }
}

const fn message(tag: u32, nested: MessageKind) -> FieldRule {
    FieldRule {
        tag,
        wire: 2,
        nested: Some(nested),
        repeated: false,
        oneof: 0,
    }
}

const fn oneof(tag: u32, nested: Option<MessageKind>, group: u8) -> FieldRule {
    FieldRule {
        tag,
        wire: 2,
        nested,
        repeated: false,
        oneof: group,
    }
}

const fn scalar_oneof(tag: u32, wire: u8, group: u8) -> FieldRule {
    FieldRule {
        tag,
        wire,
        nested: None,
        repeated: false,
        oneof: group,
    }
}

const fn repeated(tag: u32, wire: u8) -> FieldRule {
    FieldRule {
        tag,
        wire,
        nested: None,
        repeated: true,
        oneof: 0,
    }
}

fn rules(kind: MessageKind) -> Vec<FieldRule> {
    match kind {
        MessageKind::PeerRequest => vec![
            scalar(1, 0),
            message(2, MessageKind::Authorization),
            scalar(3, 0),
            scalar(4, 0),
            oneof(10, Some(MessageKind::MutationRequest), 1),
            oneof(11, Some(MessageKind::ReadRequest), 1),
            oneof(12, Some(MessageKind::ResolveRequest), 1),
            oneof(13, Some(MessageKind::EffectRequest), 1),
            oneof(14, Some(MessageKind::EffectResolveRequest), 1),
            oneof(15, Some(MessageKind::MigrationRequest), 1),
        ],
        MessageKind::Authorization => vec![
            scalar(1, 2),
            scalar(2, 2),
            scalar(3, 2),
            repeated(4, 2),
            scalar(5, 2),
            scalar(6, 0),
            scalar(7, 0),
            scalar(8, 2),
            scalar(9, 2),
        ],
        MessageKind::MutationRequest => vec![
            message(1, MessageKind::Target),
            message(2, MessageKind::MutationIdentity),
            scalar(3, 0),
            oneof(10, Some(MessageKind::CellCommand), 1),
        ],
        MessageKind::ReadRequest => vec![
            message(1, MessageKind::Target),
            scalar(2, 0),
            message(3, MessageKind::Receipt),
            scalar_oneof(10, 0, 1),
            oneof(15, Some(MessageKind::CellQuery), 1),
        ],
        MessageKind::ResolveRequest => vec![
            message(1, MessageKind::Target),
            message(2, MessageKind::MutationIdentity),
            scalar(3, 2),
        ],
        MessageKind::EffectRequest => vec![
            message(1, MessageKind::Target),
            scalar(2, 2),
            message(3, MessageKind::EffectIdentity),
            oneof(10, Some(MessageKind::CellCommand), 1),
        ],
        MessageKind::EffectResolveRequest => vec![
            message(1, MessageKind::Target),
            scalar(2, 2),
            message(3, MessageKind::EffectIdentity),
            scalar(4, 2),
        ],
        MessageKind::MigrationRequest => vec![
            message(1, MessageKind::Target),
            scalar(2, 2),
            scalar(3, 2),
            scalar(4, 0),
            scalar(5, 2),
            scalar(6, 0),
        ],
        MessageKind::EffectIdentity => vec![
            scalar(1, 2),
            scalar(2, 2),
            scalar(3, 2),
            scalar(4, 0),
            scalar(5, 0),
            scalar(6, 0),
        ],
        MessageKind::Target => vec![scalar(1, 2), scalar(2, 2), scalar(3, 2), scalar(4, 2)],
        MessageKind::MutationIdentity => {
            vec![scalar(1, 2), scalar(2, 2), scalar(3, 0), scalar(4, 0)]
        }
        MessageKind::CellCommand | MessageKind::CellQuery => {
            vec![scalar(1, 0), scalar(2, 0), scalar(3, 2)]
        }
        MessageKind::Receipt => vec![scalar(1, 2), scalar(2, 2), scalar(3, 0)],
        MessageKind::PeerReply => vec![
            oneof(1, Some(MessageKind::MutationReply), 1),
            oneof(2, Some(MessageKind::ReadReply), 1),
            oneof(3, Some(MessageKind::ResolveReply), 1),
            oneof(4, Some(MessageKind::Error), 1),
            oneof(5, Some(MessageKind::MigrationReply), 1),
        ],
        MessageKind::MutationReply => vec![
            message(1, MessageKind::Receipt),
            oneof(2, Some(MessageKind::MutationResult), 1),
            oneof(3, Some(MessageKind::Error), 1),
        ],
        MessageKind::MutationResult => vec![scalar_oneof(1, 2, 1)],
        MessageKind::ReadReply => vec![
            message(1, MessageKind::Receipt),
            oneof(2, Some(MessageKind::CellDescription), 1),
            scalar_oneof(6, 2, 1),
            oneof(7, Some(MessageKind::Error), 1),
        ],
        MessageKind::ResolveReply => vec![scalar(1, 0), message(2, MessageKind::MutationReply)],
        MessageKind::Error => vec![
            scalar(1, 0),
            scalar(2, 0),
            scalar(3, 2),
            scalar(4, 0),
            scalar(5, 2),
        ],
        MessageKind::CellDescription => {
            vec![scalar(1, 2), scalar(2, 2), scalar(3, 2), scalar(4, 0)]
        }
        MessageKind::MigrationReply => vec![message(1, MessageKind::CellDescription)],
    }
}

pub(super) struct FieldOccurrence {
    tag: u32,
    payload: Option<Range<usize>>,
}

impl FieldOccurrence {
    pub(super) const fn tag(&self) -> u32 {
        self.tag
    }
}

pub(super) fn validate_message(input: &[u8], kind: MessageKind) -> Result<Vec<FieldOccurrence>> {
    let rules = rules(kind);
    let mut position = 0;
    let mut fields = Vec::new();
    let mut singular = HashSet::new();
    let mut oneofs = HashSet::new();
    while position < input.len() {
        let key = decode_varint(input, &mut position)?;
        let tag = u32::try_from(key >> 3).map_err(|_| Error::Peer("field tag overflow"))?;
        let wire = (key & 7) as u8;
        if tag == 0 {
            return Err(Error::Peer("zero Protobuf field tag"));
        }
        let rule = rules
            .iter()
            .find(|rule| rule.tag == tag)
            .ok_or(Error::Peer("unknown Protobuf field"))?;
        if wire != rule.wire {
            return Err(Error::Peer("wrong Protobuf wire type"));
        }
        if !rule.repeated && !singular.insert(tag) {
            return Err(Error::Peer("duplicate singular Protobuf field"));
        }
        if rule.oneof != 0 && !oneofs.insert(rule.oneof) {
            return Err(Error::Peer("duplicate Protobuf oneof"));
        }
        let payload = match wire {
            0 => {
                decode_varint(input, &mut position)?;
                None
            }
            1 => {
                take(input, &mut position, 8)?;
                None
            }
            2 => {
                let length = usize::try_from(decode_varint(input, &mut position)?)
                    .map_err(|_| Error::Peer("Protobuf length overflow"))?;
                let start = position;
                take(input, &mut position, length)?;
                let range = start..position;
                if let Some(nested) = rule.nested {
                    validate_message(&input[range.clone()], nested)?;
                }
                Some(range)
            }
            5 => {
                take(input, &mut position, 4)?;
                None
            }
            _ => return Err(Error::Peer("unsupported Protobuf wire type")),
        };
        fields.push(FieldOccurrence { tag, payload });
    }
    Ok(fields)
}

fn decode_varint(input: &[u8], position: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..=63).step_by(7) {
        let byte = *input
            .get(*position)
            .ok_or(Error::Peer("truncated Protobuf varint"))?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return Err(Error::Peer("Protobuf varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Peer("Protobuf varint overflow"))
}

fn take<'a>(input: &'a [u8], position: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = position
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .ok_or(Error::Peer("truncated Protobuf field"))?;
    let value = &input[*position..end];
    *position = end;
    Ok(value)
}

pub(super) fn require_fields(fields: &[FieldOccurrence], required: &[u32]) -> Result<()> {
    if required
        .iter()
        .any(|required| !fields.iter().any(|field| field.tag == *required))
    {
        return Err(Error::Peer("required Protobuf field is missing"));
    }
    Ok(())
}

pub(super) fn field_payload(fields: &[FieldOccurrence], tag: u32) -> Result<Range<usize>> {
    fields
        .iter()
        .find(|field| field.tag == tag)
        .and_then(|field| field.payload.clone())
        .ok_or(Error::Peer("required Protobuf payload is missing"))
}

pub(super) fn oneof_payload<'a>(
    input: &'a [u8],
    fields: &[FieldOccurrence],
    tags: &[u32],
) -> Result<(u32, &'a [u8])> {
    let field = fields
        .iter()
        .find(|field| tags.contains(&field.tag))
        .ok_or(Error::Peer("peer operation is missing"))?;
    let range = field
        .payload
        .clone()
        .ok_or(Error::Peer("peer operation payload is missing"))?;
    Ok((field.tag, &input[range]))
}

pub(super) fn validate_operation(tag: u32, payload: &[u8]) -> Result<()> {
    let (kind, required, operation_tags): (MessageKind, &[u32], &[u32]) = match tag {
        10 => (MessageKind::MutationRequest, &[1, 2], &[10]),
        11 => (MessageKind::ReadRequest, &[1], &[10, 15]),
        12 => (MessageKind::ResolveRequest, &[1, 2, 3], &[]),
        13 => (MessageKind::EffectRequest, &[1, 2, 3], &[10]),
        14 => (MessageKind::EffectResolveRequest, &[1, 2, 3, 4], &[]),
        15 => (MessageKind::MigrationRequest, &[1, 2, 3, 4, 5, 6], &[]),
        _ => return Err(Error::Peer("peer operation is not implemented")),
    };
    let fields = validate_message(payload, kind)?;
    require_fields(&fields, required)?;
    if !operation_tags.is_empty()
        && !fields
            .iter()
            .any(|field| operation_tags.contains(&field.tag))
    {
        return Err(Error::Peer("typed peer operation is not implemented"));
    }
    Ok(())
}
