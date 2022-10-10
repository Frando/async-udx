#![allow(dead_code)]

use std::{io, time::Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
criterion_group!(server_benches, bench_throughput);
criterion_main!(server_benches);
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}
use async_udx::{UdxSocket, UdxStream};
// use async_udx::{UdxSocket, UdxStream};
fn bench_throughput(c: &mut Criterion) {
    tracing_subscriber::fmt().init();
    let rt = rt();
    eprintln!("setup");
    let mut group = c.benchmark_group("throughput");
    // let len: usize = 1024 * 1024 * 1; // 1M
    // let len: usize = 1024 * 3;
    // let lens = [1024 * 4, 1024 * 64, 1024 * 512, 1024 * 1024];
    // let lens = [1024 * 512, 1024 * 1024];
    let lens = [1024 * 512, 1024 * 4];
    let n_streams = [1, 4, 16, 64, 512];
    // let lens = [1024 * 64];
    // let lens = [1024 * 512];
    for n_streams in n_streams {
        for len in lens {
            group.throughput(Throughput::Bytes(len as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("udx_{}", n_streams), len as u64),
                &len,
                |b, len| {
                    // let num_streams = 10usize;
                    let num_streams = n_streams;
                    let limit = *len / num_streams;
                    b.to_async(&rt).iter_custom(|iters| async move {
                        let socka = UdxSocket::bind("127.0.0.1:0").await.unwrap();
                        let sockb = UdxSocket::bind("127.0.0.1:0").await.unwrap();
                        let addra = socka.local_addr().unwrap();
                        let addrb = sockb.local_addr().unwrap();
                        let mut readers = vec![];
                        let mut writers = vec![];
                        for i in 1..=num_streams as u32 {
                            let streama = socka.connect(addrb, i, i).unwrap();
                            let streamb = sockb.connect(addra, i, i).unwrap();
                            let read_buf = vec![0u8; limit];
                            let write_buf = vec![1u8; limit];
                            if i % 2 == 0 {
                                readers.push((streama, read_buf));
                                writers.push((streamb, write_buf));
                            } else {
                                readers.push((streamb, write_buf));
                                writers.push((streama, read_buf));
                            }
                        }

                        let start = Instant::now();
                        for _i in 0..iters {
                            let writers_task = tokio::spawn(async move {
                                let mut tasks = vec![];
                                while let Some(writer) = writers.pop() {
                                    let task = tokio::spawn(async move {
                                        let (mut writer, message) = writer;
                                        writer.write_all(&message).await.unwrap();
                                        (writer, message)
                                    });
                                    tasks.push(task);
                                }
                                let mut writers = vec![];
                                while let Some(task) = tasks.pop() {
                                    let writer = task.await.unwrap();
                                    writers.push(writer);
                                }
                                writers
                            });

                            let mut tasks = vec![];
                            while let Some(reader) = readers.pop() {
                                let task = tokio::spawn(async move {
                                    let (mut reader, mut read_buf) = reader;
                                    reader.read_exact(&mut read_buf).await.unwrap();
                                    (reader, read_buf)
                                });
                                tasks.push(task);
                            }
                            while let Some(task) = tasks.pop() {
                                let reader = task.await.unwrap();
                                readers.push(reader);
                            }
                            writers = writers_task.await.unwrap();
                        }
                        let res = start.elapsed();
                        drop(socka);
                        drop(sockb);
                        res
                    })
                },
            );
        }
        // group.bench_with_input(
        //     BenchmarkId::new("udx_single_stream", len as u64),
        //     &len,
        //     |b, len| {
        //         let limit = *len;
        //         b.to_async(&rt).iter_custom(|iters| async move {
        //             let (mut wa, mut rb) = setup_pipe_udx().await.unwrap();
        //             // let mut wa = ra.clone();
        //             // let wb = rb.clone();
        //             let mut read_buf = vec![0u8; limit];
        //             // let mut rb = BufReader::new(wb);
        //             let start = Instant::now();
        //             let _message = vec![1u8; limit];
        //             for _i in 0..iters {
        //                 // eprintln!("####### ITER {}", i);
        //                 let twa = tokio::spawn(async move {
        //                     let message = vec![1u8; limit];
        //                     wa.write_all(&message).await.unwrap();
        //                     // eprintln!("write all! {}", message.len());
        //                     wa
        //                 });
        //                 rb.read_exact(&mut read_buf).await.unwrap();
        //                 // eprintln!("read all!");
        //                 wa = twa.await.unwrap();
        //             }
        //             let res = start.elapsed();
        //             // eprintln!("stats a {:?}", wa.stats());
        //             // eprintln!("stats b {:?}", wb.stats());
        //             res
        //         })
        //     },
        // );
    }
    //     group.bench_with_input(BenchmarkId::new("udx", len as u64), &len, |b, len| {
    //         let limit = *len;
    //         b.to_async(&rt).iter_custom(|iters| async move {
    //             let mut dur = Duration::new(0, 0);
    //             let (mut wa, wb) = setup_pipe_udx().await.unwrap();
    //             // let mut wa = ra.clone();
    //             // let wb = rb.clone();
    //             let mut read_buf = vec![0u8; limit];
    //             let mut rb = BufReader::new(wb);
    //             let start = Instant::now();
    //             for i in 0..iters {
    //                 let twa = tokio::spawn(async move {
    //                     let message = vec![1u8; limit];
    //                     wa.write_all(&message).await.unwrap();
    //                     wa
    //                 });
    //                 rb.read_exact(&mut read_buf).await.unwrap();
    //                 wa = twa.await.unwrap();
    //             }
    //             start.elapsed()
    //         })
    //     });
    // }
    group.finish();
    // group.sample_size(20);
    // group.bench_function(format!("pipe-udx-{}", len), |b| {
    //     b.to_async(&rt)
    //         .iter(|| async { run_pipe_udx(len).await.unwrap() })
    // });

    // group.bench_function(format!("pipe-udp-{}", len), |b| {
    //     b.to_async(&rt)
    //         .iter(|| async { run_pipe_udp(len).await.unwrap() })
    // });

    // group.bench_function(format!("pipe-tcp-{}", len), |b| {
    //     b.to_async(&rt)
    //         .iter(|| async { run_pipe_tcp(len).await.unwrap() })
    // });
}

