use std::collections::HashMap;
use std::io;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

use crate::packet::{Header, Packet, PacketStatus, PendingRead};
use crate::{constants::*, AckRes, UdxStream};
use crate::{StreamRef, UdxBuf};

use crate::mutex::Mutex;

pub struct UdxHandle {
    inner: Arc<Mutex<UdxSocket>>,
}

pub struct SocketRef(Arc<Mutex<UdxSocket>>);

// pub struct UdpSocket {
//     watcher: Async<std::net::UdpSocket>,
// }

impl SocketRef {
    pub fn new(socket: UdxSocket) -> Self {
        Self(Arc::new(Mutex::new(socket)))
    }

    // fn stable_id(&self) -> usize {
    //     &*self.0 as *const _ as usize
    // }

    pub fn connect(&mut self, local_id: u32) -> io::Result<StreamRef> {
        let mut socket = self.0.lock("connect");
        if socket.streams_len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Too many streams open",
            ));
        }
        let stream = UdxStream::new(local_id);
        let stream = StreamRef::new(stream);
        socket.streams_by_id.insert(local_id, stream.clone());
        Ok(stream)
    }
}

impl Clone for SocketRef {
    fn clone(&self) -> Self {
        // self.lock("clone").ref_count += 1;
        Self(self.0.clone())
    }
}

