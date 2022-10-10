use bytes::BytesMut;
use futures::Future;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Debug;
use std::io;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Waker;
use std::task::{Context, Poll};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tokio::time::Sleep;
use tracing::{debug, trace};

use crate::constants::UDX_HEADER_SIZE;
use crate::constants::UDX_MTU;
use crate::mutex::Mutex;
use crate::packet::PacketSet;
use crate::packet::{Header, IncomingPacket};
use crate::stream::UdxStream;
use crate::udp::{RecvMeta, Transmit, UdpSocket, UdpState, BATCH_SIZE};

const MAX_LOOP: usize = 60;

#[derive(Debug)]
pub(crate) enum EventIncoming {
    Packet(IncomingPacket),
}

#[derive(Debug)]
pub(crate) enum EventOutgoing {
    Transmit(PacketSet),
    // TransmitOne(PacketRef),
    StreamDropped(u32),
}

#[derive(Debug)]
struct StreamHandle {
    recv_tx: Sender<EventIncoming>,
}

#[derive(Clone, Debug)]
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
        &self,
        dest: SocketAddr,
        local_id: u32,
        remote_id: u32,
    ) -> io::Result<UdxStream> {
        self.0
            .lock("UdxSocket::connect")
            .connect(dest, local_id, remote_id)
    }

    pub fn stats(&self) -> SocketStats {
        self.0.lock("UdxSocket::stats").stats.clone()
    }
}

impl Drop for UdxSocket {
    fn drop(&mut self) {
        // Only the driver is left, shutdown.
        if Arc::strong_count(&self.0) == 2 {
            let mut socket = self.0.lock("UdxSocket::drop");
            socket.has_refs = false;
            if let Some(waker) = socket.drive_waker.take() {
                waker.wake();
            }
        }
    }
}

pub struct SocketDriver(UdxSocket);

impl Future for SocketDriver {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut socket = self.0.lock("UdxSocket::poll_drive");
        let mut should_continue = false;
        should_continue |= socket.poll_recv(cx)?;
        if let Some(send_overflow_timer) = socket.send_overflow_timer.as_mut() {
            match send_overflow_timer.as_mut().poll(cx) {
                Poll::Pending => {}
                Poll::Ready(()) => {
                    log::warn!("send overflow timer clear!");
                    socket.send_overflow_timer = None;
                    should_continue = true;
                }
            }
        } else {
            should_continue |= socket.poll_transmit(cx)?;
        }

        if !should_continue && !socket.has_refs && socket.streams.is_empty() {
            return Poll::Ready(Ok(()));
        }
        if should_continue {
            drop(socket);
            cx.waker().wake_by_ref();
        } else {
            socket.drive_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

pub struct UdxSocketInner {
    socket: UdpSocket,
    send_rx: Receiver<EventOutgoing>,
    send_tx: Sender<EventOutgoing>,
    streams: HashMap<u32, StreamHandle>,
    pending_transmits: VecDeque<Transmit>,
    pending_transmits_packets: VecDeque<PacketSet>,
    recv_buf: Box<[u8]>,
    udp_state: Arc<UdpState>,
    stats: SocketStats,
    has_refs: bool,
    drive_waker: Option<Waker>,

    send_overflow_timer: Option<Pin<Box<Sleep>>>,
}

impl fmt::Debug for UdxSocketInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdxSocketInner")
            .field("socket", &self.socket)
            .field("streams", &self.streams)
            .field("pending_transmits", &self.pending_transmits.len())
            .field("udp_state", &self.udp_state)
            .field("stats", &self.stats)
            .field("has_refs", &self.has_refs)
            .field("drive_waker", &self.drive_waker)
            .finish()
    }
}

#[derive(Default, Clone, Debug)]
pub struct SocketStats {
    tx_transmits: usize,
    tx_dgrams: usize,
    tx_bytes: usize,
    rx_bytes: usize,
    rx_dgrams: usize,
    tx_window_start: Option<Instant>,
    tx_window_bytes: usize,
    tx_window_dgrams: usize,
}