async fn setup_pipe_tcp() -> io::Result<(TcpStream, TcpStream)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let writer = tokio::task::spawn(async move {
        let (stream, _peer_addr) = listener.accept().await.unwrap();
        stream
    });
    let reader = tokio::net::TcpStream::connect(addr).await?;
    let writer = writer.await?;
    Ok((reader, writer))
}

async fn setup_pipe_udx() -> io::Result<(UdxStream, UdxStream)> {
    let socka = UdxSocket::bind("127.0.0.1:0").await?;
    let sockb = UdxSocket::bind("127.0.0.1:0").await?;
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;
    let streama = socka.connect(addrb, 1, 2)?;
    let streamb = sockb.connect(addra, 2, 1)?;
    Ok((streama, streamb))
}

// async fn setup_pipe_udx() -> io::Result<(UdxStream, UdxStream)> {
//     let socka = UdxSocket::bind("127.0.0.1:0").await?;
//     let sockb = UdxSocket::bind("127.0.0.1:0").await?;
//     let addra = socka.local_addr()?;
//     let addrb = sockb.local_addr()?;
//     let streama = socka.connect(addrb, 1, 2)?;
//     let streamb = sockb.connect(addra, 2, 1)?;
//     Ok((streama, streamb))
// }

// trait IoStream: AsyncRead + AsyncWrite + Send + 'static {}
// impl IoStream for TcpStream {}
// impl IoStream for UdxStream {}
// async fn run_pipe(a: impl IoStream, b: impl IoStream, len: usize) {
//     // let fut_a = async move {
//     // }
// }

// async fn run_pipe<R, W>(
//     writer_a: W,
//     reader_a: R,
//     writer_b: W,
//     reader_b: R,
//     limit: usize,
//     bidi: bool,
// ) -> io::Result<()>
// where
//     R: AsyncRead + Send + Unpin + 'static,
//     W: AsyncWrite + Send + Unpin + 'static,
// {
//     let buf_size = 2048;
//     let msg_len = 1000;
//     let rb = run_read(reader_b, buf_size, limit);
//     let wa = run_write(writer_a, msg_len, limit);
//     let trb = tokio::spawn(rb);
//     let twa = tokio::spawn(wa);
//     if bidi {
//         let ra = run_read(reader_a, buf_size, limit);
//         let wb = run_write(writer_b, msg_len, limit);
//         let tra = tokio::spawn(ra);
//         let twb = tokio::spawn(wb);
//         tra.await?;
//         twb.await?;
//     }
//     trb.await?;
//     twa.await?;
//     Ok(())
// }

// async fn run_read(
//     mut reader: impl AsyncRead + Unpin,
//     buf_size: usize,
//     limit: usize,
// ) -> io::Result<()> {
//     let mut read_buf = vec![0u8; buf_size];
//     let mut len = 0;
//     while len < limit {
//         let n = reader.read(&mut read_buf).await?;
//         len += n;
//     }
//     Ok(())
// }

// async fn run_write(
//     mut writer: impl AsyncWrite + Unpin,
//     msg_size: usize,
//     limit: usize,
// ) -> io::Result<()> {
//     let msg = vec![1u8; msg_size];
//     let mut len = 0;
//     while len < limit {
//         writer.write_all(&msg).await?;
//         len += msg.len();
//     }
//     Ok(())
// }

