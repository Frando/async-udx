use futures::Future;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;
use tokio::net::ToSocketAddrs;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tracing::{debug, trace};

use crate::constants::UDX_HEADER_SIZE;
use crate::mutex::Mutex;
use crate::packet::{Header, IncomingPacket, Packet, PacketRef};
use crate::stream::UdxStream;

const MAX_LOOP: usize = 60;

#[derive(Debug)]
pub(crate) enum EventIncoming {
    Packet(IncomingPacket),
}

#[derive(Debug)]
struct StreamHandle {
    recv_tx: Sender<EventIncoming>,
}

#[derive(Clone)]
pub struct UdxSocket(Arc<Mutex<UdxSocketInner>>);

impl std::ops::Deref for UdxSocket {
    type Target = Mutex<UdxSocketInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl UdxSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let inner = UdxSocketInner::bind(addr).await?;
        let socket = Self(Arc::new(Mutex::new(inner)));
        let driver = SocketDriver(socket.clone());
        tokio::spawn(async {
            if let Err(e) = driver.await {
                tracing::error!("Socket I/O error: {}", e);
            }
        });
        Ok(socket)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.lock("UdxSocket::local_addr").socket.local_addr()
    }

    pub fn connect(
        &mut self,
        dest: SocketAddr,
        local_id: u32,
        remote_id: u32,
    ) -> io::Result<UdxStream> {
        self.0
            .lock("UdxSocket::connect")
            .connect(dest, local_id, remote_id)
    }
}

pub struct SocketDriver(UdxSocket);

impl Future for SocketDriver {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut socket = self.0.lock("UdxSocket::poll_drive");
        let should_continue = socket.poll_drive(cx)?;
        drop(socket);
        if should_continue {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

pub struct UdxSocketInner {
    socket: UdpSocket,
    send_rx: Receiver<PacketRef>,
    send_tx: Sender<PacketRef>,
    streams: HashMap<u32, StreamHandle>,
    pending_send: Option<PacketRef>,
    read_buf: Vec<u8>,
}

impl UdxSocketInner {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        Ok(Self {
            socket,
            send_rx,
            send_tx,
            streams: HashMap::new(),
            read_buf: vec![0u8; 2048],
            pending_send: None,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn connect(
        &mut self,
        dest: SocketAddr,
        local_id: u32,
        remote_id: u32,
    ) -> io::Result<UdxStream> {
        debug!(
            "connect {} [{}] -> {} [{}])",
            self.local_addr().unwrap(),
            local_id,
            dest,
            remote_id
        );
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        let stream = UdxStream::connect(recv_rx, self.send_tx.clone(), dest, remote_id);
        let handle = StreamHandle { recv_tx };
        self.streams.insert(local_id, handle);
        Ok(stream)
    }

    fn poll_send_packet(&mut self, cx: &mut Context<'_>, packet: &Packet) -> io::Result<bool> {
        trace!(
            to = packet.header.stream_id,
            "send typ {} seq {} ack {} tx {}",
            packet.header.typ,
            packet.header.seq,
            packet.header.ack,
            packet.transmits.load(Ordering::SeqCst)
        );
        match self
            .socket
            .poll_send_to(cx, &packet.buf.as_slice(), packet.dest)
        {
            Poll::Pending => Ok(false),
            Poll::Ready(Err(err)) => Err(err),
            Poll::Ready(Ok(n)) => {
                if n == packet.buf.len() {
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    fn poll_transmit(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        // process transmits
        let mut iters = 0;
        while iters < MAX_LOOP {
            iters += 1;
            if let Some(packet) = self.pending_send.take() {
                if self.poll_send_packet(cx, &packet)? {
                    // packet.transmits.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.pending_send = Some(packet);
                    break;
                }
            } else {
                match Pin::new(&mut self.send_rx).poll_recv(cx) {
                    Poll::Pending => break,
                    Poll::Ready(None) => unreachable!(),
                    Poll::Ready(Some(packet)) => {
                        self.pending_send = Some(packet);
                    }
                }
            }
        }
        Ok(iters == MAX_LOOP)
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        // process recv
        let mut iters = 0;
        while iters < MAX_LOOP {
            iters += 1;
            let (len, header, _peer) = {
                // todo: vectorize
                let mut buf = ReadBuf::new(&mut self.read_buf);
                let peer = match self.socket.poll_recv_from(cx, &mut buf)? {
                    Poll::Pending => break,
                    Poll::Ready(peer) => peer,
                };
                let header = Header::from_bytes(buf.filled())?;
                (buf.filled().len(), header, peer)
            };
            trace!(
                to = header.stream_id,
                "recv typ {} seq {} ack {} len {}",
                header.typ,
                header.seq,
                header.ack,
                len
            );
            match self.streams.get_mut(&header.stream_id) {
                Some(handle) => {
                    let incoming = IncomingPacket {
                        header,
                        buf: self.read_buf[UDX_HEADER_SIZE..len].to_vec().into(),
                        read_offset: 0,
                    };
                    let event = EventIncoming::Packet(incoming);
                    match handle.recv_tx.send(event) {
                        Ok(()) => {}
                        Err(_packet) => unimplemented!(),
                    }
                }
                None => {
                    // received packet for nonexisting stream.
                    // emit event to allow to open channel
                }
            }
        }
        Ok(iters == MAX_LOOP)
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        let mut should_continue = false;
        should_continue |= self.poll_recv(cx)?;
        should_continue |= self.poll_transmit(cx)?;
        Ok(should_continue)
    }
}

pub enum SocketEvent {
    UnknownStream,
}
