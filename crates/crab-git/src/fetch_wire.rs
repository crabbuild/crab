//! Protocol-v2 client framing for fetching from a smart HTTP upload-pack server.
//!
//! This is the client counterpart of Crab's upload-pack server framing. It is
//! sans-IO: callers own transport, authentication, deadlines and size bounds.
//! Requests always send `done`, so responses carry no negotiation rounds.

use gix_hash::ObjectId;

/// The largest pkt-line, including its four-byte length prefix.
pub const MAX_PKT_LINE: usize = 65_520;
const FLUSH: &[u8] = b"0000";
const DELIMITER: &[u8] = b"0001";

/// Failures decoding a server's protocol-v2 messages.
#[derive(Debug, thiserror::Error)]
pub enum FetchWireError {
    #[error("Git server sent a malformed {0}")]
    Malformed(&'static str),
    #[error("Git server does not support protocol version 2")]
    VersionTwoRequired,
    #[error("Git server does not support {0}")]
    Unsupported(&'static str),
    #[error("Git server reported an error: {0}")]
    Remote(String),
    #[error("Git source is shallow; Crab imports complete histories only")]
    Shallow,
}

type Result<T> = std::result::Result<T, FetchWireError>;

/// Commands and features advertised by a protocol-v2 server.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub agent: Option<String>,
    pub object_format: Option<String>,
    pub ls_refs: Option<Vec<String>>,
    pub fetch: Option<Vec<String>>,
}

impl Capabilities {
    /// Whether `fetch` advertises `feature`.
    #[must_use]
    pub fn fetch_supports(&self, feature: &str) -> bool {
        self.fetch
            .as_ref()
            .is_some_and(|features| features.iter().any(|value| value == feature))
    }
}

/// Parse the smart-HTTP `info/refs?service=git-upload-pack` body of a v2 server.
pub fn parse_advertisement(body: &[u8]) -> Result<Capabilities> {
    let mut lines = PacketLines::new(body);
    let mut first = lines.next_line()?;
    // Smart HTTP prefixes the advertisement with a service announcement.
    if let Packet::Data(line) = first
        && line.starts_with(b"# service=")
    {
        if lines.next_line()? != Packet::Flush {
            return Err(FetchWireError::Malformed("service announcement"));
        }
        first = lines.next_line()?;
    }
    if first != Packet::Data(b"version 2") {
        return Err(FetchWireError::VersionTwoRequired);
    }
    let mut capabilities = Capabilities::default();
    loop {
        match lines.next_line()? {
            Packet::Flush => break,
            Packet::Data(line) => {
                let line = text(line, "capability")?;
                let (key, value) = line.split_once('=').unwrap_or((line, ""));
                let features = || {
                    value
                        .split(' ')
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                        .collect()
                };
                match key {
                    "agent" => capabilities.agent = Some(value.to_owned()),
                    "object-format" => capabilities.object_format = Some(value.to_owned()),
                    "ls-refs" => capabilities.ls_refs = Some(features()),
                    "fetch" => capabilities.fetch = Some(features()),
                    _ => {}
                }
            }
            _ => return Err(FetchWireError::Malformed("capability advertisement")),
        }
    }
    if capabilities.ls_refs.is_none() {
        return Err(FetchWireError::Unsupported("ls-refs"));
    }
    if capabilities.fetch.is_none() {
        return Err(FetchWireError::Unsupported("fetch"));
    }
    if capabilities
        .object_format
        .as_deref()
        .is_some_and(|format| format != "sha1")
    {
        return Err(FetchWireError::Unsupported("SHA-256 repositories"));
    }
    Ok(capabilities)
}

/// Encode an `ls-refs` request limited to `prefixes`.
#[must_use]
pub fn encode_ls_refs(agent: &str, prefixes: &[&str]) -> Vec<u8> {
    let mut body = command(agent, "ls-refs");
    for argument in ["symrefs", "peel", "unborn"] {
        packet(&mut body, argument);
    }
    for prefix in prefixes {
        packet(&mut body, &format!("ref-prefix {prefix}"));
    }
    body.extend_from_slice(FLUSH);
    body
}

/// One reference reported by `ls-refs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedRef {
    pub name: String,
    /// `None` for an unborn symbolic ref, such as an empty repository's HEAD.
    pub oid: Option<ObjectId>,
    pub symref_target: Option<String>,
    pub peeled: Option<ObjectId>,
}