impl std::ops::Deref for SocketRef {
    type Target = Mutex<UdxSocket>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct UdxSocket {
    socket: UdpSocket,
    send_queue: VecDeque<Packet>,
    status: u32,
    readers: u32,
    events: u32,
    ttl: u32,
    pending_closes: u32,

    streams_len: usize,
    streams_max_len: usize,

    // streams: Vec<Arc<UdxStream>>,
    streams_by_id: HashMap<u32, StreamRef>,

    read_buf: Vec<u8>,
    write_buf: Vec<u8>, // pub(crate) write_wakers: VecDeque<Waker>,
}

impl UdxSocket {
    pub fn streams_len(&self) -> usize {
        self.streams_len
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn bind(addr: SocketAddr) -> Result<Self, io::Error> {
        let ttl = UDX_DEFAULT_TTL;
        let socket = UdpSocket::bind(addr).await?;
        socket.set_ttl(ttl)?;
        Ok(Self {
            socket,
            send_queue: VecDeque::with_capacity(16),
            status: 0 | UDX_SOCKET_BOUND,
            readers: 0,
            events: 0,
            pending_closes: 0,
            ttl,
            streams_len: 0,
            streams_max_len: 16,
            // streams: Vec::with_capacity(16),
            streams_by_id: HashMap::new(),

            read_buf: vec![0u8; 2048],
            write_buf: vec![0u8; 2048],
        })
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Read from the socket.
        let mut buf = ReadBuf::new(&mut self.read_buf);
        let incoming = match self.socket.poll_recv_from(cx, &mut buf) {
            Poll::Pending => None,
            Poll::Ready(Ok(peer)) => Some((buf.filled().len(), peer)),
            Poll::Ready(Err(err)) => None,
        };
        if let Some((n, peer)) = incoming {
            match self.process_incoming(n) {
                Ok(ProcessRes::Handled) => {}
                Ok(ProcessRes::NotHandled) => {
                    self.on_unhandled_packet(&self.read_buf[..n])?;
                }
                Err(_err) => {}
            }
        }

        // Write packets out.
        while let Some(mut packet) = self.send_queue.get_mut(0) {
            assert!(matches!(packet.status, PacketStatus::Sending));
            // TODO: This should update the status within stream.outgoing, not here.
            packet.status = PacketStatus::Inflight;
            packet.transmits += 1;

            self.write_buf[0..].copy_from_slice(&packet.bufs[0]);
            self.write_buf[packet.bufs[0].len()..].copy_from_slice(&packet.bufs[1]);
            let len = packet.bufs[0].len() + packet.bufs[1].len();

            match self
                .socket
                .poll_send_to(cx, &self.write_buf[..len], packet.dest)
            {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n < len {
                        break;
                    }
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            };
            // self.socket
            //     .send_to(&self.write_buf[0..len], packet.dest)
            //     .await?;

            let _ = self.send_queue.pop_front();
        }

        Poll::Pending
    }

    // async fn drive(&mut self) -> io::Result<()> {
    //     tokio::select! {
    //         res = self.recv_and_process().await => {
    //             res?;
    //         }
    //         res = self.send_queue().await => {
    //             res?;
    //         }
    //     }
    // }

    // async fn recv_and_process(&mut self) -> io::Result<()> {
    //     let (bytes, peer) = self.socket.recv_from(&mut self.read_buf).await?;
    //     match self.process_incoming(bytes)? {
    //         ProcessRes::Handled => {}
    //         ProcessRes::NotHandled => {
    //             self.on_unhandled_packet(&self.read_buf[..bytes])?;
    //         }
    //     }
    //     Ok(())
    // }

    async fn send_queue(&mut self) -> io::Result<()> {
        while let Some(mut packet) = self.send_queue.get_mut(0) {
            assert!(matches!(packet.status, PacketStatus::Sending));
            // TODO: This should update the status within stream.outgoing, not here.
            packet.status = PacketStatus::Inflight;
            packet.transmits += 1;

            self.write_buf[0..].copy_from_slice(&packet.bufs[0]);
            self.write_buf[packet.bufs[0].len()..].copy_from_slice(&packet.bufs[1]);
            let len = packet.bufs[0].len() + packet.bufs[1].len();

            self.socket
                .send_to(&self.write_buf[0..len], packet.dest)
                .await?;

            let _ = self.send_queue.pop_front();
        }
        Ok(())
    }

    fn process_incoming(&mut self, bytes: usize) -> io::Result<ProcessRes> {
        let header = match Header::from_bytes(&self.read_buf[..bytes]) {
            // let header = match Header::from_bytes(&bytes) {
            Some(header) => header,
            None => return Ok(ProcessRes::NotHandled),
        };
        let stream = match self.streams_by_id.get_mut(&header.local_id) {
            Some(stream) => stream,
            None => {
                // self.invoke_preconnect();
                // and match again.
                return Ok(ProcessRes::NotHandled);
            }
        };
        let mut stream = stream.lock("process_incoming");
        if stream.status & UDX_STREAM_DEAD > 0 {
            return Ok(ProcessRes::NotHandled);
        }

        if header.r#type & UDX_HEADER_SACK > 0 {
            // process_sacks(stream, buf, buf_len)
        }

        let mut offset = UDX_HEADER_SIZE;
        let mut len = bytes - UDX_HEADER_SIZE;

        // Done with header processing now.
        // For future compat, make sure we are now pointing at the actual data using the data_offset
        if header.data_offset > 0 {
            if header.data_offset > len {
                return Ok(ProcessRes::NotHandled);
            }
            offset = header.data_offset;
            len += header.data_offset;
        }

        // For all stream packets, ensure that they are causally newer (or same)
        if seq_compare(stream.ack, header.seq) <= 0 {
            if header.r#type & UDX_HEADER_DATA_OR_END > 0
                && !stream.incoming.contains_key(&header.seq)
                && stream.status & UDX_STREAM_SHOULD_READ == UDX_STREAM_READ
            {
                let buf = self.read_buf[offset..(offset + len)].to_vec();
                stream.process_incoming_data_packet(&header, buf);
            }

            if header.r#type & UDX_HEADER_END > 0 {
                stream.status |= UDX_STREAM_ENDING_REMOTE;
                stream.remote_ended = header.seq;
            }

            if header.r#type & UDX_HEADER_DESTROY > 0 {
                stream.status |= UDX_STREAM_DESTROYED_REMOTE;
                // clear_outgoing_packets(stream)
                // close_maybe(stream)
                return Ok(ProcessRes::Handled);
            }
        }

