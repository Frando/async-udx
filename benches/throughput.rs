use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
criterion_group!(server_benches, bench_throughput);
criterion_main!(server_benches);
use async_udx::{SocketRef, StreamRef, UdxSocket};
fn bench_throughput(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("throughput");
    let len: usize = 1024 * 1024 * 1; // 1M
    group.throughput(Throughput::Bytes(len as u64));
    group.sample_size(20);

    group.bench_function(format!("pipe-udx-{}", len), |b| {
        b.to_async(&rt)
            .iter(|| async { run_pipe_udx(len).await.unwrap() })
    });

    group.bench_function(format!("pipe-udp-{}", len), |b| {
        b.to_async(&rt)
            .iter(|| async { run_pipe_udp(len).await.unwrap() })
    });

    group.bench_function(format!("pipe-tcp-{}", len), |b| {
        b.to_async(&rt)
            .iter(|| async { run_pipe_tcp(len).await.unwrap() })
    });
}

async fn run_pipe_tcp(len: usize) -> io::Result<()> {
    let msg_size = 1000;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    // write task.
    let w = tokio::task::spawn(async move {
        let (stream, peer_addr) = listener.accept().await.unwrap();

        let msg = vec![0u8; msg_size];
        let mut sent = 0;
        let mut writer = BufWriter::new(stream);
        while sent <= len {
            writer.write_all(&msg).await.unwrap();
            sent += msg.len();
        }
    });

    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let mut read = 0;
    let mut buf = vec![0u8; 2048];
    while read <= len {
        let n = stream.read(&mut buf).await?;
        read += n;
        if n == 0 {
            break;
        }
    }

    w.await;

    Ok(())
}

async fn run_pipe_udx(len: usize) -> io::Result<()> {
    let ((socka, mut streama), (sockb, mut streamb)) = setup_pair().await?;
    let msg_size = 1000;
    // write task.
    let w = tokio::task::spawn(async move {
        let msg = vec![0u8; msg_size];
        let mut sent = 0;
        let mut writer = BufWriter::new(streama);
        while sent < len {
            // eprintln!("wait for write");
            writer.write_all(&msg).await.unwrap();
            sent += msg.len();
        }
        // eprintln!("wrote {}", sent);
    });

    // read
    let mut read = 0;
    let mut buf = vec![0u8; 2048];
    while read < len {
        // eprintln!("wait for read");
        let n = streamb.read(&mut buf).await?;
        read += n;
        // eprintln!("read {}", read);
    }
    w.await;
    socka.close();
    sockb.close();
    Ok(())
}

async fn setup_pair() -> io::Result<((SocketRef, StreamRef), (SocketRef, StreamRef))> {
    let socka = UdxSocket::bind("127.0.0.1:0").await?;
    let sockb = UdxSocket::bind("127.0.0.1:0").await?;
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;
    let mut socka = SocketRef::new(socka);
    let mut sockb = SocketRef::new(sockb);
    let mut streama = socka.connect(addrb, 1, 2)?;
    let mut streamb = sockb.connect(addra, 2, 1)?;
    let ta = socka.drive();
    let tb = sockb.drive();
    Ok(((socka, streama), (sockb, streamb)))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

async fn run_pipe_udp(len: usize) -> io::Result<()> {
    let msg_size = 1000;
    let socka = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let sockb = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;
    let ack_interval = 10;
    // write task.
    let w = tokio::task::spawn(async move {
        let msg = vec![0u8; msg_size];
        let mut sent = 0;
        let mut recv_buf = vec![0u8; 2048];
        // ack every 10 packets.
        let mut cnt = 0;
        while sent <= len {
            sockb.send_to(&msg, addra).await.unwrap();
            sent += msg.len();
            cnt += 1;
            if cnt % ack_interval == 0 {
                // wait for ack.
                sockb.recv(&mut recv_buf).await.unwrap();
            }
        }
    });

    let mut read = 0;
    let mut buf = vec![0u8; 2048];
    let ack = vec![1u8; 1];
    let mut cnt = 0;
    while read <= len {
        let n = socka.recv(&mut buf).await?;
        read += n;
        cnt += 1;
        if cnt % ack_interval == 0 {
            socka.send_to(&ack, addrb).await?;
        }
        if n == 0 {
            break;
        }
    }
    w.await;

    Ok(())
}