impl SocketStats {
    fn track_tx(&mut self, transmit: &Transmit) {
        self.tx_bytes += transmit.contents.len();
        self.tx_transmits += 1;
        self.tx_dgrams += transmit.num_segments();
        self.tx_window_bytes += transmit.contents.len();
        self.tx_window_dgrams += transmit.num_segments();
        if self.tx_window_start.is_none() {
            self.tx_window_start = Some(Instant::now());
        }
        if self.tx_window_start.as_ref().unwrap().elapsed() > Duration::from_millis(1000) {
            let elapsed = self
                .tx_window_start
                .as_ref()
                .unwrap()
                .elapsed()
                .as_secs_f32();
            trace!(
                "{} MB/s {} pps",
                self.tx_window_bytes as f32 / (1024. * 1024.) / elapsed,
                self.tx_window_dgrams as f32 / elapsed
            );
            self.tx_window_bytes = 0;
            self.tx_window_dgrams = 0;
            self.tx_window_start = Some(Instant::now());
        }
    }
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
            pending_transmits: VecDeque::with_capacity(BATCH_SIZE),
            pending_transmits_packets: VecDeque::with_capacity(BATCH_SIZE),
            stats: SocketStats::default(),
            has_refs: true,
            drive_waker: None,
            send_overflow_timer: None,
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
            local_id,
        );
        let handle = StreamHandle { recv_tx };
        self.streams.insert(local_id, handle);
        Ok(stream)
    }

    fn poll_transmit(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        // let local_addr = self.local_addr().unwrap();
        let mut iters = 0;
        loop {
            iters += 1;
            let mut send_rx_pending = false;
            while self.pending_transmits.len() < BATCH_SIZE {
                match Pin::new(&mut self.send_rx).poll_recv(cx) {
                    Poll::Pending => {
                        send_rx_pending = true;
                        break;
                    }
                    Poll::Ready(None) => unreachable!(),
                    Poll::Ready(Some(event)) => match event {
                        EventOutgoing::StreamDropped(local_id) => {
                            let _ = self.streams.remove(&local_id);
                        }
                        EventOutgoing::Transmit(packet_set) => {
                            let transmit = packet_set.to_transmit();
                            self.stats.track_tx(&transmit);
                            trace!("send {:?}", packet_set);
                            // trace!("send {:?}", transmit);
                            self.pending_transmits.push_back(transmit);
                            self.pending_transmits_packets.push_back(packet_set);
                        }
                    },
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
                Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::Interrupted => {
                    // Send overflow! Scale back write rate.
                    self.send_overflow_timer =
                        Some(Box::pin(tokio::time::sleep(Duration::from_millis(20))));
                    log::warn!("send overflow timer set!");
                    break Ok(false);
                }
                Poll::Ready(Err(err)) => break Err(err),
                Poll::Ready(Ok(n)) => {
                    self.pending_transmits.drain(..n);
                    for packet_set in self.pending_transmits_packets.drain(..n) {
                        for packet in packet_set.packets {
                            packet.time_sent.set_now();
                        }
                    }
                }
            }
            if send_rx_pending {
                break Ok(false);
            }
            if iters > 0 {
                break Ok(true);
            }
        }
    }

    fn poll_recv<'a>(&'a mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        let local_addr = self.local_addr().unwrap();
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
                Poll::Pending => return Ok(false),
                Poll::Ready(Err(e)) => return Err(e),
                Poll::Ready(Ok(msgs)) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(msgs) {
                        let mut data: BytesMut = buf[0..meta.len].into();
                        let len = data.len();
                        self.stats.rx_bytes += len;
                        self.stats.rx_dgrams += 1;
                        match Header::from_bytes(&data) {
                            Err(_err) => {
                                // received invalid header.
                                // treat as received message.
                                // push to recv_message queue.
                            }
                            Ok(header) => {
                                trace!(
                                    to = header.stream_id,
                                    "[{}] recv from :{} typ {} seq {} ack {} len {}",
                                    local_addr.port(),
                                    meta.addr.port(),
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
}

pub enum SocketEvent {
    UnknownStream,
}
