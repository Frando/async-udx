use bytes::{Bytes, BytesMut};
use futures::{Future, FutureExt};
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};
// use postage::mpsc;
// use postage::sink::{PollSend, Sink, TrySendError};
// use postage::stream::{PollRecv, Stream};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Debug};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc},
    task::{Context, Poll, Waker},
};
use tokio::io::ReadBuf;
use tokio::net::ToSocketAddrs;
use tracing::{debug, trace};
// use tokio::sync::mpsc;
use tokio::time::{Interval, Sleep, Timeout};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
};

use crate::constants::{UDX_HEADER_DATA, UDX_MAX_DATA_SIZE, UDX_MAX_TRANSMITS};
use crate::constants::{UDX_HEADER_SIZE, UDX_MAGIC_BYTE, UDX_VERSION};
use crate::mutex::Mutex;

// const MAX_WAITING: usize = 2048;
const MAX_PENDING: u32 = 64;
const UDX_MTU: usize = 1400;
const UDX_CLOCK_GRANULARITY_MS: Duration = Duration::from_millis(20);

const MAX_TRANSMITS: u8 = UDX_MAX_TRANSMITS;

const STATUS_WAITING: usize = 0;
const STATUS_INFLIGHT: usize = 2;
const STATUS_SENDING: usize = 1;

const SSTHRESH: usize = 0xffff;

const MAX_LOOP: usize = 50;

pub(crate) struct Packet {
    // seq: u32,
    pub time_sent: Instant,
    pub transmits: AtomicUsize,
    pub dest: SocketAddr,
    pub header: Header,
    pub buf: Vec<u8>,
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
    pub fn has_type(&self, typ: u32) -> bool {
        self.header.typ & typ != 0
    }

    pub fn new(dest: SocketAddr, header: Header, body: &[u8]) -> Self {
        let len = UDX_HEADER_SIZE + body.len();
        let mut buf = vec![0u8; len];
        header.encode(&mut buf[..UDX_HEADER_SIZE]);
        buf[UDX_HEADER_SIZE..].copy_from_slice(body);
        Self {
            time_sent: Instant::now(),
            transmits: AtomicUsize::new(0),
            dest,
            header,
            buf,
        }
    }

    pub fn from_stream(stream: &UdxStreamInner, typ: u32, body: &[u8]) -> Arc<Self> {
        let header = Header::from_stream(stream, typ);
        let dest = stream.remote_addr;
        Arc::new(Self::new(dest, header, body))
    }

    fn seq(&self) -> u32 {
        self.header.seq
    }
}

