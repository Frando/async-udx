use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU32, Arc},
};

use crate::{constants::*, StreamRef, UdxBuf, UdxSocket, UdxStream};

pub struct PendingRead {
    pub seq: u32,
    pub r#type: u32,
    pub buf: Vec<u8>,
}

pub enum PacketStatus {
    Waiting,
    Sending,
    Inflight,
}

pub struct Packet {
    pub(crate) seq: u32,
    pub(crate) status: PacketStatus,
    pub(crate) r#type: u32,
    pub(crate) ttl: u32,

    pub(crate) fifo_gc: u32,
    pub(crate) transmits: u8,
    pub(crate) size: u16,
    pub(crate) time_sent: u64,

    pub(crate) dest: SocketAddr,

    // pub(crate) header: [u8; UDX_HEADER_SIZE],
    pub(crate) bufs_len: usize,
    pub(crate) bufs: [UdxBuf; 2],

    pub(crate) ctx: PacketContext,
}

impl Packet {
    pub fn new_stream(r#type: u32, stream: &UdxStream, buf: Vec<u8>) -> Self {
        let header = Header {
            r#type,
            data_offset: 0,
            local_id: stream.remote_id,
            recv_win: u32::MAX,
            seq: stream.seq,
            ack: stream.ack,
        };
        let header_buf = header.to_vec();
        let packet = Packet {
            seq: stream.seq,
            transmits: 0,
            size: (Header::SIZE + buf.len()) as u16,
            dest: stream.remote_addr.unwrap(), // TODO: No unwrap here.
            bufs_len: 2,
            bufs: [header_buf, buf],
            // "unset" props
            r#type: 0,
            ctx: PacketContext::None,
            time_sent: 0,
            fifo_gc: 0,
            ttl: 0,
            // TODO: Better set to None here?
            status: PacketStatus::Waiting,
        };
        packet
    }
}

pub enum PacketContext {
    None,
    StreamWrite(Arc<PktStreamWrite>),
    StreamSend(Arc<PktStreamSend>),
    // StreamDestroy(PktStream)
    Send(PktSend),
}

pub struct PktSend {
    pub packet: Box<Packet>,
    pub handle: UdxSocket,
    pub on_send: Option<OnSendCallback>,
}

pub struct PktStreamWrite {
    pub packets: AtomicU32,
    pub handle: StreamRef,
    pub on_ack: Option<OnAckCallback>,
    pub data: Vec<u8>,
}

pub struct PktStreamSend {
    pub pkt: Box<Packet>,
    // handle: UdxStreamHandle,
    pub on_send: Option<OnStreamSendCallback>,
}

pub type OnAckCallback = Box<dyn Fn()>;
pub type OnSendCallback = Box<dyn Fn()>;
pub type OnStreamSendCallback = Box<dyn Fn()>;

pub struct Header {
    pub r#type: u32,
    pub data_offset: usize,
    pub local_id: u32,
    pub recv_win: u32,
    pub seq: u32,
    pub ack: u32,
}

impl Header {
    const SIZE: usize = UDX_HEADER_SIZE;
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < UDX_HEADER_SIZE {
            return None;
        }

        if buf[0] != UDX_MAGIC_BYTE || buf[1] != UDX_VERSION {
            return None;
        }

        let r#type = buf[2] as u32;
        let data_offset = buf[3];

        let local_id = read_u32_le(&buf[4..8]);
        let recv_win = read_u32_le(&buf[8..12]);
        let seq = read_u32_le(&buf[12..16]);
        let ack = read_u32_le(&buf[16..20]);

        // let data = buf[20..];
        Some(Self {
            r#type,
            data_offset: data_offset as usize,
            recv_win,
            local_id,
            seq,
            ack,
        })
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut buf = vec![0u8; Self::SIZE];
        self.encode(&mut buf);
        buf
    }

    pub fn encode(&self, buf: &mut [u8]) -> bool {
        if buf.len() < Self::SIZE {
            return false;
        }
        buf[0] = UDX_MAGIC_BYTE;
        buf[1] = UDX_VERSION;
        buf[2..3].copy_from_slice(&self.r#type.to_le_bytes());
        buf[3..4].copy_from_slice(&self.data_offset.to_le_bytes());
        buf[4..8].copy_from_slice(&self.local_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.recv_win.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..20].copy_from_slice(&self.ack.to_le_bytes());
        return true;
    }
}

impl Packet {
    pub fn read(_buf: &[u8]) -> Option<Self> {
        None
    }
}

fn read_u32_le(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf.try_into().unwrap())
}

/* fn write_u32_le */