        if header.r#type & UDX_HEADER_MESSAGE > 0 {
            let buf = self.read_buf[offset..(offset + len)].to_vec();
            stream.on_recv(buf);
        }

        // process the read queue
        while (stream.status & UDX_STREAM_SHOULD_READ) == UDX_STREAM_READ {
            let ack = stream.ack;
            let packet = stream.incoming.remove(&ack);
            match packet {
                None => break,
                Some(packet) => {
                    stream.pkts_buffered -= 1;
                    stream.ack += 1;

                    if packet.r#type & UDX_HEADER_DATA > 1 {
                        stream.on_read(Some(packet.buf));
                    }
                }
            }
        }

        // Check if the ack is oob.
        if seq_compare(stream.seq, header.ack) < 0 {
            return Ok(ProcessRes::Handled);
        }

        // Congestion control...
        if stream.remote_acked != header.ack {
            if stream.cwnd < stream.ssthresh {
                stream.cwnd += UDX_MTU;
            } else {
                stream.cwnd += ((UDX_MTU * UDX_MTU) / stream.cwnd).max(1);
            }
        } else if header.r#type & UDX_HEADER_DATA_OR_END == 0 {
            stream.dup_acks += 1;
            if stream.dup_acks >= 3 {
                stream.fast_retransmit();
            }
        }

        let len = seq_diff(header.ack, stream.remote_acked);
        for _i in 0..len {
            let seq = stream.remote_acked;
            stream.remote_acked += 1;
            match stream.ack_packet(seq, false) {
                AckRes::Acked => continue,
                AckRes::Ended => {
                    // it ended, so ack that and trigger close
                    // TODO: make this work as well, if the ack packet is lost, ie
                    // have some internal (capped) queue of "gracefully closed" streams
                    stream.send_state_packet();
                    stream.close_maybe(None);
                    return Ok(ProcessRes::Handled);
                }
                AckRes::NotFound => {
                    return Ok(ProcessRes::Handled);
                }
            }
        }
        // if data pkt, send an ack - use deferred acks as well...
        if header.r#type & UDX_HEADER_DATA_OR_END > 0 {
            stream.send_state_packet();
        }

        if stream.status & UDX_STREAM_SHOULD_END_REMOTE == UDX_STREAM_END_REMOTE
            && seq_compare(stream.remote_ended, stream.ack) <= 0
        {
            stream.on_read(None);
            if stream.close_maybe(None) {
                return Ok(ProcessRes::Handled);
            }
        }

        if stream.pkts_waiting > 0 {
            stream.check_timeouts()?;
        }

        Ok(ProcessRes::Handled)
    }

    /* async fn on_incoming(&mut self, header: Header) -> bool { */
    /*     let data = &self.read_buf[UDX_HEADER_SIZE..]; */
    /*     let stream = self.streams_by_id.get(&header.local_id); */
    /*     if stream.is_none() { */
    /*         return false; */
    /*     } */
    /*     let stream = stream.unwrap(); */
    /*     true */
    /* } */

    fn on_unhandled_packet(&self, buf: &[u8]) -> io::Result<()> {
        // TODO: What to do with non-stream packets?
        Ok(())
    }

    pub fn queue_send(&mut self, packet: Packet) {
        self.send_queue.push_back(packet)
    }

    pub fn send(&self, req: SendRequest) {}
    pub fn send_ttl(&mut self, mut req: SendRequest, buf: UdxBuf, dest: SocketAddr, ttl: u32) {
        {
            let pkt = &mut req.pkt;
            pkt.status = PacketStatus::Sending;
            pkt.r#type = UDX_PACKET_SEND;
            pkt.ttl = ttl;
            pkt.dest = dest;
            pkt.transmits = 0;
            pkt.bufs_len = 1;
            pkt.bufs[0] = buf;
        }
        self.send_queue.push_back(req.pkt);
    }
}

pub struct SendRequest {
    pkt: Packet,
    data: u32, // request id
}

fn seq_diff(a: u32, b: u32) -> i32 {
    (a as i32) - (b as i32)
}

fn seq_compare(a: u32, b: u32) -> i8 {
    let d = seq_diff(a, b);
    if d < 0 {
        -1
    } else if d > 0 {
        1
    } else {
        0
    }
}

pub enum ProcessRes {
    Handled,
    NotHandled,
}
