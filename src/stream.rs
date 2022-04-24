use futures::Future;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Debug};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tracing::{debug, trace};
// use tokio::sync::mpsc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Sleep;

use crate::constants::{UDX_HEADER_DATA, UDX_MAX_DATA_SIZE, UDX_MAX_TRANSMITS};

use crate::error::UdxError;
use crate::mutex::Mutex;
use crate::packet::{Header, IncomingPacket, Packet};
use crate::socket::EventIncoming;

const UDX_MTU: usize = 1400;
const UDX_CLOCK_GRANULARITY_MS: Duration = Duration::from_millis(20);

const MAX_TRANSMITS: u8 = UDX_MAX_TRANSMITS;

const SSTHRESH: usize = 0xffff;

const MAX_LOOP: usize = 50;

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

#[derive(Clone)]
pub struct UdxStream(Arc<Mutex<UdxStreamInner>>);

impl Debug for UdxStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.0.lock("UdxStream::debug");
        write!(f, "{:#?}", &*inner)
    }
}

impl UdxStream {
    pub(crate) fn connect(
        recv_rx: Receiver<EventIncoming>,
        send_tx: Sender<Arc<Packet>>,
        dest: SocketAddr,
        local_id: u32,
        remote_id: u32,
    ) -> Self {
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
            send_tx,
            send_queue: VecDeque::new(),
            outgoing: HashMap::new(),
            incoming: BTreeMap::new(),
            remote_addr: dest,
            read_waker: None,
            write_waker: None,
            drive_waker: None,
            error: None,
        };
        let stream = UdxStream(Arc::new(Mutex::new(stream)));
        let driver = StreamDriver {
            stream: stream.clone(),
        };
        tokio::task::spawn(async move { driver.await });
        stream
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
    incoming: BTreeMap<u32, IncomingPacket>,
    outgoing: HashMap<u32, Arc<Packet>>,
    send_tx: Sender<Arc<Packet>>,
    recv_rx: Receiver<EventIncoming>,

    pub seq: u32,
    pub remote_acked: u32,
    pub ack: u32,

    srtt: Duration,
    rttvar: Duration,

    inflight: usize,
    cwnd: usize,

    read_cursor: u32,

    // pkts_waiting: usize,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    pub(crate) drive_waker: Option<Waker>,

    pub remote_id: u32,
    pub local_id: u32,

    remote_addr: SocketAddr,

    send_queue: VecDeque<Arc<Packet>>,

    rto: Duration,
    rto_timeout: Pin<Box<Sleep>>,

    error: Option<UdxError>,
}

impl UdxStreamInner {
    fn create_header(&self, typ: u32) -> Header {
        let header = Header {
            stream_id: self.remote_id,
            typ,
            seq: self.seq,
            ack: self.ack,
            data_offset: 0,
            recv_win: u32::MAX,
        };
        header
    }

    fn create_packet(&self, typ: u32, body: &[u8]) -> Arc<Packet> {
        let header = self.create_header(typ);
        let dest = self.remote_addr;
        Arc::new(Packet::new(dest, header, body))
    }

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
        let packet = self.create_packet(0, &[]);
        self.send_queue.push_back(packet);
        self.wake_driver();
    }

    fn send_data_packet(&mut self, buf: &[u8]) -> usize {
        let len = buf.len().min(UDX_MAX_DATA_SIZE);
        if self.inflight + len > self.cwnd {
            return 0;
        }
        let packet = self.create_packet(UDX_HEADER_DATA, &buf[..len]);
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
