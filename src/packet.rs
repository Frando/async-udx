use bytes::Bytes;
use std::fmt::{self, Debug};
use std::sync::Arc;
use std::time::Instant;
use std::{io, net::SocketAddr, sync::atomic::AtomicUsize};

use crate::constants::{UDX_HEADER_SIZE, UDX_MAGIC_BYTE, UDX_VERSION};

#[derive(Debug)]
pub(crate) enum PacketRef {
    Owned(Packet),
    Shared(Arc<Packet>),
}

impl std::ops::Deref for PacketRef {
    type Target = Packet;
    fn deref(&self) -> &Self::Target {
        match self {
            PacketRef::Owned(packet) => &packet,
            PacketRef::Shared(packet) => &packet,
        }
    }
}

pub enum PacketBuf {
    Data(Vec<u8>),
    HeaderOnly([u8; 20]),
}
impl PacketBuf {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Data(vec) => &vec[..],
            Self::HeaderOnly(array) => &array[..],
        }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Data(vec) => &mut vec[..],
            Self::HeaderOnly(array) => &mut array[..],
        }
    }
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }
}

pub(crate) struct Packet {
    pub time_sent: Instant,
    pub transmits: AtomicUsize,
    pub dest: SocketAddr,
    pub header: Header,
    pub buf: PacketBuf,
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packet")
            .field("transmits", &self.transmits)
            .field("dest", &self.dest)
            .field("header", &self.header)
            .field("buf(len)", &self.buf.len())
            .finish()
    }
}

impl Packet {
    // pub fn has_type(&self, typ: u32) -> bool {
    //     self.header.typ & typ != 0
    // }

    pub fn new(dest: SocketAddr, header: Header, body: &[u8]) -> Self {
        let mut buf = if body.is_empty() {
            PacketBuf::HeaderOnly([0u8; 20])
        } else {
            let len = UDX_HEADER_SIZE + body.len();
            PacketBuf::Data(vec![0u8; len])
        };
        header.encode(&mut buf.as_mut_slice()[..UDX_HEADER_SIZE]);
        if !body.is_empty() {
            buf.as_mut_slice()[UDX_HEADER_SIZE..].copy_from_slice(body);
        }
        Self {
            time_sent: Instant::now(),
            transmits: AtomicUsize::new(0),
            dest,
            header,
            buf,
        }
    }

    pub fn seq(&self) -> u32 {
        self.header.seq
    }
}

pub(crate) struct IncomingPacket {
    pub header: Header,
    pub buf: Bytes,
    pub read_offset: usize,
}

impl fmt::Debug for IncomingPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "IncomingPacket(header {:?}, buf len {})",
            self.header,
            self.buf.len()
        )
    }
}

impl IncomingPacket {
    pub fn ack(&self) -> u32 {
        self.header.ack
    }
    pub fn seq(&self) -> u32 {
        self.header.seq
    }
    pub fn has_type(&self, typ: u32) -> bool {
        self.header.typ & typ != 0
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub typ: u32,
    pub data_offset: usize,
    pub stream_id: u32,
    pub recv_win: u32,
    pub seq: u32,
    pub ack: u32,
}

impl Header {
    const SIZE: usize = UDX_HEADER_SIZE;
    // pub fn has_typ(&self, typ: u32) -> bool {
    //     self.typ & typ != 0
    // }
    pub fn from_bytes(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < UDX_HEADER_SIZE || buf[0] != UDX_MAGIC_BYTE || buf[1] != UDX_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bad header"));
        }

        let typ = buf[2] as u32;
        let data_offset = buf[3];
        let local_id = read_u32_le(&buf[4..8]);
        let recv_win = read_u32_le(&buf[8..12]);
        let seq = read_u32_le(&buf[12..16]);
        let ack = read_u32_le(&buf[16..20]);
        Ok(Self {
            typ,
            data_offset: data_offset as usize,
            recv_win,
            stream_id: local_id,
            seq,
            ack,
        })
    }

    // pub fn to_vec(&self) -> Vec<u8> {
    //     let mut buf = vec![0u8; Self::SIZE];
    //     self.encode(&mut buf);
    //     buf
    // }

    pub fn encode(&self, buf: &mut [u8]) -> bool {
        if buf.len() < Self::SIZE {
            return false;
        }
        buf[0] = UDX_MAGIC_BYTE;
        buf[1] = UDX_VERSION;
        buf[2..3].copy_from_slice(&(self.typ as u8).to_le_bytes());
        buf[3..4].copy_from_slice(&(self.data_offset as u8).to_le_bytes());
        buf[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.recv_win.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..20].copy_from_slice(&self.ack.to_le_bytes());
        true
    }
}

fn read_u32_le(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf.try_into().unwrap())
}
