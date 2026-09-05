//! Shared protocol-v2 framing for the remote helper and HTTP transport.
use crate::{UploadPackFilter, combine_upload_pack_filters, parse_upload_pack_filter};
use gix_hash::ObjectId;
use gix_packetline::{PacketLineRef, decode::PacketLineOrWantedSize};
use std::collections::HashSet;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio_util::sync::CancellationToken;

/// Maximum packet-line size including its four-byte length prefix.
pub const MAX_PACKET_BYTES: usize = 65_520;
/// Maximum decoded command payload accepted by either transport.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_PACKETS: usize = 65_536;

/// Framing, syntax, cancellation and I/O failures at the protocol boundary.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("{0}")]
    Protocol(String),
    #[error("Git protocol I/O failed")]
    Io(#[from] std::io::Error),
    #[error("Git protocol request cancelled")]
    Cancelled,
}
type Result<T> = std::result::Result<T, WireError>;
fn protocol(message: impl Into<String>) -> WireError {
    WireError::Protocol(message.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Packet {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One framed command; transport envelopes may impose additional end-of-body checks.
pub struct CommandRequest {
    pub command: String,
    pub capabilities: Vec<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
/// Parsed reference advertisement arguments.
pub struct LsRefsRequest {
    pub symrefs: bool,
    pub peel: bool,
    pub unborn: bool,
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
/// Parsed fetch negotiation and pack options.
pub struct FetchRequest {
    pub wants: Vec<ObjectId>,
    pub haves: Vec<ObjectId>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub deepen_relative: bool,
    pub include_tags: bool,
    pub no_progress: bool,
    pub done: bool,
    pub thin_pack: bool,
    pub ofs_delta: bool,
    pub filter: UploadPackFilter,
}

/// Read one bounded protocol-v2 command without consuming the next request.
pub async fn read_command_request<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cancellation: &CancellationToken,
) -> Result<Option<CommandRequest>> {
    let closed = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(WireError::Cancelled),
        result = reader.fill_buf() => result?.is_empty(),
    };
    if closed {
        return Ok(None);
    }
    let first = read_packet(reader, cancellation).await?;
    let Packet::Data(first) = first else {
        return match first {
            Packet::Flush => Ok(None),
            Packet::Delimiter | Packet::ResponseEnd => {
                Err(protocol("request must start with command"))
            }
            Packet::Data(_) => Err(protocol("request packet was consumed twice")),
        };
    };
    let command = text_line(&first)?;
    let command = command
        .strip_prefix("command=")
        .ok_or_else(|| protocol("request is missing command="))?
        .to_owned();
    if command.is_empty()
        || !command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(protocol("invalid protocol-v2 command name"));
    }

    let mut capabilities = Vec::new();
    let mut args = Vec::new();
    let mut in_args = false;
    let mut packet_count = 1usize;
    let mut byte_count = first.len();
    loop {
        if packet_count >= MAX_REQUEST_PACKETS || byte_count > MAX_REQUEST_BYTES {
            return Err(protocol("protocol-v2 request exceeds bounds"));
        }
        let packet = read_packet(reader, cancellation).await?;
        packet_count += 1;
        match packet {
            Packet::Data(data) => {
                byte_count = byte_count.saturating_add(data.len());
                if byte_count > MAX_REQUEST_BYTES {
                    return Err(protocol("protocol-v2 request exceeds bounds"));
                }
                let line = text_line(&data)?.to_owned();
                if in_args {
                    args.push(line);
                } else {
                    capabilities.push(line);
                }
            }
            Packet::Delimiter => {
                if in_args {
                    return Err(protocol("duplicate protocol-v2 request delimiter"));
                }
                in_args = true;
            }
            Packet::Flush => {
                if !in_args {
                    return Err(protocol("protocol-v2 request is missing its delimiter"));
                }
                validate_request_capabilities(&capabilities)?;
                return Ok(Some(CommandRequest {
                    command,
                    capabilities,
                    args,
                }));
            }
            Packet::ResponseEnd => return Err(protocol("response-end is not valid in a request")),
        }
    }
}

fn validate_request_capabilities(capabilities: &[String]) -> Result<()> {
    let mut seen_agent = false;
    for capability in capabilities {
        if let Some(agent) = capability.strip_prefix("agent=") {
            if seen_agent
                || agent.is_empty()
                || agent.bytes().any(|byte| byte <= b' ' || byte >= 0x7f)
            {
                return Err(protocol(
                    "invalid or duplicate protocol-v2 agent capability",
                ));
            }
            seen_agent = true;
            continue;
        }
        return Err(protocol(format!(
            "protocol-v2 request capability was not advertised: {capability}"
        )));
    }
    Ok(())
}

/// Validate and parse reference advertisement arguments.
pub fn parse_ls_refs(args: &[String]) -> Result<LsRefsRequest> {
    let mut request = LsRefsRequest::default();
    let mut seen = HashSet::new();
    for arg in args {
        if let Some(prefix) = arg.strip_prefix("ref-prefix ") {
            if prefix.is_empty() || prefix.chars().any(char::is_whitespace) {
                return Err(protocol("ref-prefix must contain one non-empty value"));
            }
            request.prefixes.push(prefix.to_owned());
            continue;
        }
        match arg.as_str() {
            "symrefs" => request.symrefs = true,
            "peel" => request.peel = true,
            "unborn" => request.unborn = true,
            "ref-prefix" => return Err(protocol("ref-prefix is missing its value")),
            _ => return Err(protocol(format!("unsupported ls-refs argument: {arg}"))),
        }
        if !seen.insert(arg.clone()) {
            return Err(protocol(format!("duplicate ls-refs argument: {arg}")));
        }
    }
    Ok(request)
}

/// Validate and parse fetch arguments before repository planning.
pub fn parse_fetch(args: &[String]) -> Result<FetchRequest> {
    let mut request = FetchRequest::default();
    let mut seen_single = HashSet::new();
    let mut filter_count = 0usize;
    for arg in args {
        if request.done {
            return Err(protocol("fetch arguments follow done"));
        }
        let (key, value) = arg
            .split_once(' ')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "want" => request.wants.push(parse_oid(
                value.ok_or_else(|| protocol("want is missing its object ID"))?,
            )?),
            "have" => request.haves.push(parse_oid(
                value.ok_or_else(|| protocol("have is missing its object ID"))?,
            )?),
            "shallow" => request.shallow.push(parse_oid(
                value.ok_or_else(|| protocol("shallow is missing its object ID"))?,
            )?),
            "deepen" => {
                let raw = value.ok_or_else(|| protocol("deepen is missing its depth"))?;
                let depth = raw
                    .parse::<u32>()
                    .map_err(|_| protocol("invalid deepen depth"))?;
                if depth == 0 || request.deepen.replace(depth).is_some() {
                    return Err(protocol("duplicate or zero deepen depth"));
                }
            }
            "deepen-relative" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate deepen-relative argument"));
                }
                request.deepen_relative = true;
            }
            "thin-pack" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate thin-pack argument"));
                }
                request.thin_pack = true;
            }
            "no-progress" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate no-progress argument"));
                }
                request.no_progress = true;
            }
            "include-tag" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate include-tag argument"));
                }
                request.include_tags = true;
            }
            "ofs-delta" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate ofs-delta argument"));
                }
                request.ofs_delta = true;
            }
            "sideband-all" => return Err(protocol("sideband-all was not advertised")),
            "done" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate done argument"));
                }
                request.done = true;
            }
            "filter" => {
                filter_count = filter_count.saturating_add(1);
                if filter_count > 16 {
                    return Err(protocol("fetch request contains too many filters"));
                }
                let value = value.ok_or_else(|| protocol("filter is missing its specification"))?;
                let parsed =
                    parse_upload_pack_filter(value).map_err(|error| protocol(error.to_string()))?;
                let previous = std::mem::take(&mut request.filter);
                request.filter = combine_upload_pack_filters(
                    [previous, parsed]
                        .into_iter()
                        .filter(|filter| !matches!(filter, UploadPackFilter::None))
                        .collect(),
                );
            }
            "deepen-since" | "deepen-not" | "want-ref" | "packfile-uris" | "wait-for-done"
            | "server-option" => {
                return Err(protocol(format!("unsupported fetch argument: {key}")));
            }
            _ => return Err(protocol(format!("unsupported fetch argument: {arg}"))),
        }
    }
    if request.wants.is_empty() {
        return Err(protocol("fetch request contains no wants"));
    }
    if request.deepen_relative && request.deepen.is_none() {
        return Err(protocol("deepen-relative requires deepen"));
    }
    Ok(request)
}