/// Parse an `ls-refs` response.
pub fn parse_ls_refs(body: &[u8]) -> Result<Vec<AdvertisedRef>> {
    let mut lines = PacketLines::new(body);
    let mut refs = Vec::new();
    loop {
        match lines.next_line()? {
            Packet::Flush => break,
            Packet::Data(line) => {
                let line = text(line, "ref listing")?;
                remote_error(line)?;
                let mut fields = line.split(' ');
                let oid = fields
                    .next()
                    .ok_or(FetchWireError::Malformed("ref listing"))?;
                let name = fields
                    .next()
                    .ok_or(FetchWireError::Malformed("ref listing"))?;
                let oid = match oid {
                    "unborn" => None,
                    oid => Some(object_id(oid)?),
                };
                let mut reference = AdvertisedRef {
                    name: name.to_owned(),
                    oid,
                    symref_target: None,
                    peeled: None,
                };
                for attribute in fields {
                    if let Some(target) = attribute.strip_prefix("symref-target:") {
                        reference.symref_target = Some(target.to_owned());
                    } else if let Some(peeled) = attribute.strip_prefix("peeled:") {
                        reference.peeled = Some(object_id(peeled)?);
                    }
                }
                refs.push(reference);
            }
            _ => return Err(FetchWireError::Malformed("ref listing")),
        }
    }
    if lines.rest().is_empty() {
        Ok(refs)
    } else {
        Err(FetchWireError::Malformed("ref listing trailer"))
    }
}

/// Encode a single-round `fetch` request for a complete, self-contained pack.
///
/// The request never asks for a thin pack, so every delta base is in the pack
/// or reachable from `haves` on the server, and it always sends `done`.
#[must_use]
pub fn encode_fetch(agent: &str, wants: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    let mut body = command(agent, "fetch");
    packet(&mut body, "ofs-delta");
    for want in wants {
        packet(&mut body, &format!("want {want}"));
    }
    for have in haves {
        packet(&mut body, &format!("have {have}"));
    }
    packet(&mut body, "done");
    body.extend_from_slice(FLUSH);
    body
}

/// Incremental decoder of a fetch response to a request that sent `done`.
pub struct FetchResponse {
    buffer: Vec<u8>,
    start: usize,
    state: Section,
    progress: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Header,
    Ignored,
    Packfile,
    Done,
}