pub(crate) struct IncomingPacket {
    pub(crate) header: Header,
    pub(crate) buf: Bytes,
    pub(crate) read_offset: usize,
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

// struct PendingRead {
//     seq: u32,
//     buf: Bytes,
// }

#[derive(Clone)]
pub struct UdxStream(Arc<Mutex<UdxStreamInner>>);

// impl UdxStream {
//     fn drive(&self) -> StreamNext {
//         StreamNext { stream: self.clone() }
//     }
// }
pub struct StreamDriver {
    stream: UdxStream,
}

impl Future for StreamDriver {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        {
            let mut stream = self.stream.0.lock("UdxStream::poll_drive");
            let should_continue = stream.poll_drive(cx);
            if should_continue {
                drop(stream);
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                stream.drive_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Debug for UdxStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.0.lock("UdxStream::debug");
        write!(f, "{:#?}", &*inner)
    }
}

impl std::ops::Deref for UdxSocket {
    type Target = Mutex<UdxSocketInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::Deref for UdxStream {
    type Target = Mutex<UdxStreamInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsyncRead for UdxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = self.0.lock("UdxStream::poll_read");
        Pin::new(&mut *inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for UdxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.0.lock("UdxStream::poll_write");
        Pin::new(&mut *inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub struct UdxStreamInner {
    incoming: BTreeMap<u32, IncomingPacket>,
    outgoing: HashMap<u32, Arc<Packet>>,
    send_tx: Sender<Arc<Packet>>,
    recv_rx: Receiver<EventIncoming>,

    seq: u32,
    remote_acked: u32,

    ack: u32,

    pub srtt: Duration,
    pub rttvar: Duration,

    inflight: usize,
    cwnd: usize,

    read_cursor: u32,

    // pkts_waiting: usize,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    pub(crate) drive_waker: Option<Waker>,

    remote_id: u32,
    local_id: u32,

    remote_addr: SocketAddr,

    send_queue: VecDeque<Arc<Packet>>,

    rto: Duration,
    rto_timeout: Pin<Box<Sleep>>,

    error: Option<UdxError>,
}

#[derive(Debug, Clone)]
struct UdxError {
    kind: io::ErrorKind,
    reason: String,
}

impl fmt::Display for UdxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind_str = io::Error::from(self.kind);
        write!(f, "{} {}", kind_str, self.reason)
    }
}

impl std::error::Error for UdxError {}

impl UdxError {
    pub fn new(kind: io::ErrorKind, reason: impl ToString) -> Self {
        Self {
            kind,
            reason: reason.to_string(),
        }
    }
}

impl From<io::Error> for UdxError {
    fn from(err: io::Error) -> Self {
        Self::new(err.kind(), format!("{}", err))
    }
}

impl From<UdxError> for io::Error {
    fn from(err: UdxError) -> Self {
        io::Error::new(err.kind, err.reason)
    }
}

#[derive(Debug)]
enum EventIncoming {
    Packet(IncomingPacket),
}

impl UdxStreamInner {
    fn poll_drive(&mut self, cx: &mut Context<'_>) -> bool {
        match self.poll_drive_inner(cx) {
            Ok(should_continue) => should_continue,
            Err(error) => {
                self.error = Some(error.into());
                false
            }
        }
    }
    fn poll_drive_inner(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        self.poll_incoming(cx);
        self.poll_check_timeouts(cx)?;
        self.poll_transmit(cx);
        Ok(false)
    }

    fn wake_driver(&mut self) {
        if let Some(waker) = self.drive_waker.take() {
            waker.wake();
        }
    }

    fn poll_check_timeouts(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        match self.rto_timeout.as_mut().poll(cx) {
            Poll::Pending => return Ok(()),
            Poll::Ready(_) => {}
        }
        if self.remote_acked == self.seq {
            return Ok(());
        }
        // schedule next timeout.
        let next = tokio::time::Instant::now()
            .checked_add(2 * self.rto)
            .unwrap();
        self.rto_timeout.as_mut().reset(next);
        // shrink the window
        self.cwnd = UDX_MTU.max(self.cwnd / 2);
        // queue retransmits.
        for i in self.remote_acked..self.seq {
            if let Some(packet) = self.outgoing.get(&i) {
                let may_retransmit = packet.transmits.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |transmits| {
                        if transmits >= MAX_TRANSMITS as usize {
                            None
                        } else {
                            Some(transmits + 1)
                        }
                    },
                );
                match may_retransmit {
                    Ok(_) => {
                        trace!("queue retransmit {}", packet.header.seq);
                        self.send_queue.push_back(packet.clone());
                    }
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Max retransmits reached without ack",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn poll_transmit(&mut self, _cx: &mut Context<'_>) {
        while let Some(packet) = self.send_queue.pop_front() {
            match self.send_tx.send(packet) {
                Ok(()) => {}
                Err(_packet) => unimplemented!(),
            }
        }
    }

    fn poll_incoming(&mut self, cx: &mut Context<'_>) {
        let mut remote_ack = 0;
        loop {
            let next_event = Pin::new(&mut self.recv_rx).poll_recv(cx);
            match next_event {
                Poll::Pending => break,
                Poll::Ready(None) => unimplemented!(),
                Poll::Ready(Some(event)) => match event {
                    EventIncoming::Packet(packet) => {
                        remote_ack = remote_ack.max(packet.ack());
                        self.handle_incoming(packet);
                    }
                },
            }
        }
        self.send_acks();
        self.handle_remote_ack(remote_ack);
    }

    fn send_acks(&mut self) {
        while self.incoming.contains_key(&self.ack) == true {
            self.ack += 1;
            self.send_state_packet();
        }
        // let mut incoming = self.incoming.keys().collect::<Vec<_>>();
        // incoming.sort();
        // eprintln!("send acks {}", self.ack);
        // eprintln!("INC {:?}", incoming);
    }

    fn handle_remote_ack(&mut self, ack: u32) {
        if ack <= self.remote_acked {
            return;
        }

        // eprintln!("ACK {}", seq);
        // let mut outgoing = self.outgoing.keys().collect::<Vec<_>>();
        // outgoing.sort();
        // eprintln!("out {:?}", outgoing);
        while self.remote_acked < ack {
            // TODO: Clear from send queue.
            self.remote_acked += 1;
            let packet = self.outgoing.remove(&(self.remote_acked - 1));
            if let Some(packet) = packet {
                self.handle_ack(packet);
            } else {
                // Received invalid ack (too high)
                tracing::error!("received invalid ack (too high)");
            }
        }
    }

    fn handle_ack(&mut self, packet: Arc<Packet>) {
        self.inflight -= packet.buf.len();

        // recalculate timings
        let rtt = Instant::now() - packet.time_sent;
        if packet.transmits.load(Ordering::SeqCst) == 1 {
            if self.srtt.is_zero() {
                self.srtt = rtt;
                self.rttvar = rtt / 2;
                self.rto = self.srtt + UDX_CLOCK_GRANULARITY_MS.max(4 * self.rttvar);
            } else {
                let delta = if rtt < self.srtt {
                    self.srtt - rtt
                } else {
                    rtt - self.srtt
                };
                // RTTVAR <- (1 - beta) * RTTVAR + beta * |SRTT - R'| where beta is 1/4
                self.rttvar = (3 * self.rttvar + delta) / 4;
                // SRTT <- (1 - alpha) * SRTT + alpha * R' where alpha is 1/8
                self.srtt = (7 * self.srtt + rtt) / 8;
            }
            // RTO <- SRTT + max (G, K*RTTVAR) where K is 4 maxed with 1s
            self.rto = Duration::from_millis(1000)
                .max(self.srtt + UDX_CLOCK_GRANULARITY_MS.max(4 * self.rttvar));
        }

        // reset rto
        // self.rto = Duration::from_millis(100);
        self.rto_timeout
            .as_mut()
            .reset(tokio::time::Instant::now().checked_add(self.rto).unwrap());

        if (self.inflight + UDX_MTU) <= self.cwnd {
            if let Some(waker) = self.write_waker.take() {
                waker.wake();
            }
        }
        // self.remote_acked = seq;
        // if !self.pending_sends_reached() {
        //     if let Some(waker) = self.write_waker.take() {
        //         waker.wake();
        //     }
        // }
    }

    fn handle_incoming(&mut self, packet: IncomingPacket) {
        // congestion control..
        if packet.ack() != self.remote_acked {
            if self.cwnd < SSTHRESH {
                self.cwnd += UDX_MTU;
            } else {
                self.cwnd += ((UDX_MTU * UDX_MTU) / self.cwnd).max(1);
            }
        }

        // process incoming data
        if packet.has_type(UDX_HEADER_DATA) {
            let seq = packet.seq();
            debug!("INCOMING {} (ack {})", seq, self.ack);
            if seq >= self.ack {
                self.incoming.insert(seq, packet);
            }
            if seq == self.ack {
                if let Some(waker) = self.read_waker.take() {
                    waker.wake();
                }
            }
        }
    }

    fn read_next(&mut self, buf: &mut tokio::io::ReadBuf<'_>) -> io::Result<bool> {
        let mut did_read = false;
        while let Some(mut packet) = self.incoming.remove(&self.read_cursor) {
            let start = packet.read_offset;
            let len = packet.buf.len() - start;
            if len > buf.remaining() {
                let end = start + buf.remaining();
                buf.put_slice(&packet.buf[start..end]);
                packet.read_offset = end;
                self.incoming.insert(packet.header.seq, packet);
                break;
            }

            buf.put_slice(&packet.buf[start..]);
            did_read = true;
            self.read_cursor += 1;
        }
        Ok(did_read)
    }

    fn send_state_packet(&mut self) {
        let packet = Packet::from_stream(&self, 0, &[]);
        self.send_queue.push_back(packet);
        self.wake_driver();
    }

    fn send_data_packet(&mut self, buf: &[u8]) -> usize {
        let len = buf.len().min(UDX_MAX_DATA_SIZE);
        if self.inflight + len > self.cwnd {
            return 0;
        }
        let packet = Packet::from_stream(&self, UDX_HEADER_DATA, &buf[..len]);
        self.inflight += packet.buf.len();
        self.outgoing.insert(packet.seq(), Arc::clone(&packet));
        self.send_queue.push_back(packet);
        self.seq += 1;
        self.wake_driver();
        len
    }
}

impl AsyncWrite for UdxStreamInner {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(error) = &self.error {
            return Poll::Ready(Err(error.clone().into()));
        }
        if buf.len() == 0 {
            return Poll::Ready(Ok(0));
        }
        let mut written = 0;
        loop {
            if written >= buf.len() {
                break Poll::Ready(Ok(written));
            }
            let n = self.send_data_packet(&buf[written..]);
            if n == 0 {
                break if written == 0 {
                    self.write_waker = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(written))
                };
            }
            written += n;
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for UdxStreamInner {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        trace!(
            "poll read @ {} incoming len {}",
            self.ack,
            self.incoming.len()
        );
        if let Some(error) = &self.error {
            return Poll::Ready(Err(error.clone().into()));
        }
        let did_read = self.read_next(buf)?;
        let res = if !did_read {
            self.read_waker = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        };
        debug!(
            "poll read res {:?} did_read {} filled {} remaining {}",
            res,
            did_read,
            buf.filled().len(),
            buf.remaining()
        );
        res
    }
}

struct StreamHandle {
    recv_tx: Sender<EventIncoming>,
}

#[derive(Clone)]
pub struct UdxSocket(Arc<Mutex<UdxSocketInner>>);

impl UdxSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let inner = UdxSocketInner::bind(addr).await?;
        let socket = Arc::new(Mutex::new(inner));
        let this = Self(socket);
        tokio::task::spawn({
            let socket = this.clone();
            async move {
                loop {
                    let res = socket.next().await;
                    if let Err(_) = res {
                        break;
                    }
                    // socket.next().await;
                }
            }
        });
        Ok(this)
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

    pub(crate) fn next(&self) -> SocketDriver {
        SocketDriver {
            socket: self.clone(),
        }
    }
}

pub struct UdxSocketInner {
    socket: UdpSocket,
    send_rx: Receiver<Arc<Packet>>,
    send_tx: Sender<Arc<Packet>>,
    streams: HashMap<u32, StreamHandle>,
    pending_send: Option<Arc<Packet>>,
    read_buf: Vec<u8>,
}

pub struct SocketDriver {
    socket: UdxSocket,
}

impl Future for SocketDriver {
    type Output = io::Result<SocketEvent>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut socket = self.socket.0.lock("UdxSocket::poll_drive");
        let ev = socket.poll_drive(cx);
        drop(socket);
        ev
    }
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

    // pub fn next<'a>(&'a mut self) -> NextFuture<'a> {
    //     NextFuture { socket: self }
    // }

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
        let rto = Duration::from_millis(1000);
        let stream = UdxStreamInner {
            recv_rx,
            remote_id,
            local_id,
            seq: 0,
            ack: 0,
            inflight: 0,
            cwnd: 2 * UDX_MTU,
            remote_acked: 0,
            read_cursor: 0,
            srtt: Duration::ZERO,
            rttvar: Duration::ZERO,
            rto_timeout: Box::pin(tokio::time::sleep(rto)),
            rto,
            send_tx: self.send_tx.clone(),
            send_queue: VecDeque::new(),
            outgoing: HashMap::new(),
            incoming: BTreeMap::new(),
            remote_addr: dest,
            read_waker: None,
            write_waker: None,
            drive_waker: None,
            error: None,
        };
        let handle = StreamHandle { recv_tx };
        self.streams.insert(local_id, handle);
        let stream = UdxStream(Arc::new(Mutex::new(stream)));
        tokio::task::spawn({
            let stream = stream.clone();
            async move {
                StreamDriver { stream }.await;
            }
        });
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
        match self.socket.poll_send_to(cx, &packet.buf, packet.dest) {
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

    fn poll_transmit(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        // process transmits
        let mut iters = 0;
        loop {
            iters += 1;
            if iters > MAX_LOOP {
                cx.waker().wake_by_ref();
                break;
            }
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
        Ok(())
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        // process recv
        let mut iters = 0;
        loop {
            iters += 1;
            if iters > MAX_LOOP {
                cx.waker().wake_by_ref();
                break;
            }
            let (len, header, _peer) = {
                // todo: vectorize
                let mut buf = ReadBuf::new(&mut self.read_buf);
                let peer = match self.socket.poll_recv_from(cx, &mut buf)? {
                    Poll::Pending => break,
                    Poll::Ready(peer) => peer,
                };
                let header = Header::from_bytes(&buf.filled())?;
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
        Ok(())
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<SocketEvent>> {
        self.poll_recv(cx)?;
        self.poll_transmit(cx)?;
        Poll::Pending
    }
}

pub enum SocketEvent {
    UnknownStream,
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
    pub fn has_typ(&self, typ: u32) -> bool {
        self.typ & typ != 0
    }
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

    fn from_stream(stream: &UdxStreamInner, typ: u32) -> Self {
        let header = Header {
            stream_id: stream.remote_id,
            typ,
            seq: stream.seq,
            ack: stream.ack,
            data_offset: 0,
            recv_win: u32::MAX,
        };
        header
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
        buf[2..3].copy_from_slice(&(self.typ as u8).to_le_bytes());
        buf[3..4].copy_from_slice(&(self.data_offset as u8).to_le_bytes());
        buf[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.recv_win.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..20].copy_from_slice(&self.ack.to_le_bytes());
        return true;
    }
}

fn read_u32_le(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf.try_into().unwrap())
}