async fn read_packet<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cancellation: &CancellationToken,
) -> Result<Packet> {
    let mut header = [0u8; 4];
    read_exact_cancellable(reader, &mut header, cancellation).await?;
    let length = u16::from_str_radix(
        std::str::from_utf8(&header).map_err(|_| protocol("packet length is not ASCII"))?,
        16,
    )
    .map_err(|_| protocol("packet length is not hexadecimal"))? as usize;
    match length {
        0 => Ok(Packet::Flush),
        1 => Ok(Packet::Delimiter),
        2 => Ok(Packet::ResponseEnd),
        3 => Err(protocol("invalid packet-line length 0003")),
        length if !(4..=MAX_PACKET_BYTES).contains(&length) => {
            Err(protocol("packet-line length exceeds the protocol bound"))
        }
        length => {
            let decoded = gix_packetline::decode::hex_prefix(&header)
                .map_err(|_| protocol("invalid packet-line header"))?;
            let PacketLineOrWantedSize::Wanted(wanted) = decoded else {
                return Err(protocol("packet-line header changed while decoding"));
            };
            if usize::from(wanted) != length - 4 {
                return Err(protocol("packet-line length changed while decoding"));
            }
            let mut data = vec![0u8; length - 4];
            read_exact_cancellable(reader, &mut data, cancellation).await?;
            if !matches!(
                gix_packetline::decode::to_data_line(&data),
                Ok(PacketLineRef::Data(_))
            ) {
                return Err(protocol("packet-line data exceeds the protocol bound"));
            }
            Ok(Packet::Data(data))
        }
    }
}