impl Default for FetchResponse {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchResponse {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            state: Section::Header,
            progress: Vec::new(),
        }
    }

    /// Decode the next response bytes, passing pack data to `pack` in order.
    pub fn feed<E>(
        &mut self,
        bytes: &[u8],
        pack: &mut dyn FnMut(&[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), FeedError<E>> {
        self.buffer.drain(..self.start);
        self.start = 0;
        self.buffer.extend_from_slice(bytes);
        loop {
            let available = &self.buffer[self.start..];
            let Some((packet, length)) = parse_packet(available).map_err(FeedError::Wire)? else {
                return Ok(());
            };
            let consumed = self.start + length;
            match (self.state, packet) {
                (Section::Done, _) => {
                    return Err(FeedError::Wire(FetchWireError::Malformed(
                        "data after the packfile",
                    )));
                }
                (Section::Header, Packet::Data(line)) => {
                    let line = text(line, "section header").map_err(FeedError::Wire)?;
                    remote_error(line).map_err(FeedError::Wire)?;
                    self.state = match line {
                        "packfile" => Section::Packfile,
                        // A shallow source cannot provide a complete closure.
                        "shallow-info" => return Err(FeedError::Wire(FetchWireError::Shallow)),
                        "packfile-uris" => {
                            return Err(FeedError::Wire(FetchWireError::Unsupported(
                                "packfile URIs",
                            )));
                        }
                        "acknowledgments" | "wanted-refs" => Section::Ignored,
                        _ => {
                            return Err(FeedError::Wire(FetchWireError::Malformed(
                                "fetch response section",
                            )));
                        }
                    };
                }
                (Section::Ignored, Packet::Data(_)) => {}
                (Section::Ignored, Packet::Delimiter) => self.state = Section::Header,
                (Section::Packfile, Packet::Data(payload)) => match payload.split_first() {
                    Some((1, data)) => pack(data).map_err(FeedError::Sink)?,
                    Some((2, message)) => {
                        // Keep only the latest progress line for status reporting.
                        self.progress.clear();
                        self.progress.extend_from_slice(message);
                    }
                    Some((3, message)) => {
                        return Err(FeedError::Wire(FetchWireError::Remote(
                            String::from_utf8_lossy(message).trim().to_owned(),
                        )));
                    }
                    _ => {
                        return Err(FeedError::Wire(FetchWireError::Malformed(
                            "side-band packet",
                        )));
                    }
                },
                (Section::Packfile, Packet::Flush) => self.state = Section::Done,
                _ => {
                    return Err(FeedError::Wire(FetchWireError::Malformed(
                        "fetch response framing",
                    )));
                }
            }
            self.start = consumed;
        }
    }

    /// Finish decoding; the packfile section must have ended with a flush.
    pub fn finish(self) -> Result<()> {
        if self.state == Section::Done && self.start == self.buffer.len() {
            Ok(())
        } else {
            Err(FetchWireError::Malformed("truncated fetch response"))
        }
    }

    /// The latest progress message, if the server sent one.
    #[must_use]
    pub fn progress(&self) -> Option<String> {
        (!self.progress.is_empty()).then(|| {
            String::from_utf8_lossy(&self.progress)
                .trim_end_matches(['\r', '\n'])
                .rsplit('\r')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    }
}

/// A wire failure or a rejection from the pack consumer.
#[derive(Debug, thiserror::Error)]
pub enum FeedError<E> {
    #[error(transparent)]
    Wire(FetchWireError),
    #[error("pack consumer rejected streamed data")]
    Sink(#[source] E),
}

#[derive(Debug, PartialEq, Eq)]
enum Packet<'a> {
    Flush,
    Delimiter,
    ResponseEnd,
    Data(&'a [u8]),
}

struct PacketLines<'a> {
    bytes: &'a [u8],
}

impl<'a> PacketLines<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    // Data lines are returned without one trailing LF.
    fn next_line(&mut self) -> Result<Packet<'a>> {
        let (packet, length) =
            parse_packet(self.bytes)?.ok_or(FetchWireError::Malformed("truncated pkt-line"))?;
        self.bytes = &self.bytes[length..];
        Ok(match packet {
            Packet::Data(line) => Packet::Data(line.strip_suffix(b"\n").unwrap_or(line)),
            other => other,
        })
    }

    fn rest(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Parse one pkt-line, or `None` when more bytes are needed.
fn parse_packet(bytes: &[u8]) -> Result<Option<(Packet<'_>, usize)>> {
    let Some(prefix) = bytes.get(..4) else {
        return Ok(None);
    };
    let length = std::str::from_utf8(prefix)
        .ok()
        .and_then(|prefix| usize::from_str_radix(prefix, 16).ok())
        .ok_or(FetchWireError::Malformed("pkt-line length"))?;
    Ok(Some(match length {
        0 => (Packet::Flush, 4),
        1 => (Packet::Delimiter, 4),
        2 => (Packet::ResponseEnd, 4),
        3 => return Err(FetchWireError::Malformed("pkt-line length")),
        length if length > MAX_PKT_LINE => {
            return Err(FetchWireError::Malformed("oversized pkt-line"));
        }
        length => {
            let Some(payload) = bytes.get(4..length) else {
                return Ok(None);
            };
            (Packet::Data(payload), length)
        }
    }))
}

fn command(agent: &str, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    packet(&mut body, &format!("command={name}"));
    packet(&mut body, &format!("agent={agent}"));
    packet(&mut body, "object-format=sha1");
    body.extend_from_slice(DELIMITER);
    body
}

fn packet(body: &mut Vec<u8>, line: &str) {
    body.extend_from_slice(format!("{:04x}", line.len() + 5).as_bytes());
    body.extend_from_slice(line.as_bytes());
    body.push(b'\n');
}

fn text<'a>(line: &'a [u8], what: &'static str) -> Result<&'a str> {
    std::str::from_utf8(line.strip_suffix(b"\n").unwrap_or(line))
        .map_err(|_| FetchWireError::Malformed(what))
}

fn remote_error(line: &str) -> Result<()> {
    match line.strip_prefix("ERR ") {
        Some(message) => Err(FetchWireError::Remote(message.to_owned())),
        None => Ok(()),
    }
}

fn object_id(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|_| FetchWireError::Malformed("object ID"))
}

#[cfg(test)]
mod tests;