// async setup_pipe_udx() -> io::Result<(UdxStream, UdxStream)> {
// }

// async fn run_pipe_tcp(len: usize) -> io::Result<()> {
//     let msg_size = 1000;
//     let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//     let addr = listener.local_addr()?;
//     // write task.
//     let w = tokio::task::spawn(async move {
//         let (stream, peer_addr) = listener.accept().await.unwrap();

//         let msg = vec![0u8; msg_size];
//         let mut sent = 0;
//         let mut writer = BufWriter::new(stream);
//         while sent <= len {
//             writer.write_all(&msg).await.unwrap();
//             sent += msg.len();
//         }
//     });

//     let mut stream = tokio::net::TcpStream::connect(addr).await?;
//     let mut read = 0;
//     let mut buf = vec![0u8; 2048];
//     while read <= len {
//         let n = stream.read(&mut buf).await?;
//         read += n;
//         if n == 0 {
//             break;
//         }
//     }

//     w.await;

//     Ok(())
// }

// async fn run_pipe_udx(len: usize) -> io::Result<()> {
//     let ((socka, mut streama), (sockb, mut streamb)) = setup_pair().await?;
//     let msg_size = 1000;
//     // write task.
//     let w = tokio::task::spawn(async move {
//         let msg = vec![0u8; msg_size];
//         let mut sent = 0;
//         let mut writer = BufWriter::new(streama);
//         while sent < len {
//             // eprintln!("wait for write");
//             writer.write_all(&msg).await.unwrap();
//             sent += msg.len();
//         }
//         // eprintln!("wrote {}", sent);
//     });

//     // read
//     let mut read = 0;
//     let mut buf = vec![0u8; 2048];
//     while read < len {
//         // eprintln!("wait for read");
//         let n = streamb.read(&mut buf).await?;
//         read += n;
//         // eprintln!("read {}", read);
//     }
//     w.await;
//     socka.close();
//     sockb.close();
//     Ok(())
// }

// async fn setup_pair() -> io::Result<((UdxSocket, UdxStream), (UdxSocket, UdxStream))> {
//     let socka = UdxSocket::bind("127.0.0.1:0").await?;
//     let sockb = UdxSocket::bind("127.0.0.1:0").await?;
//     let addra = socka.local_addr()?;
//     let addrb = sockb.local_addr()?;
//     let mut streama = socka.connect(addrb, 1, 2)?;
//     let mut streamb = sockb.connect(addra, 2, 1)?;
//     // let ta = socka.drive();
//     // let tb = sockb.drive();
//     Ok(((socka, streama), (sockb, streamb)))
// }

// async fn run_pipe_udp(len: usize) -> io::Result<()> {
//     let msg_size = 1000;
//     let socka = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
//     let sockb = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
//     let addra = socka.local_addr()?;
//     let addrb = sockb.local_addr()?;
//     let ack_interval = 10;
//     // write task.
//     let w = tokio::task::spawn(async move {
//         let msg = vec![0u8; msg_size];
//         let mut sent = 0;
//         let mut recv_buf = vec![0u8; 2048];
//         // ack every 10 packets.
//         let mut cnt = 0;
//         while sent <= len {
//             sockb.send_to(&msg, addra).await.unwrap();
//             sent += msg.len();
//             cnt += 1;
//             if cnt % ack_interval == 0 {
//                 // wait for ack.
//                 sockb.recv(&mut recv_buf).await.unwrap();
//             }
//         }
//     });

//     let mut read = 0;
//     let mut buf = vec![0u8; 2048];
//     let ack = vec![1u8; 1];
//     let mut cnt = 0;
//     while read <= len {
//         let n = socka.recv(&mut buf).await?;
//         read += n;
//         cnt += 1;
//         if cnt % ack_interval == 0 {
//             socka.send_to(&ack, addrb).await?;
//         }
//         if n == 0 {
//             break;
//         }
//     }
//     w.await;

//     Ok(())
// }
// group.bench_with_input(BenchmarkId::new("tcp", len as u64), &len, |b, len| {
//     let limit = *len;
//     b.to_async(&rt).iter_custom(|iters| async move {
//         // let mut dur = Duration::new(0, 0);
//         let (mut sa, mut sb) = setup_pipe_tcp().await.unwrap();
//         let (mut _ra, mut wa) = sa.into_split();
//         let (mut rb, mut _wb) = sb.into_split();
//         let mut read_buf = vec![0u8; limit];
//         let mut rb = BufReader::new(rb);
//         let start = Instant::now();
//         for i in 0..iters {
//             let twa = tokio::spawn(async move {
//                 let message = vec![1u8; limit];
//                 wa.write_all(&message).await.unwrap();
//                 wa
//             });
//             rb.read_exact(&mut read_buf).await.unwrap();
//             wa = twa.await.unwrap();
//         }
//         start.elapsed()
//     })
// });