fn text_line(data: &[u8]) -> Result<&str> {
    let data = data.strip_suffix(b"\n").unwrap_or(data);
    if data.contains(&b'\n') {
        return Err(protocol("protocol-v2 line contains an embedded LF"));
    }
    std::str::from_utf8(data).map_err(|_| protocol("packet-line data is not UTF-8"))
}

fn parse_oid(value: &str) -> Result<ObjectId> {
    if value.len() != 40 {
        return Err(protocol("object ID must contain exactly 40 hex digits"));
    }
    ObjectId::from_hex(value.as_bytes()).map_err(|_| protocol("invalid object ID"))
}

/// Write a bounded packet, optionally prefixed by a sideband channel.
pub async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
    band: Option<u8>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let payload_len = data.len() + usize::from(band.is_some());
    let length = payload_len.saturating_add(4);
    if length > MAX_PACKET_BYTES {
        return Err(protocol("packet-line payload exceeds the protocol bound"));
    }
    write_all_cancellable(writer, format!("{length:04x}").as_bytes(), cancellation).await?;
    if let Some(band) = band {
        write_all_cancellable(writer, &[band], cancellation).await?;
    }
    write_all_cancellable(writer, data, cancellation).await?;
    Ok(())
}

/// Write and flush the end-of-section marker.
pub async fn write_flush<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0000", cancellation).await?;
    flush_cancellable(writer, cancellation).await?;
    Ok(())
}

/// Write the separator between response sections.
pub async fn write_delimiter<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0001", cancellation).await?;
    Ok(())
}

/// Write and flush the end-of-response marker.
pub async fn write_response_end<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0002", cancellation).await?;
    flush_cancellable(writer, cancellation).await?;
    Ok(())
}

async fn read_exact_cancellable<R: AsyncRead + Unpin>(
    reader: &mut R,
    bytes: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(WireError::Cancelled),
        result = reader.read_exact(bytes) => {
            result.map(|_| ()).map_err(Into::into)
        }
    }
}

/// Write bytes while observing request cancellation.
pub async fn write_all_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(WireError::Cancelled),
        result = writer.write_all(bytes) => result.map_err(Into::into),
    }
}

