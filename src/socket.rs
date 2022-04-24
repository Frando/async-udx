use std::collections::HashMap;
use std::fmt;
use std::io;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use tokio::io::ReadBuf;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::task::JoinHandle;
use tokio::time::Interval;

use crate::get_milliseconds;
use crate::mutex::Mutex;
use crate::packet::{Header, Packet, PacketRef, PacketStatus};
use crate::SendRes;
use crate::{constants::*, AckRes, UdxStreamInner};
use crate::{UdxBuf, UdxStream};

pub struct UdxSocket(Arc<Mutex<UdxSocketInner>>);

impl fmt::Debug for UdxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SocketRef")
    }
}

impl UdxSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let inner = UdxSocketInner::bind(addr).await?;
        let socket = Self(Arc::new(Mutex::new(inner)));
        socket.clone().drive();
        Ok(socket)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.lock("socket:local_addr").local_addr()
    }

    fn drive(&self) -> JoinHandle<io::Result<()>> {
        let this = self.clone();
        tokio::task::spawn(async move {
            loop {
                let res = futures::future::poll_fn(|cx| {
                    let res = this.0.lock("sock:outer poll_fn").poll_drive(cx);
                    res
                })
                .await;
                res?;
            }
        })
    }

    pub fn close(&self) {
        self.0.lock("sock:close").status |= UDX_SOCKET_CLOSING;
    }

    // fn stable_id(&self) -> usize {
    //     &*self.0 as *const _ as usize
    // }

    pub fn connect(
        &self,
        dest: SocketAddr,
        local_id: u32,
        remote_id: u32,
    ) -> io::Result<UdxStream> {
        let mut socket = self.0.lock("sock:connect");
        if socket.streams_len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Too many streams open",
            ));
        }
        let mut stream = UdxStreamInner::new(local_id);
        let local_addr = socket.local_addr().unwrap();
        stream.connect(self.clone(), local_addr, dest, remote_id)?;
        let stream = UdxStream::new(stream);
        socket.streams.insert(local_id, stream.clone());
        Ok(stream)
    }
}

impl Clone for UdxSocket {
    fn clone(&self) -> Self {
        // self.lock("clone").ref_count += 1;
        Self(self.0.clone())
    }
}

impl std::ops::Deref for UdxSocket {
    type Target = Mutex<UdxSocketInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct UdxSocketInner {
    socket: UdpSocket,
    send_queue: VecDeque<Packet>,
    status: u32,
    // readers: u32,
    // events: u32,
    ttl: u32,
    pending_closes: u32,

    streams: HashMap<u32, UdxStream>,

    read_buf: Vec<u8>,
    // write_buf: Vec<u8>,
    timeout_interval: Interval,
}

impl UdxSocketInner {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let ttl = UDX_DEFAULT_TTL;
        let socket = UdpSocket::bind(addr).await?;
        socket.set_ttl(ttl)?;
        Ok(Self {
            socket,
            send_queue: VecDeque::with_capacity(16),
            status: 0 | UDX_SOCKET_BOUND,
            pending_closes: 0,
            ttl,
            streams: HashMap::new(),

            read_buf: vec![0u8; 2048],
            // write_buf: vec![0u8; 2048],
            timeout_interval: tokio::time::interval(Duration::from_millis(
                UDX_CLOCK_GRANULARITY_MS as u64,
            )),
        })
    }

