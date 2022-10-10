use bytes::BytesMut;
use futures::Future;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tracing::{debug, trace};

use crate::constants::UDX_HEADER_SIZE;
use crate::constants::UDX_MTU;
use crate::mutex::Mutex;
use crate::packet::{Header, IncomingPacket};
use crate::stream::UdxStream;
use crate::udp::{RecvMeta, Transmit, UdpSocket, UdpState, BATCH_SIZE};

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
    send_rx: Receiver<Transmit>,
    send_tx: Sender<Transmit>,
    streams: HashMap<u32, StreamHandle>,
    pending_transmits: VecDeque<Transmit>,
    recv_buf: Box<[u8]>,
    udp_state: Arc<UdpState>,
}

impl UdxSocketInner {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        let socket = UdpSocket::from_std(socket)?;
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let recv_buf = vec![0; UDX_MTU * BATCH_SIZE];
        Ok(Self {
            socket,
            send_rx,
            send_tx,
            streams: HashMap::new(),
            recv_buf: recv_buf.into(),
            udp_state: Arc::new(UdpState::new()),
            pending_transmits: VecDeque::new(),
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
        let stream = UdxStream::connect(
            recv_rx,
            self.send_tx.clone(),
            self.udp_state.clone(),
            dest,
            remote_id,
        );
        let handle = StreamHandle { recv_tx };
        self.streams.insert(local_id, handle);
        Ok(stream)
    }

    fn poll_transmit(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        let mut iters = 0;
        loop {
            iters += 1;
            while self.pending_transmits.len() < BATCH_SIZE {
                match Pin::new(&mut self.send_rx).poll_recv(cx) {
                    Poll::Pending => break,
                    Poll::Ready(None) => unreachable!(),
                    Poll::Ready(Some(transmit)) => {
                        self.pending_transmits.push_back(transmit);
                    }
                }
            }
            if self.pending_transmits.is_empty() {
                break Ok(false);
            }

            match self
                .socket
                .poll_send(&self.udp_state, cx, self.pending_transmits.as_slices().0)
            {
                Poll::Pending => break Ok(false),
                Poll::Ready(Err(err)) => break Err(err),
                Poll::Ready(Ok(n)) => {
                    self.pending_transmits.drain(..n);
                }
            }
            if iters > 10 {
                break Ok(true);
            }
        }
    }

    fn poll_recv<'a>(&'a mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        // Taken from: quinn/src/endpoint.rs
        let mut metas = [RecvMeta::default(); BATCH_SIZE];
        let mut iovs = MaybeUninit::<[IoSliceMut<'a>; BATCH_SIZE]>::uninit();
        self.recv_buf
            .chunks_mut(self.recv_buf.len() / BATCH_SIZE)
            .enumerate()
            .for_each(|(i, buf)| unsafe {
                iovs.as_mut_ptr()
                    .cast::<IoSliceMut>()
                    .add(i)
                    .write(IoSliceMut::<'a>::new(buf));
            });
        let mut iovs = unsafe { iovs.assume_init() };

        // process recv
        let mut iters = 0;
        while iters < MAX_LOOP {
            iters += 1;
            match self.socket.poll_recv(cx, &mut iovs, &mut metas) {
                Poll::Pending => break,
                Poll::Ready(Err(e)) => return Err(e),
                Poll::Ready(Ok(msgs)) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(msgs) {
                        let mut data: BytesMut = buf[0..meta.len].into();
                        let len = data.len();
                        match Header::from_bytes(&data) {
                            Err(_err) => {
                                // received invalid header.
                                // treat as received message.
                                // push to recv_message queue.
                            }
                            Ok(header) => {
                                trace!(
                                    to = header.stream_id,
                                    "recv typ {} seq {} ack {} len {}",
                                    header.typ,
                                    header.seq,
                                    header.ack,
                                    len
                                );
                                let stream_id = header.stream_id;
                                match self.streams.get(&stream_id) {
                                    None => {
                                        // received packet for nonexisting stream.
                                        // emit event to allow to open channel
                                    }
                                    Some(handle) => {
                                        let _ = data.split_to(UDX_HEADER_SIZE);
                                        let incoming = IncomingPacket {
                                            header,
                                            buf: data.into(),
                                            read_offset: 0,
                                        };
                                        let event = EventIncoming::Packet(incoming);
                                        match handle.recv_tx.send(event) {
                                            Ok(()) => {}
                                            Err(_packet) => {
                                                // stream was dropped.
                                                // remove stream?
                                                self.streams.remove(&stream_id);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
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
