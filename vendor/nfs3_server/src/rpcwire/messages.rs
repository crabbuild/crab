use std::io::Cursor;

use anyhow::bail;
use nfs3_types::rpc::{
    accept_stat_data, accepted_reply, call_body, fragment_header, msg_body, opaque_auth,
    rejected_reply, reply_body, rpc_msg,
};
use nfs3_types::xdr_codec::{Pack, Unpack, Void};
use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Debug)]
pub enum PackedRpcMessage {
    Incomplete(IncompleteRpcMessage),
    Complete(CompleteRpcMessage),
}

impl PackedRpcMessage {
    pub fn new() -> Self {
        Self::Incomplete(IncompleteRpcMessage::default())
    }
    pub async fn recv(&mut self, input: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<bool> {
        match self {
            Self::Incomplete(incomplete) => {
                let eof = incomplete.recv(input).await?;
                if eof {
                    let data = std::mem::take(incomplete);
                    *self = Self::Complete(CompleteRpcMessage(data.0));
                }
                Ok(eof)
            }
            Self::Complete(_) => Ok(true),
        }
    }
}

/// Contains collected RPC message fragments without their headers.
#[derive(Default, Debug)]
pub struct IncompleteRpcMessage(Vec<u8>);

impl IncompleteRpcMessage {
    async fn recv(&mut self, input: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<bool> {
        let mut header_buf = [0_u8; 4];
        input.read_exact(&mut header_buf).await?;
        let header: fragment_header = header_buf.into();
        let prev_length = self.0.len();
        let fragment_length = header.fragment_length() as usize;
        self.0.resize(prev_length + fragment_length, 0);
        input.read_exact(&mut self.0[prev_length..]).await?;

        Ok(header.eof())
    }
}

#[derive(Debug)]
pub struct CompleteRpcMessage(Vec<u8>);

impl CompleteRpcMessage {
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

pub struct IncomingRpcMessage {
    xid: u32,
    body: call_body<'static>,
    data: Option<Vec<u8>>,
    message_start: usize, // offset of the start of the message in the data buffer
}

impl TryFrom<CompleteRpcMessage> for IncomingRpcMessage {
    type Error = anyhow::Error;

    fn try_from(packed: CompleteRpcMessage) -> Result<Self, Self::Error> {
        let packed = packed.0;
        let (rpc, pos) = match rpc_msg::unpack(&mut Cursor::new(&packed)) {
            Ok(ok) => ok,
            Err(err) => {
                bail!("Failed to unpack RPC message: {err}");
            }
        };

        let xid = rpc.xid;
        let body = match rpc.body {
            msg_body::CALL(call) => call,
            msg_body::REPLY(_) => {
                bail!("Expected a CALL message, got REPLY. XID: {xid}");
            }
        };

        Ok(Self {
            xid,
            body,
            data: Some(packed),
            message_start: pos,
        })
    }
}

impl IncomingRpcMessage {
    pub const fn xid(&self) -> u32 {
        self.xid
    }
    pub const fn body(&self) -> &call_body<'static> {
        &self.body
    }

    pub const fn take_data(&mut self) -> Cursor<Vec<u8>> {
        let data = self.data.take().expect("Data already taken");
        let mut cursor = Cursor::new(data);
        cursor.set_position(self.message_start as u64);
        cursor
    }

    pub fn into_success_reply<T>(self, message: &T) -> anyhow::Result<HandleResult>
    where
        T: Pack + 'static,
    {
        let rpc = rpc_msg {
            xid: self.xid,
            body: msg_body::REPLY(reply_body::MSG_ACCEPTED(accepted_reply {
                verf: opaque_auth::default(),
                reply_data: accept_stat_data::SUCCESS,
            })),
        };

        pack(&rpc, message).map(HandleResult::Reply)
    }

    pub fn into_rpc_mismatch(self) -> anyhow::Result<HandleResult> {
        use nfs3_types::rpc::RPC_VERSION_2;

        let reply =
            reply_body::MSG_DENIED(rejected_reply::rpc_mismatch(RPC_VERSION_2, RPC_VERSION_2));

        let rpc = rpc_msg {
            xid: self.xid,
            body: msg_body::REPLY(reply),
        };
        pack(&rpc, &Void).map(HandleResult::Reply)
    }

    pub fn into_error_reply(self, err: accept_stat_data) -> anyhow::Result<HandleResult> {
        let rpc = rpc_msg {
            xid: self.xid,
            body: msg_body::REPLY(reply_body::MSG_ACCEPTED(accepted_reply {
                verf: opaque_auth::default(),
                reply_data: err,
            })),
        };
        pack(&rpc, &Void).map(HandleResult::Reply)
    }
}

pub enum HandleResult {
    Reply(CompleteRpcMessage),
    NoReply,
}

fn pack<T>(rpc: &rpc_msg, message: &T) -> anyhow::Result<CompleteRpcMessage>
where
    T: Pack + 'static,
{
    let size = rpc
        .packed_size()
        .checked_add(message.packed_size())
        .expect("Failed to calculate size");

    let mut cursor = Cursor::new(Vec::with_capacity(size));
    rpc.pack(&mut cursor)?;
    message.pack(&mut cursor)?;
    Ok(CompleteRpcMessage(cursor.into_inner()))
}
