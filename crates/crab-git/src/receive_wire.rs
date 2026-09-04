//! Bounded native receive-pack command framing and exact status responses.
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

use bstr::ByteSlice;
use gix_hash::ObjectId;
use gix_packetline::{PacketLineRef, blocking_io::encode, decode::PacketLineOrWantedSize};

use crate::receive_plan::RefUpdate;

/// Maximum receive command count; pack data has separate intake limits.
pub const MAX_COMMANDS: usize = 1024;
/// Maximum combined command bytes, excluding the following pack.
pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;

// Git's protocol-common limits pkt-lines to 65,520 bytes including the prefix.
// hex_prefix decodes lengths without enforcing that protocol limit.
const MAX_PACKET_DATA: usize = 65_516;

const CAPABILITIES: &str =
    "report-status delete-refs ofs-delta atomic object-format=sha1 agent=crab";

/// A complete command section, with the input positioned at the pack boundary.
#[derive(Debug)]
pub struct ReceiveRequest {
    /// Exact commands in client order; validation/publication must retain their OIDs.
    pub updates: Vec<RefUpdate>,
    /// Whether the client negotiated per-ref status output.
    pub report_status: bool,
}

/// Errors in receive command and response framing.
#[derive(Debug, thiserror::Error)]
pub enum ReceiveWireError {
    #[error("receive protocol I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid receive packet length")]
    Packet(#[from] gix_packetline::decode::Error),
    #[error("receive command is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid receive object ID")]
    ObjectId {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("invalid receive ref name")]
    RefName(#[from] gix_validate::reference::name::Error),
    #[error("{0}")]
    Protocol(&'static str),
}

type Result<T> = std::result::Result<T, ReceiveWireError>;

/// Read one complete command section without consuming any pack bytes.
///
/// Call on a blocking worker with an I/O deadline enforced by the transport.
/// A lone flush is an empty request/probe. Shallow pushes, certificates and
/// unadvertised capabilities are rejected. Pack completeness, ref policy and
/// old-value checks belong to the receiver, not this parser.
pub fn read_request(reader: &mut impl Read) -> Result<ReceiveRequest> {
    let mut request = ReceiveRequest {
        updates: Vec::new(),
        report_status: false,
    };
    let mut total = 0usize;
    let mut names = std::collections::HashSet::new();
    loop {
        let mut prefix = [0; 4];
        reader.read_exact(&mut prefix)?;
        let len = match gix_packetline::decode::hex_prefix(&prefix)? {
            PacketLineOrWantedSize::Line(PacketLineRef::Flush) => return Ok(request),
            PacketLineOrWantedSize::Wanted(len) => usize::from(len),
            _ => {
                return Err(ReceiveWireError::Protocol(
                    "unexpected receive control packet",
                ));
            }
        };
        total = total.saturating_add(len + 4);
        if len > MAX_PACKET_DATA
            || total > MAX_COMMAND_BYTES
            || request.updates.len() >= MAX_COMMANDS
        {
            return Err(ReceiveWireError::Protocol(
                "receive command section exceeds its limit",
            ));
        }
        let mut packet = vec![0; len];
        reader.read_exact(&mut packet)?;
        let line = std::str::from_utf8(&packet)?;
        let line = line.strip_suffix('\n').unwrap_or(line);
        let command = if request.updates.is_empty() {
            let (command, capabilities) = line.split_once('\0').ok_or(
                ReceiveWireError::Protocol("first receive command must include capabilities"),
            )?;
            if capabilities.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(ReceiveWireError::Protocol(
                    "receive capabilities contain control bytes",
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for capability in capabilities.split_ascii_whitespace() {
                if !seen.insert(capability) {
                    return Err(ReceiveWireError::Protocol("duplicate receive capability"));
                }
                match capability {
                    "report-status" => request.report_status = true,
                    "delete-refs" | "ofs-delta" | "atomic" | "object-format=sha1" => {}
                    value
                        if value.starts_with("agent=")
                            && value.len() > 6
                            && !value.bytes().any(|b| b.is_ascii_control()) => {}
                    _ => return Err(ReceiveWireError::Protocol("unsupported receive capability")),
                }
            }
            command
        } else {
            if line.contains('\0') {
                return Err(ReceiveWireError::Protocol(
                    "capabilities are allowed only on the first command",
                ));
            }
            line
        };
        let mut fields = command.split(' ');
        let old = fields
            .next()
            .ok_or(ReceiveWireError::Protocol("missing old object ID"))?;
        let new = fields
            .next()
            .ok_or(ReceiveWireError::Protocol("missing new object ID"))?;
        let name = fields
            .next()
            .ok_or(ReceiveWireError::Protocol("missing destination ref"))?;
        if fields.next().is_some() || !name.starts_with("refs/") {
            return Err(ReceiveWireError::Protocol(
                "receive commands require exact old/new IDs and a full ref",
            ));
        }
        gix_validate::reference::name(name.as_bytes().as_bstr())?;
        if !names.insert(name.to_owned()) {
            return Err(ReceiveWireError::Protocol("duplicate receive destination"));
        }
        let old = parse_oid(old)?;
        let new = parse_oid(new)?;
        if old == new {
            return Err(ReceiveWireError::Protocol(
                "receive command does not change the ref",
            ));
        }
        request.updates.push(RefUpdate {
            name: name.to_owned(),
            old,
            new,
        });
    }
}

fn parse_oid(value: &str) -> Result<Option<ObjectId>> {
    if value.len() != 40 {
        return Err(ReceiveWireError::Protocol(
            "receive object IDs must be full SHA-1 values",
        ));
    }
    let oid =
        ObjectId::from_hex(value.as_bytes()).map_err(|source| ReceiveWireError::ObjectId {
            source: source.into(),
        })?;
    Ok((!oid.is_null()).then_some(oid))
}

/// Write a native receive advertisement, excluding the HTTP service preamble.
///
/// Pass only authorized, validated full refs in canonical name order. Empty
/// repositories advertise the standard zero-ID capability pseudo-ref.
/// The receiver must implement the advertised atomic batch and deletion semantics.
pub fn advertise(writer: &mut impl Write, refs: &BTreeMap<String, ObjectId>) -> Result<()> {
    for (position, (name, oid)) in refs.iter().enumerate() {
        gix_validate::reference::name(name.as_bytes().as_bstr())?;
        let suffix = if position == 0 {
            1 + CAPABILITIES.len()
        } else {
            0
        };
        if !name.starts_with("refs/") || oid.is_null() || 42 + name.len() + suffix > MAX_PACKET_DATA
        {
            return Err(ReceiveWireError::Protocol("invalid advertised ref"));
        }
    }
    if refs.is_empty() {
        encode::data_to_write(
            format!("{} capabilities^{{}}\0{CAPABILITIES}\n", "0".repeat(40)).as_bytes(),
            &mut *writer,
        )?;
    }
    for (position, (name, oid)) in refs.iter().enumerate() {
        let capabilities = if position == 0 {
            format!("\0{CAPABILITIES}")
        } else {
            String::new()
        };
        encode::data_to_write(
            format!("{oid} {name}{capabilities}\n").as_bytes(),
            &mut *writer,
        )?;
    }
    encode::flush_to_write(writer)?;
    Ok(())
}

/// Write an atomic batch's actual result after its outcome is known.
///
/// `rejection` rejects every command. Never use rejection for an uncertain
/// marker write or a failure after commit; fail the transport in those cases.
/// Call only when the client requested `report-status`.
pub fn report(
    writer: &mut impl Write,
    updates: &[RefUpdate],
    unpack_error: Option<&str>,
    rejection: Option<&str>,
) -> Result<()> {
    let reason = rejection.or(unpack_error);
    for update in updates {
        gix_validate::reference::name(update.name.as_bytes().as_bstr())?;
        let suffix = reason.map_or(0, |message| 1 + message.len());
        if !update.name.starts_with("refs/") || 4 + update.name.len() + suffix > MAX_PACKET_DATA {
            return Err(ReceiveWireError::Protocol("invalid status ref"));
        }
    }
    for message in [unpack_error, rejection].into_iter().flatten() {
        if message.is_empty()
            || message.len() > 1024
            || message.chars().any(char::is_control)
            || message == "ok"
        {
            return Err(ReceiveWireError::Protocol("invalid receive status reason"));
        }
    }
    encode::data_to_write(
        format!("unpack {}\n", unpack_error.unwrap_or("ok")).as_bytes(),
        &mut *writer,
    )?;
    for update in updates {
        let line = match reason {
            Some(reason) => format!("ng {} {reason}\n", update.name),
            None => format!("ok {}\n", update.name),
        };
        encode::data_to_write(line.as_bytes(), &mut *writer)?;
    }
    encode::flush_to_write(writer)?;
    Ok(())
}
