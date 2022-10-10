use bytes::BufMut;
use futures::{Future, FutureExt};
use std::collections::HashMap;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Debug};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tokio::sync::oneshot;
use tokio::time::Sleep;
use tracing::trace;

use crate::constants::{
    UDX_CLOCK_GRANULARITY_MS, UDX_HEADER_DATA, UDX_HEADER_END, UDX_MAX_DATA_SIZE,
    UDX_MAX_TRANSMITS, UDX_MTU,
};
use crate::error::UdxError;
use crate::mutex::{Mutex, MutexGuard};
use crate::packet::{Header, IncomingPacket, Packet, PacketRef};
use crate::socket::EventIncoming;
use crate::udp::{Transmit, UdpState};

const SSTHRESH: usize = 0xffff;
const MAX_SEGMENTS: usize = 10;

#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub pkts_transmitted: usize,
    pub pkts_received: usize,
    pub pkts_inflight: usize,

    pub bytes_transmitted: usize,
    pub bytes_received: usize,
    pub bytes_inflight: usize,
}

#[derive(Debug)]
enum StreamState {
    Open,
    LocalClosed,
    RemoteClosed,
}

#[derive(Clone)]
pub struct UdxStream(Arc<Mutex<UdxStreamInner>>);

pub struct StreamDriver(UdxStream);

impl Future for StreamDriver {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut stream = self.0.lock("UdxStream::poll_drive");
        if stream.drive_waker.is_some() {
            stream.drive_waker = None;
        }

        let should_continue = stream.poll_drive(cx);

        if stream.closed_and_drained() {
            if let Some(tx) = stream.on_close.take() {
                tx.send(()).ok();
            }
            return Poll::Ready(());
        }

        let should_continue = match should_continue {
            Ok(should_continue) => should_continue,
            Err(err) => {
                stream.terminate(err);
                false
            }
        };
        if should_continue {
            drop(stream);
            cx.waker().wake_by_ref();
        } else {
            stream.drive_waker = Some(cx.waker().clone());
        }

        Poll::Pending
    }
}

impl UdxStream {
    pub(crate) fn connect(
        recv_rx: Receiver<EventIncoming>,
        send_tx: Sender<Transmit>,
        udp_state: Arc<UdpState>,
        dest: SocketAddr,
        // _local_id: u32,
        remote_id: u32,
    ) -> Self {
        let rto = Duration::from_millis(1000);
        let stream = UdxStreamInner {
            recv_rx,
            remote_id,
            // local_id,
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
            send_tx,
            send_queue: VecDeque::new(),
            outgoing: HashMap::new(),
            incoming: BTreeMap::new(),
            remote_addr: dest,
            read_waker: None,
            write_waker: None,
            drive_waker: None,
            error: None,
            on_close: None,
            stats: Default::default(),
            state: StreamState::Open,
            udp_state,
        };
        let stream = UdxStream(Arc::new(Mutex::new(stream)));
        let driver = StreamDriver(stream.clone());
        tokio::task::spawn(async move { driver.await });
        stream
    }

    pub fn close(&self) -> impl Future<Output = ()> {
        let mut stream = self.lock("UdxStream::close");
        stream.terminate(UdxError::close_graceful());
        let (tx, rx) = oneshot::channel();
        stream.on_close = Some(tx);
        drop(stream);
        let rx = rx.map(|_r| ());
        rx
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.lock("UdxStream::dest").remote_addr
    }

    pub fn stats(&self) -> StreamStats {
        (*self.lock("UdxStream::stats").stats()).clone()
    }

