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
use crate::packet::{Dgram, Header, IncomingPacket, PacketSet};
use crate::stream::UdxStream;
use crate::udp::{RecvMeta, Transmit, UdpSocket, UdpState, BATCH_SIZE};

const MAX_LOOP: usize = 60;

const RECV_QUEUE_MAX_LEN: usize = 1024;

#[derive(Debug)]
pub(crate) enum EventIncoming {
    Packet(IncomingPacket),
}

#[derive(Debug)]
pub(crate) enum EventOutgoing {
    Transmit(PacketSet),
    TransmitDgram(Dgram),
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

    pub fn send(&self, dest: SocketAddr, buf: &[u8]) {
        let dgram = Dgram::new(dest, buf.to_vec());
        let ev = EventOutgoing::TransmitDgram(dgram);
        self.0.lock("UdxSocket::send").send_tx.send(ev).unwrap();
    }

    pub fn recv(&self) -> RecvFuture {
        RecvFuture(self.clone())
    }
}

pub struct RecvFuture(UdxSocket);
impl Future for RecvFuture {
    type Output = io::Result<(SocketAddr, Vec<u8>)>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut socket = self.0.lock("UdxSocket::recv");
        if let Some(dgram) = socket.recv_dgrams.pop_front() {
            Poll::Ready(Ok((dgram.dest, dgram.buf)))
        } else {
            socket.recv_waker = Some(cx.waker().clone());
            Poll::Pending
        }
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
    outgoing_transmits: VecDeque<Transmit>,
    outgoing_packet_sets: VecDeque<PacketSet>,
    recv_buf: Option<Box<[u8]>>,
    udp_state: Arc<UdpState>,
    stats: SocketStats,
    has_refs: bool,
    drive_waker: Option<Waker>,

    send_overflow_timer: Option<Pin<Box<Sleep>>>,
    recv_dgrams: VecDeque<Dgram>,
    recv_waker: Option<Waker>,
}

impl fmt::Debug for UdxSocketInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdxSocketInner")
            .field("socket", &self.socket)
            .field("streams", &self.streams)
            .field("pending_transmits", &self.outgoing_transmits.len())
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
            recv_buf: Some(recv_buf.into()),
            udp_state: Arc::new(UdpState::new()),
            outgoing_transmits: VecDeque::with_capacity(BATCH_SIZE),
            outgoing_packet_sets: VecDeque::with_capacity(BATCH_SIZE),
            stats: SocketStats::default(),
            has_refs: true,
            drive_waker: None,
            send_overflow_timer: None,
            recv_waker: None,
            recv_dgrams: VecDeque::new(),
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
        let mut iters = 0;
        loop {
            iters += 1;
            let mut send_rx_pending = false;
            while self.outgoing_transmits.len() < BATCH_SIZE {
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
                        EventOutgoing::TransmitDgram(dgram) => {
                            self.outgoing_transmits.push_back(dgram.into_transmit());
                        }
                        EventOutgoing::Transmit(packet_set) => {
                            let transmit = packet_set.to_transmit();
                            trace!("send {:?}", packet_set);
                            self.outgoing_transmits.push_back(transmit);
                            self.outgoing_packet_sets.push_back(packet_set);
                        }
                    },
                }
            }
            if self.outgoing_transmits.is_empty() {
                break Ok(false);
            }

            match self
                .socket
                .poll_send(&self.udp_state, cx, self.outgoing_transmits.as_slices().0)
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
                    for transmit in self.outgoing_transmits.drain(..n) {
                        self.stats.track_tx(&transmit);
                    }
                    // update packet sent time for data packets.
                    let n = n.min(self.outgoing_packet_sets.len());
                    for packet_set in self.outgoing_packet_sets.drain(..n) {
                        for packet in packet_set.iter_shared() {
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
        let mut metas = [RecvMeta::default(); BATCH_SIZE];
        let mut recv_buf = self.recv_buf.take().unwrap();
        let mut iovs = unsafe { iovectors_from_buf::<BATCH_SIZE>(&mut recv_buf) };

        // process recv
        let mut iters = 0;
        let res = loop {
            iters += 1;
            if iters == MAX_LOOP {
                break Ok(true);
            }
            match self.socket.poll_recv(cx, &mut iovs, &mut metas) {
                Poll::Ready(Ok(msgs)) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(msgs) {
                        let data: BytesMut = buf[0..meta.len].into();
                        if let Err(data) = self.process_packet(data, meta) {
                            // received invalid header. emit as message on socket.
                            // TODO: Remove the queue and invoke a poll handler directly?
                            self.on_recv_dgram(data, meta);
                        }
                    }
                }
                Poll::Pending => break Ok(false),
                Poll::Ready(Err(e)) => break Err(e),
            }
        };
        self.recv_buf = Some(recv_buf);
        res
    }

    fn on_recv_dgram(&mut self, data: BytesMut, meta: &RecvMeta) {
        if self.recv_dgrams.len() < RECV_QUEUE_MAX_LEN {
            self.recv_dgrams
                .push_back(Dgram::new(meta.addr, data.to_vec()));
            if let Some(waker) = self.recv_waker.take() {
                waker.wake()
            }
        } else {
            drop(data)
        }
    }

    fn process_packet(&mut self, mut data: BytesMut, meta: &RecvMeta) -> Result<(), BytesMut> {
        let local_addr = self.local_addr().unwrap();
        let len = data.len();
        self.stats.rx_bytes += len;
        self.stats.rx_dgrams += 1;

        // try to decode the udx header
        let header = match Header::from_bytes(&data) {
            Ok(header) => header,
            Err(_) => return Err(data),
        };
        let stream_id = header.stream_id;
        trace!(
            to = stream_id,
            "[{}] recv from :{} typ {} seq {} ack {} len {}",
            local_addr.port(),
            meta.addr.port(),
            header.typ,
            header.seq,
            header.ack,
            len
        );
        match self.streams.get(&stream_id) {
            Some(handle) => {
                let _ = data.split_to(UDX_HEADER_SIZE);
                let incoming = IncomingPacket {
                    header,
                    buf: data.into(),
                    read_offset: 0,
                    from: meta.addr,
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
            None => {
                // received packet for nonexisting stream.
                return Err(data);
            }
        }
        Ok(())
    }
}

pub enum SocketEvent {
    UnknownStream,
}

// Create an array of IO vectors from a buffer.
// Safety: buf has to be longer than N. You may only read from slices that have been written to.
// Taken from: quinn/src/endpoint.rs
unsafe fn iovectors_from_buf<'a, const N: usize>(buf: &'a mut [u8]) -> [IoSliceMut; N] {
    let mut iovs = MaybeUninit::<[IoSliceMut<'a>; N]>::uninit();
    buf.chunks_mut(buf.len() / N)
        .enumerate()
        .for_each(|(i, buf)| {
            iovs.as_mut_ptr()
                .cast::<IoSliceMut>()
                .add(i)
                .write(IoSliceMut::<'a>::new(buf));
        });
    iovs.assume_init()
}