    pub fn streams_len(&self) -> usize {
        self.streams.len()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn poll_next(&self, cx: &mut Context<'_>) {}

    fn try_poll_send_packet(&self, cx: &mut Context<'_>, packet: &Packet) -> io::Result<SendRes> {
        let buf0 = &packet.bufs[0];
        let buf1 = &packet.bufs[1];
        let len = buf0.len() + buf1.len();
        // TODO: Don't memcpy when retrying the same packet.
        // self.write_buf[0..buf0.len()].copy_from_slice(&buf0);
        // self.write_buf[buf0.len()..len].copy_from_slice(&buf1);
        let mut write_buf = [0u8; 2048];
        write_buf[0..buf0.len()].copy_from_slice(&buf0);
        write_buf[buf0.len()..len].copy_from_slice(&buf1);

        let socket_ttl = self.socket.ttl()?;
        let adjust_ttl = packet.ttl > 0 && socket_ttl != packet.ttl;

        if adjust_ttl {
            self.socket.set_ttl(packet.ttl)?;
        }
        let res = match self.socket.poll_send_to(cx, &write_buf[..len], packet.dest) {
            Poll::Pending => Ok(SendRes::NotSent),
            Poll::Ready(Ok(n)) if n < len => {
                // packet was only sent partially.
                // can this actually happen?
                // this means we should retry right away?
                let _ = self.socket.poll_send_ready(cx);
                Ok(SendRes::NotSent)
            }
            Poll::Ready(Ok(_n)) => Ok(SendRes::Sent),
            Poll::Ready(Err(err)) => Err(err),
        };
        if adjust_ttl {
            self.socket.set_ttl(socket_ttl)?;
        }
        res
    }

    // pub fn poll_check_timeouts(&mut self) -> io::Result<()> {
    //     for stream in self.streams_by_id.values() {
    //         stream.lock("stream:check_timeouts").check_timeouts()?;
    //     }
    //     Ok(())
    // }

    pub fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.status & UDX_SOCKET_CLOSING > 0 {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "Closed by user")));
        }
        // Read from the socket.
        let mut read = 0;
        loop {
            let mut buf = ReadBuf::new(&mut self.read_buf);

            let (len, _peer) = match self.socket.poll_recv_from(cx, &mut buf) {
                Poll::Pending => break,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Ready(Ok(peer)) => (buf.filled().len(), peer),
            };
            match self.process_incoming(len) {
                Ok(ProcessRes::Handled) => {}
                Ok(ProcessRes::NotHandled) => {
                    // self.on_unhandled_packet(&self.read_buf[..n])?;
                }
                Err(_err) => {}
            }
            read += 1;
        }
        if read > 0 {
            // eprintln!("[poll] read {}", read);
        }

        let interval_ticked = match self.timeout_interval.poll_tick(cx) {
            Poll::Ready(_) => true,
            Poll::Pending => false,
        };
        // register next wakeup.
        // while !matches!(self.timeout_interval.poll_tick(cx), Poll::Pending) {}
        // Write packets out.
        'send_packets: for stream_ref in self.streams.values() {
            let mut stream = stream_ref.lock("stream:poll_send_queue:check_timeouts");
            if interval_ticked {
                match stream.check_timeouts() {
                    Ok(()) => {}
                    Err(err) => return Poll::Ready(Err(err)),
                }
            } else {
                stream.flush_waiting_packets();
            }
            // TODO: This sorts packets always by streams, punishing later ids.
            while let Some(mut packet_ref) = stream.send_queue.pop_front() {
                let mut packet = match &mut packet_ref {
                    PacketRef::Packet(packet) => Some(packet),
                    PacketRef::Ref(seq) => stream.outgoing.get_mut(&seq),
                };
                let packet = packet.as_mut().unwrap();
                assert!(matches!(packet.status, PacketStatus::Sending));
                let res = self.try_poll_send_packet(cx, &packet);
                match res? {
                    SendRes::NotSent => {
                        stream.send_queue.push_front(packet_ref);
                        break 'send_packets;
                    }
                    SendRes::Sent => {
                        packet.status = PacketStatus::Inflight;
                        packet.transmits += 1;
                        packet.time_sent = get_milliseconds();
                    }
                };
            }
            drop(stream);
        }
        // eprintln!(
        //     "processed all streams. remaining: {}",
        //     self.streams_by_id
        //         .values()
        //         .map(|s| s.lock("iter"))
        //         .fold(0, |acc, stream| stream.send_queue.len() + acc)
        // );

        // always return pending - no need to call this again until either IO or the timeout is
        // reached.
        // TODO: Should be woken if new writes are queued in a stream?
        Poll::Pending
    }

    fn process_incoming(&mut self, bytes: usize) -> io::Result<ProcessRes> {
        let header = match Header::from_bytes(&self.read_buf[..bytes]) {
            Some(header) => header,
            None => return Ok(ProcessRes::NotHandled),
        };
        // eprintln!("[{}] incoming {:?}", self.local_addr()?.port(), header);
        let stream = match self.streams.get_mut(&header.stream_id) {
            Some(stream) => stream,
            None => {
                // self.invoke_preconnect();
                // and match again.
                return Ok(ProcessRes::NotHandled);
            }
        };
        let mut offset = UDX_HEADER_SIZE;
        let mut len = bytes - UDX_HEADER_SIZE;
        // Done with header processing now.
        // For future compat, make sure we are now pointing at the actual data using the data_offset
        if header.data_offset > 0 {
            if header.data_offset > len {
                return Ok(ProcessRes::NotHandled);
            }
            offset += header.data_offset;
            len -= header.data_offset;
        }
        let buf = &self.read_buf[offset..(offset + len)];
        let mut stream = stream.lock("stream:sock.process_incoming");
        stream.process_incoming(&header, &buf)
    }

    fn on_unhandled_packet(&self, _buf: &[u8]) -> io::Result<()> {
        // TODO: What to do with non-stream packets?
        Ok(())
    }

    // pub fn send(&self, _req: SendRequest) {}
    // pub fn send_ttl(&mut self, mut req: SendRequest, buf: UdxBuf, dest: SocketAddr, ttl: u32) {
    //     {
    //         let pkt = &mut req.pkt;
    //         pkt.status = PacketStatus::Sending;
    //         pkt.r#type = UDX_PACKET_SEND;
    //         pkt.ttl = ttl;
    //         pkt.dest = dest;
    //         pkt.transmits = 0;
    //         pkt.bufs_len = 1;
    //         pkt.bufs[0] = buf;
    //     }
    //     self.send_queue.push_back(req.pkt);
    // }
}

pub struct SendRequest {
    pkt: Packet,
    data: u32, // request id
}

pub fn seq_diff(a: u32, b: u32) -> i32 {
    (a as i32) - (b as i32)
}

pub fn seq_compare(a: u32, b: u32) -> i8 {
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