    pub(crate) fn lock(&self, reason: &'static str) -> MutexGuard<'_, UdxStreamInner> {
        self.0.lock(reason)
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
pub(crate) struct UdxStreamInner {
    send_tx: Sender<Transmit>,
    recv_rx: Receiver<EventIncoming>,

    incoming: BTreeMap<u32, IncomingPacket>,
    outgoing: HashMap<u32, Arc<Packet>>,
    send_queue: VecDeque<PacketRef>,

    remote_id: u32,
    remote_addr: SocketAddr,

    seq: u32,
    ack: u32,
    remote_acked: u32,
    inflight: usize,
    cwnd: usize,
    rto: Duration,
    rto_timeout: Pin<Box<Sleep>>,
    rttvar: Duration,
    srtt: Duration,

    read_cursor: u32,

    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    drive_waker: Option<Waker>,

    error: Option<UdxError>,

    on_close: Option<oneshot::Sender<()>>,

    stats: StreamStats,
    state: StreamState,

    udp_state: Arc<UdpState>,
}

impl UdxStreamInner {
    fn create_header(&self, mut typ: u32) -> Header {
        if matches!(self.state, StreamState::LocalClosed) {
            typ = typ & UDX_HEADER_END;
        }
        Header {
            stream_id: self.remote_id,
            typ,
            seq: self.seq,
            ack: self.ack,
            data_offset: 0,
            recv_win: u32::MAX,
        }
    }

    fn create_packet(&self, typ: u32, body: &[u8]) -> Packet {
        let header = self.create_header(typ);
        let dest = self.remote_addr;
        Packet::new(dest, header, body)
    }

    fn terminate(&mut self, error: impl Into<UdxError>) {
        self.error = Some(error.into());
        self.state = StreamState::LocalClosed;
        self.send_state_packet();

        if let Some(waker) = self.read_waker.take() {
            waker.wake();
        }
        if let Some(waker) = self.write_waker.take() {
            waker.wake();
        }
        self.wake_driver();
    }

    fn closed_and_drained(&self) -> bool {
        self.error.is_some() && self.send_queue.is_empty() && self.outgoing.is_empty()
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        self.poll_check_timeouts(cx)?;
        self.poll_incoming(cx)?;
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
                        if transmits >= UDX_MAX_TRANSMITS as usize {
                            None
                        } else {
                            Some(transmits + 1)
                        }
                    },
                );
                match may_retransmit {
                    Ok(_) => {
                        trace!("queue retransmit {}", packet.header.seq);
                        self.send_queue.push_back(PacketRef::Shared(packet.clone()));
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
        let max_segments = self.udp_state.max_gso_segments().min(MAX_SEGMENTS);
        // eprintln!("poll_transmit send_queue {:#?}", self.send_queue);
        // let mut num_datagrams = 0;
        let mut transmits = VecDeque::new();
        let mut queue = VecDeque::new();
        let mut segment_size = 0;
        while let Some(packet) = self.send_queue.pop_front() {
            if packet.buf.len() != segment_size {
                queue_transmits(
                    &mut transmits,
                    &mut queue,
                    segment_size,
                    max_segments,
                    self.remote_addr,
                );
            }
            segment_size = packet.buf.len();
            queue.push_back(packet);
        }
        queue_transmits(
            &mut transmits,
            &mut queue,
            segment_size,
            max_segments,
            self.remote_addr,
        );
        // eprintln!("poll_transmit transmits {:#?}", transmits);

        while let Some(transmit) = transmits.pop_front() {
            match self.send_tx.send(transmit) {
                Ok(()) => {}
                Err(_packet) => unimplemented!(),
            }
        }
    }

    fn poll_incoming(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let mut remote_ack = 0;
        loop {
            let next_event = Pin::new(&mut self.recv_rx).poll_recv(cx);
            match next_event {
                Poll::Pending => break,
                Poll::Ready(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "Socket driver future was dropped",
                    ))
                }
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
        Ok(())
    }

    fn send_acks(&mut self) {
        while self.incoming.contains_key(&self.ack) {
            self.ack += 1;
            self.send_state_packet();
        }
    }

    fn handle_remote_ack(&mut self, ack: u32) {
        if ack <= self.remote_acked {
            return;
        }

        while self.remote_acked < ack {
            // TODO: Clear from send queue.
            self.remote_acked += 1;
            let packet = self.outgoing.remove(&(self.remote_acked - 1));
            if let Some(packet) = packet {
                self.handle_ack(&packet);
            } else {
                // Received invalid ack (too high)
                tracing::error!("received invalid ack (too high)");
            }
        }
    }

    fn handle_ack(&mut self, packet: &Packet) {
        self.inflight -= packet.buf.len();
        self.stats.bytes_inflight -= packet.data_len();
        self.stats.pkts_inflight -= 1;

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

        self.rto_timeout
            .as_mut()
            .reset(tokio::time::Instant::now().checked_add(self.rto).unwrap());

        if (self.inflight + UDX_MTU) <= self.cwnd {
            if let Some(waker) = self.write_waker.take() {
                waker.wake();
            }
        }
    }

    fn handle_incoming(&mut self, packet: IncomingPacket) {
        self.stats.pkts_received += 1;
        // congestion control..
        if packet.ack() != self.remote_acked {
            if self.cwnd < SSTHRESH {
                self.cwnd += UDX_MTU;
            } else {
                self.cwnd += ((UDX_MTU * UDX_MTU) / self.cwnd).max(1);
            }
        }

        if packet.has_type(UDX_HEADER_END) {
            self.state = StreamState::RemoteClosed;
            self.terminate(UdxError::closed_by_remote());
        }

        // process incoming data
        if packet.has_type(UDX_HEADER_DATA) {
            self.stats.bytes_received += packet.data_len();
            let seq = packet.seq();
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

    fn stats(&self) -> &StreamStats {
        &self.stats
    }

    fn send_state_packet(&mut self) {
        let packet = self.create_packet(0, &[]);
        self.send_queue.push_back(PacketRef::Owned(packet));
        self.wake_driver();
        self.stats.pkts_transmitted += 1;
    }

    fn send_data_packet(&mut self, buf: &[u8]) -> usize {
        let len = buf.len().min(UDX_MAX_DATA_SIZE);
        if self.inflight + len > self.cwnd {
            return 0;
        }
        let packet = self.create_packet(UDX_HEADER_DATA, &buf[..len]);
        let packet = Arc::new(packet);

        self.inflight += packet.buf.len();
        self.seq += 1;

        self.stats.bytes_inflight += packet.data_len();
        self.stats.bytes_transmitted += packet.data_len();
        self.stats.pkts_transmitted += 1;
        self.stats.pkts_inflight += 1;

        self.outgoing.insert(packet.seq(), Arc::clone(&packet));
        self.send_queue.push_back(PacketRef::Shared(packet));
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
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut written = 0;
        loop {
            if written >= buf.len() {
                return Poll::Ready(Ok(written));
            }
            let n = self.send_data_packet(&buf[written..]);
            if n == 0 {
                return if written == 0 {
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
        // trace!(
        //     "poll read @ {} incoming len {}",
        //     self.ack,
        //     self.incoming.len()
        // );
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
        // debug!(
        //     "poll read res {:?} did_read {} filled {} remaining {}",
        //     res,
        //     did_read,
        //     buf.filled().len(),
        //     buf.remaining()
        // );
        res
    }
}

impl Debug for UdxStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.0.lock("UdxStream::debug");
        write!(f, "{:#?}", &*inner)
    }
}

fn queue_transmits(
    transmits: &mut VecDeque<Transmit>,
    queue: &mut VecDeque<PacketRef>,
    segment_size: usize,
    max_transmits: usize,
    remote_addr: SocketAddr,
) {
    while !queue.is_empty() {
        let segments = queue.len().min(max_transmits);
        let size = segments * segment_size;
        let mut buf = Vec::with_capacity(size);
        for _ in 0..segments {
            let packet = queue.pop_front().unwrap();
            buf.put_slice(packet.buf.as_slice());
        }
        let transmit = Transmit {
            destination: remote_addr,
            ecn: None,
            src_ip: None,
            segment_size: Some(UDX_MTU),
            contents: buf,
        };
        transmits.push_back(transmit);
    }
}