/// Flush pending output while observing request cancellation.
pub async fn flush_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(WireError::Cancelled),
        result = writer.flush() => result.map_err(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;
    fn packet(data: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{:04x}", data.len() + 4).into_bytes();
        bytes.extend_from_slice(data);
        bytes
    }
    #[test]
    fn combines_repeated_fetch_filters() {
        let request = parse_fetch(&[
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            "filter blob:none".to_owned(),
            "filter tree:1".to_owned(),
        ])
        .expect("repeated filters should use intersection semantics");
        assert_eq!(request.filter.canonical_spec(), "combine:blob:none+tree:1");
    }

    #[test]
    fn accepts_supported_filter_grammar_before_planning() {
        for filter in [
            "blob:limit=1m",
            "tree:1",
            "object:type=blob",
            "sparse:oid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "combine:blob%3Anone+tree%3A1",
        ] {
            parse_fetch(&[
                format!("want {}", "a".repeat(40)),
                format!("filter {filter}"),
            ])
            .expect("supported filters must parse before planning");
        }
    }

    #[test]
    fn rejects_unsupported_filter_before_planning() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "filter blob:depth=1".to_owned(),
        ])
        .expect_err("unsupported filters must fail in the wire parser");
        assert!(error.to_string().contains("unsupported filter"));
    }

    #[test]
    fn parses_ref_prefix_arguments_with_inline_values() {
        let request = parse_ls_refs(&[
            "symrefs".to_owned(),
            "ref-prefix refs/heads/".to_owned(),
            "ref-prefix refs/tags/".to_owned(),
        ])
        .expect("inline ref-prefix values should parse");
        assert!(request.symrefs);
        assert_eq!(request.prefixes, ["refs/heads/", "refs/tags/"]);
    }

    #[tokio::test]
    async fn command_request_requires_capability_delimiter() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let error = read_command_request(&mut reader, &cancellation)
            .await
            .expect_err("missing delimiter must be rejected");
        assert!(error.to_string().contains("missing its delimiter"));
    }

    #[tokio::test]
    async fn closed_v2_session_is_not_an_early_eof_error() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let cancellation = CancellationToken::new();
        assert!(
            read_command_request(&mut reader, &cancellation)
                .await
                .expect("clean close should be accepted")
                .is_none()
        );
    }

    #[tokio::test]
    async fn command_request_keeps_pkt_line_bytes_after_terminal_request() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0001");
        bytes.extend_from_slice(&packet(b"symrefs\n"));
        bytes.extend_from_slice(b"0000tail");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let request = read_command_request(&mut reader, &cancellation)
            .await
            .expect("request should parse")
            .expect("request should be present");
        assert_eq!(request.command, "ls-refs");
        assert!(request.capabilities.is_empty());
        assert_eq!(request.args, ["symrefs"]);

        let mut tail = Vec::new();
        reader
            .read_to_end(&mut tail)
            .await
            .expect("tail should remain readable");
        assert_eq!(tail, b"tail");
    }

    #[tokio::test]
    async fn command_request_accepts_large_promisor_want_batch() {
        let mut bytes = packet(b"command=fetch\n");
        bytes.extend_from_slice(b"0001");
        for index in 0..10_000 {
            bytes.extend_from_slice(&packet(format!("want {index:040x}\n").as_bytes()));
        }
        bytes.extend_from_slice(&packet(b"filter blob:none\n"));
        bytes.extend_from_slice(&packet(b"done\n"));
        bytes.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let request = read_command_request(&mut reader, &cancellation)
            .await
            .expect("large promisor request should stay within the byte bound")
            .expect("request should be present");
        assert_eq!(request.args.len(), 10_002);
    }

    #[tokio::test]
    async fn request_byte_limit_is_enforced_before_flush() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0001");
        let oversized = vec![b'a'; MAX_PACKET_BYTES - 4];
        for _ in 0..65 {
            bytes.extend_from_slice(&packet(&oversized));
        }
        bytes.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let error = read_command_request(&mut reader, &cancellation)
            .await
            .expect_err("oversized requests must fail before the flush packet");
        assert!(error.to_string().contains("exceeds bounds"));
    }

    #[test]
    fn fetch_done_must_be_the_final_argument() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "done".to_owned(),
            "no-progress".to_owned(),
        ])
        .expect_err("arguments after done must be rejected");
        assert!(error.to_string().contains("follow done"));
    }

    #[test]
    fn relative_deepen_requires_a_depth() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "deepen-relative".to_owned(),
        ])
        .expect_err("relative deepen without depth must be rejected");
        assert!(error.to_string().contains("requires deepen"));
    }
    #[test]
    fn parses_object_id_strictly() {
        assert!(parse_oid(&"a".repeat(40)).is_ok());
        assert!(parse_oid(&"a".repeat(39)).is_err());
        assert!(parse_oid(&format!("{}z", "a".repeat(39))).is_err());
    }

    #[test]
    fn accepts_optional_terminal_line_feed_and_rejects_embedded_lf() {
        assert_eq!(
            text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("a packet line without a terminal line feed should be accepted"),
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nextra\n").is_err());
        assert_eq!(
            text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
                .expect("a terminal line feed should be accepted"),
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }
}
