use derivative::Derivative;
use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};

use crate::mutex::Mutex;
// use async_std::io::Write as AsyncWrite;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::packet::{
    Header, OnAckCallback, Packet, PacketContext, PacketRef, PacketStatus, PendingRead,
    PktStreamWrite,
};

use crate::{constants::*, seq_compare, seq_diff, ProcessRes, SocketRef, UdxSocket};

pub type Time = u64;

#[derive(Debug)]
pub struct StreamRef(Arc<Mutex<UdxStream>>);

pub const MAX_WAITING: u32 = 8;

impl AsyncWrite for StreamRef {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut stream = self.0.lock("stream:poll_write");
        let req = PktStreamWrite {
            on_ack: None,
            packets: 0u32.into(),
            // handle: self.clone(),
            // data: vec![0u8; 0],
        };
        let n = stream.write(req, buf)?;
        if n < buf.len() {
            stream.write_wakers.push_back(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(n))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
    // fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    //     Poll::Ready(Ok(()))
    // }
}

impl AsyncRead for StreamRef {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut stream = self.0.lock("stream:poll_read");
        if stream.recv_bufs.is_empty() {
            // eprintln!("read starved, wait {:?}", stream.recv_bufs.len());
            stream.read_wakers.push_back(cx.waker().clone());
            return Poll::Pending;
        }
        // eprintln!("read resumed {:?}", stream.recv_bufs.len());
        let mut did_read = false;
        // eprintln!("poll_read {}", stream.recv_bufs.len());
        while let Some(recv_buf) = stream.recv_bufs.front() {
            if recv_buf.len() > buf.remaining() {
                break;
            }
            buf.put_slice(&recv_buf);
            let _ = stream.recv_bufs.pop_front();
            did_read = true;
        }
        if did_read {
            Poll::Ready(Ok(()))
        } else {
            stream.read_wakers.push_back(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Clone for StreamRef {
    fn clone(&self) -> Self {
        // self.lock("clone").ref_count += 1;
        Self(self.0.clone())
    }
}

impl std::ops::Deref for StreamRef {
    type Target = Mutex<UdxStream>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl StreamRef {
    pub fn new(stream: UdxStream) -> Self {
        Self(Arc::new(Mutex::new(stream)))
    }

    // pub fn write(&self, buf: &[u8], on_ack: Option<OnAckCallback>) -> io::Result<usize> {
    // }

    // pub fn poll_write(&self) {
    //     self.lock("poll_write").poll_write()
    // }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct UdxStream {
    pub local_id: u32,
    pub remote_id: u32,
    // pub set_id: usize,
    pub status: u32,
    // socket: Arc<UdxSocket>,
    //
    pub remote_addr: Option<SocketAddr>,
    pub local_addr: Option<SocketAddr>,

    #[derivative(Debug = "ignore")]
    pub socket: Option<SocketRef>,

    pub seq: u32,
    pub ack: u32,
    pub remote_acked: u32,
    pub remote_ended: u32,

    pub srtt: u32,
    pub rttvar: u32,
    pub rto: u32,

    pub rto_timeout: u64,

    /// how many packets are added locally but not sent?
    pub pkts_waiting: u32,
    /// packets inflight to the other peer
    pub pkts_inflight: u32,
    /// how many (data) packets received but not processed (out of order)?
    pub pkts_buffered: u32,
    /// how many duplicate acks received? Used for fast retransmit
    pub dup_acks: u32,
    /// how many retransmits are waiting to be sent? if 0, then inflight iteration is faster
    pub retransmits_waiting: u32,

    pub inflight: u32,
    pub ssthresh: u32,
    pub cwnd: u32,
    pub rwnd: u32,

    pub stats_sacks: usize,
    pub stats_pkts_sent: usize,
    pub stats_fast_rt: usize,

    pub outgoing: HashMap<u32, Packet>,
    pub incoming: HashMap<u32, PendingRead>,

    pub write_wakers: VecDeque<Waker>,
    pub read_wakers: VecDeque<Waker>,

    #[derivative(Debug = "ignore")]
    pub recv_bufs: VecDeque<Vec<u8>>,

    pub send_queue: VecDeque<PacketRef>,
}

// impl PacketRef {
//     fn ref(seq: u32) -> Self {
//         Self::Ref(seq)
//     }
//     fn packet(
// }

#[derive(Debug)]
pub enum SendRes {
    Sent,
    NotSent,
}

impl UdxStream {
    pub fn new(local_id: u32) -> Self {
        let rto = 1000;
        Self {
            status: 0,
            local_id,
            remote_id: 0,
            // set_id: 0,
            remote_addr: None,
            socket: None,
            local_addr: None,

            seq: 0,
            ack: 0,
            remote_acked: 0,
            remote_ended: 0,

            srtt: 0,
            rttvar: 0,
            rto,
            rto_timeout: get_milliseconds() + rto as u64,

            pkts_waiting: 0,
            pkts_inflight: 0,
            pkts_buffered: 0,
            dup_acks: 0,
            retransmits_waiting: 0,

            inflight: 0,
            ssthresh: 0xffff,

            cwnd: 2 * UDX_MTU,
            rwnd: 0,

            stats_sacks: 0,
            stats_pkts_sent: 0,
            stats_fast_rt: 0,

            outgoing: HashMap::with_capacity(16),
            incoming: HashMap::with_capacity(16),

            write_wakers: VecDeque::new(),
            read_wakers: VecDeque::new(),

            recv_bufs: VecDeque::new(),

            send_queue: VecDeque::new(),
        }
    }

    pub fn connected(&self) -> bool {
        return self.socket.is_some();
    }

    pub fn connect(
        &mut self,
        socket: SocketRef,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        remote_id: u32,
    ) -> Result<(), io::Error> {
        if self.status & UDX_STREAM_CONNECTED == 1 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Already connected",
            ));
        }

        // realloc socket.streams if needed.
        // not needed here, vecdeque should do this.

        self.status |= UDX_STREAM_CONNECTED;

        self.remote_id = remote_id;
        self.remote_addr = Some(remote_addr);
        // self.set_id = socket.streams_len();
        self.local_addr = Some(local_addr);
        self.socket = Some(socket);

        // handle->on_close = close_cb;

        // socket.streams[self.set_id] = self;
        // socket.streams_byid[self.set_id] = self;

        Ok(())
    }

    pub(crate) fn process_incoming(
        &mut self,
        header: &Header,
        buf: &[u8],
    ) -> io::Result<ProcessRes> {
        if self.status & UDX_STREAM_DEAD > 0 {
            return Ok(ProcessRes::NotHandled);
        }
        if header.r#type & UDX_HEADER_SACK > 0 {
            self.process_incoming_sacks(&header, &buf);
        }

        // For all self packets, ensure that they are causally newer (or same)
        if seq_compare(self.ack, header.seq) <= 0 {
            if header.r#type & UDX_HEADER_DATA_OR_END > 0
                && !self.incoming.contains_key(&header.seq)
                && self.status & UDX_STREAM_SHOULD_READ == UDX_STREAM_READ
            {
                let buf = buf.to_vec();
                self.process_incoming_data_packet(&header, buf);
            }

            if header.r#type & UDX_HEADER_END > 0 {
                self.status |= UDX_STREAM_ENDING_REMOTE;
                self.remote_ended = header.seq;
            }

            if header.r#type & UDX_HEADER_DESTROY > 0 {
                self.status |= UDX_STREAM_DESTROYED_REMOTE;
                // clear_outgoing_packets(self)
                // close_maybe(self)
                return Ok(ProcessRes::Handled);
            }
        }

        if header.r#type & UDX_HEADER_MESSAGE > 0 {
            let buf = buf.to_vec();
            self.on_recv(buf);
        }

        // process the read queue
        while (self.status & UDX_STREAM_SHOULD_READ) == UDX_STREAM_READ {
            let packet = self.incoming.remove(&self.ack);
            match packet {
                None => break,
                Some(packet) => {
                    self.pkts_buffered -= 1;
                    self.ack += 1;
                    if packet.r#type & UDX_HEADER_DATA > 0 {
                        self.on_read(Some(packet.buf));
                    }
                }
            }
        }

        // Check if the ack is oob.
        if seq_compare(self.seq, header.ack) < 0 {
            return Ok(ProcessRes::Handled);
        }

        // Congestion control...
        if self.remote_acked != header.ack {
            if self.cwnd < self.ssthresh {
                self.cwnd += UDX_MTU;
            } else {
                self.cwnd += ((UDX_MTU * UDX_MTU) / self.cwnd).max(1);
            }
        } else if header.r#type & UDX_HEADER_DATA_OR_END == 0 {
            self.dup_acks += 1;
            if self.dup_acks >= 3 {
                self.fast_retransmit();
            }
        }

        let len = seq_diff(header.ack, self.remote_acked);
        // eprintln!("recv ack {}", header.ack);
        for _i in 0..len {
            let seq = self.remote_acked;
            self.remote_acked += 1;
            match self.ack_packet(seq, false) {
                AckRes::Acked => continue,
                AckRes::Ended => {
                    // it ended, so ack that and trigger close
                    // TODO: make this work as well, if the ack packet is lost, ie
                    // have some internal (capped) queue of "gracefully closed" selfs
                    self.send_state_packet();
                    self.close_maybe(None);
                    return Ok(ProcessRes::Handled);
                }
                AckRes::NotFound => {
                    return Ok(ProcessRes::Handled);
                }
            }
        }
        // if data pkt, send an ack - use deferred acks as well...
        if header.r#type & UDX_HEADER_DATA_OR_END > 0 {
            self.send_state_packet();
        }

        if self.status & UDX_STREAM_SHOULD_END_REMOTE == UDX_STREAM_END_REMOTE
            && seq_compare(self.remote_ended, self.ack) <= 0
        {
            self.on_read(None);
            if self.close_maybe(None) {
                return Ok(ProcessRes::Handled);
            }
        }

        // if self.pkts_waiting > 0 {
        //     self.check_timeouts()?;
        // }

        Ok(ProcessRes::Handled)
    }

    fn process_incoming_data_packet(&mut self, header: &Header, buf: Vec<u8>) {
        // if header.seq == self.ack && header.r#type == UDX_HEADER_DATA {
        //     // Fast path - next in line, no need to memcpy it, stack allocate the struct and call on_read...
        //     self.ack += 1;
        //     self.on_read(Some(buf));
        //     return;
        // }

        let pending_read = PendingRead {
            seq: header.seq,
            r#type: header.r#type,
            buf,
        };
        self.incoming.insert(header.seq, pending_read);
        self.pkts_buffered += 1;
    }

    pub fn on_read(&mut self, buf: Option<Vec<u8>>) {
        if let Some(buf) = buf {
            self.recv_bufs.push_back(buf);
            // eprintln!("on_read {}", self.recv_bufs.len());
            while let Some(waker) = self.read_wakers.pop_front() {
                // eprintln!("wake read!");
                waker.wake();
            }
        }
    }

    pub fn on_recv(&mut self, _buf: Vec<u8>) {
        unimplemented!()
    }

    pub fn fast_retransmit(&mut self) {
        // unimplemented!()
    }

    pub fn send(&mut self, buf: Vec<u8>) {
        let mut packet = Packet::new_stream(UDX_HEADER_MESSAGE, &self, buf);
        packet.status = PacketStatus::Sending;
        packet.r#type = UDX_PACKET_STREAM_SEND;
        packet.ttl = 0;
        packet.transmits = 0;
        unimplemented!()
    }

    pub fn send_data_packet(&mut self, seq: u32) -> SendRes {
        let packet = self.outgoing.get_mut(&seq);
        if packet.is_none() {
            return SendRes::NotSent;
        }
        let packet = packet.unwrap();
        if self.inflight + packet.size as u32 > self.cwnd {
            return SendRes::NotSent;
        }

        assert!(matches!(packet.status, PacketStatus::Waiting));

        packet.status = PacketStatus::Sending;
        self.pkts_waiting -= 1;
        self.pkts_inflight += 1;
        self.inflight += packet.size as u32;
        if packet.transmits > 0 {
            self.retransmits_waiting -= 1;
        }
        self.stats_pkts_sent += 1;

        // pkt->fifo_gc = udx__fifo_push(&(stream->socket->send_queue), pkt);
        // self.queue_send(PacketRef::Ref(packet.seq));
        self.send_queue.push_back(PacketRef::Ref(seq));
        // int err = update_poll(stream->socket);
        // return err < 0 ? err : 1;
        SendRes::Sent
    }

    pub fn send_state_packet(&mut self) {
        let mut start = 0;
        let mut end = 0;
        let mut max = 512;
        let mut payload_len = 0;
        let mut sacks = None;
        let mut sacks_offset = 0;
        let mut i = 0;
        while i < max && payload_len < 400 {
            i += 1;
            let seq = self.ack + i;
            if payload_len >= 400 {
                break;
            }
            if !self.incoming.contains_key(&seq) {
                continue;
            }
            if sacks.is_none() {
                start = seq;
                end = seq + 1;
                sacks = Some(vec![0u8; 1024]);
            } else if seq == end {
                end += 1;
            } else {
                let sacks = sacks.as_mut().unwrap();
                encode_sacks(sacks, &mut sacks_offset, &mut payload_len, start, end);
                start = seq;
                end = seq + 1;
            }

            max = i + 512;
        }

        if start != end {
            encode_sacks(
                sacks.as_mut().unwrap(),
                &mut sacks_offset,
                &mut payload_len,
                start,
                end,
            );
        }

        let r#type = match sacks {
            Some(_) => UDX_HEADER_SACK,
            None => 0,
        };

        let buf = sacks.unwrap_or_default();

        let mut packet = Packet::new_stream(r#type, &self, buf);
        packet.status = PacketStatus::Sending;
        packet.r#type = UDX_PACKET_STREAM_STATE;
        packet.ttl = 0;

        // eprintln!("[{:?}] send state packet {:?}", self.local_addr, packet);

        self.stats_pkts_sent += 1;

        self.queue_send(PacketRef::Packet(packet));
    }

    pub fn queue_send(&mut self, packet: PacketRef) {
        self.send_queue.push_back(packet);
        // TODO: Don't unwrap.
        // self.socket
        //     .as_ref()
        //     .unwrap()
        //     .lock("sock:queue_send")
        //     .queue_send(packet)
    }

    pub fn close_maybe(&mut self, _err: Option<ErrorKind>) -> bool {
        // unimplemented!()
        true
    }

    pub fn process_incoming_sacks(&mut self, _header: &Header, buf: &[u8]) {
        let mut n = 0;
        let mut offset = 0;
        while offset + 8 <= buf.len() {
            let start = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let end = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let len = seq_diff(end, start);
            for j in 0..len {
                // eprintln!("from sacks! ack {}", start + j as u32);
                let a = self.ack_packet(start + j as u32, true);
                match a {
                    AckRes::Acked => {
                        n += 1;
                    }
                    AckRes::NotFound => {}
                    AckRes::Ended => {
                        return;
                    }
                }
            }
        }
        if n > 0 {
            self.stats_sacks += n;
        }
    }

    pub fn ack_packet(&mut self, seq: u32, sack: bool) -> AckRes {
        let packet = match self.outgoing.remove(&seq) {
            Some(pkt) => pkt,
            None => return AckRes::NotFound,
        };
        // eprintln!("got ack for {}, packet {:?}", seq, packet);
        if matches!(packet.status, PacketStatus::Inflight) {
            self.pkts_inflight -= 1;
            self.inflight -= packet.size as u32;
        }

        if packet.transmits == 1 {
            let rtt = (get_milliseconds() - packet.time_sent) as u32;
            // First round trip time sample
            if self.srtt == 0 {
                self.srtt = rtt;
                self.rttvar = rtt / 2;
                self.rto = self.srtt + (4 * self.rttvar).max(UDX_CLOCK_GRANULARITY_MS);
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
            self.rto = (self.srtt + (4 * self.rttvar).max(UDX_CLOCK_GRANULARITY_MS)).max(1000);
        }

        if !sack {
            // Reset rto timer when new data is ack'ed (inorder)
            self.rto_timeout = get_milliseconds() + (self.rto as u64);
        }

        // If this packet was queued for sending we need to remove it from the queue.
        if matches!(packet.status, PacketStatus::Sending) {
            unimplemented!()
            // udx__fifo_remove(&(stream->socket->send_queue), pkt, pkt->fifo_gc);
        }

        let w = match packet.ctx {
            PacketContext::StreamWrite(ref write) => write,
            _ => unreachable!("Invalid packet context"),
        };
        // TODO: Check ordering.
        let _ = w.packets.fetch_add(1, Ordering::SeqCst);
        if w.packets.load(Ordering::SeqCst) != 0 {
            return AckRes::Acked;
        }
        if let Some(ref _on_ack) = w.on_ack {
            unimplemented!()
            // on_ack(0, sack)
        }

        if self.status & UDX_STREAM_DEAD > 0 {
            return AckRes::Ended;
        }

        // TODO: the end condition needs work here to be more "stateless"
        // ie if the remote has acked all our writes, then instead of waiting for retransmits, we should
        // clear those and mark as local ended NOW.
        if self.status & UDX_STREAM_SHOULD_END == UDX_STREAM_END
            && self.pkts_waiting == 0
            && self.pkts_inflight == 0
        {
            self.status |= UDX_STREAM_ENDED;
            return AckRes::Ended;
        }

        AckRes::Acked
    }

    pub fn write(&mut self, req: PktStreamWrite, buf: &[u8]) -> io::Result<usize> {
        if self.pkts_waiting > MAX_WAITING {
            return Ok(0);
        }
        if self.inflight == 0 {
            self.rto_timeout = get_milliseconds() + (self.rto as u64);
        }
        let mut cap = 0;
        let req = Arc::new(req);
        while cap < buf.len() {
            let buf_partial_len = if (buf.len() - cap) < UDX_MAX_DATA_SIZE {
                buf.len() - cap
            } else {
                UDX_MAX_DATA_SIZE
            };
            let slice = &buf[cap..(cap + buf_partial_len)];
            cap += slice.len();
            let mut packet = Packet::new_stream(UDX_HEADER_DATA, &self, slice.to_vec());
            packet.status = PacketStatus::Waiting;
            packet.ttl = 0;

            // TODO: Check ordering.
            req.packets.fetch_add(1, Ordering::AcqRel);
            packet.ctx = PacketContext::StreamWrite(req.clone());

            self.seq += 1;
            self.pkts_waiting += 1;
            self.outgoing.insert(packet.seq, packet);

            if self.pkts_waiting > MAX_WAITING {
                return Ok(cap);
            }

            // If we are not the first packet in the queue, wait to send us until the queue is flushed...
            //            self.pkts_waiting += 1;
            //            if self.pkts_waiting > 1 {
            //                continue;
            //            } else {
            //                match self.send_data_packet(packet) {
            //                    SendRes::Sent => {
            //                        continue;
            //                    },
            //                    SendRes::NotSent => {
            //                        break;
            //                    }
            //                }
            //            }
        }
        Ok(cap)
    }

    pub fn check_timeouts(&mut self) -> io::Result<()> {
        if self.remote_acked == self.seq {
            return Ok(());
        }
        let now = if self.inflight > 0 {
            get_milliseconds()
        } else {
            0
        };
        let diff: i64 = self.rto_timeout as i64 - now as i64;
        // eprintln!("check timeouts, diff rto_timeout - now: {}", diff);
        // Timeout has passed, requeue and increase window.
        if now > self.rto_timeout {
            // Ensure it backs off until data is acked...
            self.rto_timeout = now + 2 * self.rto as u64;

            // Consider all packet losts - seems to be the simple consensus across different stream impls
            // which we like cause it is nice and simple to implement.
            for seq in self.remote_acked..self.seq {
                if let Some(packet) = self.outgoing.get_mut(&seq) {
                    if !matches!(packet.status, PacketStatus::Inflight) {
                        continue;
                    }
                    if packet.transmits >= UDX_MAX_TRANSMITS {
                        self.status |= UDX_STREAM_DESTROYED;
                        self.close_maybe(Some(ErrorKind::TimedOut));
                        return Err(io::Error::new(
                            ErrorKind::TimedOut,
                            "Remote did not ack enough packets",
                        ));
                    }
                    packet.status = PacketStatus::Waiting;
                    self.pkts_waiting += 1;
                    self.inflight -= packet.size as u32;
                    self.pkts_inflight -= 1;
                    self.retransmits_waiting += 1;
                }
            }

            self.cwnd = (self.cwnd / 2).max(UDX_MTU);
            dbg!("pkt loss! stream is congested, scaling back (requeued the full window");
            // eprintln!("self {:#?}", self);
        }

        self.flush_waiting_packets();
        Ok(())
    }

    fn flush_waiting_packets(&mut self) {
        let _was_waiting = self.pkts_waiting;
        let mut seq = if self.retransmits_waiting > 0 {
            self.remote_acked
        } else {
            self.seq - self.pkts_waiting
        };
        let _sent = 0;
        // Iterate over packets between seq..self.seq and send. Break when send queue is full.
        while (seq != self.seq) && self.pkts_waiting > 0 {
            let packet = self.outgoing.get_mut(&seq);
            if let Some(packet) = packet {
                if packet.has_status(PacketStatus::Waiting) {
                    match self.send_data_packet(seq) {
                        SendRes::Sent => {}
                        SendRes::NotSent => {
                            // self.outgoing.insert(seq, packet);
                            break;
                        }
                    }
                }
            }
            seq += 1;
        }

        // TODO: retransmits are counted in pkts_waiting, but we (prob) should not count them
        // towards to drain loop - investigate that.
        // if (was_waiting > 0 && stream->pkts_waiting == 0 && stream->on_drain != NULL) {
        //   stream->on_drain(stream);
        // }

        // after flushing new writes may happen
        // eprintln!("flushed! waiting {}", self.pkts_waiting);
        if self.pkts_waiting < MAX_WAITING {
            while let Some(waker) = self.write_wakers.pop_front() {
                waker.wake();
            }
        }
    }
}

// enum UdxStreamWriter {
//     Writing(WriteFut),
//     Pending(Arc<Mutex<UdxStream>>),
// }
//

pub fn get_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub enum AckRes {
    NotFound,
    Acked,
    Ended,
}

fn encode_sacks(buf: &mut [u8], offset: &mut usize, len: &mut usize, start: u32, end: u32) {
    buf[*offset..(*offset + 4)].copy_from_slice(&start.to_le_bytes());
    *offset += 4;
    buf[*offset..(*offset + 4)].copy_from_slice(&end.to_le_bytes());
    *offset += 4;
    *len += 8;
}
