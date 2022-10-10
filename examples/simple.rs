#![allow(dead_code)]

use std::time::{Duration, Instant};
use std::{future::Future, io};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use async_udx::{UdxSocket, UdxStream};
pub fn spawn<T>(name: impl ToString, future: T) -> JoinHandle<()>
where
    T: Future<Output = io::Result<()>> + Send + 'static, // T::Output: Send + 'static,
{
    let name = name.to_string();
    eprintln!("[{}] spawn", name);
    tokio::task::spawn(async move {
        match future.await {
            Ok(_) => eprintln!("[{}] end", name),
            Err(err) => eprintln!("[{}] error {}", name, err),
        }
    })
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt::init();
    let mut socka = UdxSocket::bind("127.0.0.1:20004").await?;
    let addra = socka.local_addr()?;
    eprintln!("socka {addra}");
    let mut sockb = UdxSocket::bind("127.0.0.1:20005").await?;
    let addrb = sockb.local_addr()?;
    eprintln!("sockb {addrb}");
    let streama = socka.connect(addrb, 1, 2)?;
    let streamb = sockb.connect(addra, 2, 1)?;

    let message = vec![3u8; 3000];
    let limit = 1024 * 1024 * 64;
    // let limit = 3000;
    let start = Instant::now();
    // let ra = spawn("read a", read_loop(streama.clone(), "a", max_len));
    let wa = spawn(
        "write a",
        write_loop(streama.clone(), "a", message.clone(), limit),
    );
    let rb = spawn("read b", read_loop(streamb.clone(), "b", limit));
    // let wb = spawn(
    //     "write b",
    //     write_loop(streamb.clone(), "b", message, max_len),
    // );

    // tokio::time::sleep(Duration::from_secs(1)).await;
    // spawn("drive a", drive_loop(socka, "a"));
    // spawn("drive b", drive_loop(sockb, "b"));
    wa.await;
    // wb.await;
    // ra.await;
    rb.await;
    let throughput = limit as f32 / start.elapsed().as_secs_f32() / (1024. * 1024.);
    eprintln!("finish");
    eprintln!("throughput: {} MB/s", throughput);
    // tokio::time::sleep(Duration::from_secs(1)).await;
    // eprintln!("finish a {:?}", streama);
    // eprintln!("finish b {:?}", streamb);
    // tokio::time::sleep(Duration::from_millis(2000)).await;
    // eprintln!("streama {:#?}", *streama.lock(""));
    // eprintln!("streamb {:#?}", *streamb.lock(""));

    // tf.await;
    // tokio::task::spawn(async move {
    //     read(streama).await;
    // });
    // tokio::task::spawn(async move {
    //     stream_loop(streamb).await;
    // });
    // let streama = socka.connect(
    Ok(())
}

// async fn drive_loop(mut socket: UdxSocket, name: &str) -> io::Result<()> {
//     let name = name.to_string();
//     loop {
//         let ev = socket.next().await?;
//     }
//     // loop {
//     //     eprintln!("[{}] drive start", name);
//     //     let res = futures::future::poll_fn(|cx| {
//     //         // eprintln!("[{}] poll in", name);
//     //         // let res = socket.lock("sock:outer poll_fn").poll(cx);
//     //         // eprintln!("[{}] poll out {:?}", name, res);
//     //         res
//     //     })
//     //     .await;
//     //     eprintln!("[{}] drive res {:?}", name, res);
//     // }
// }

async fn read_loop(mut stream: UdxStream, name: &str, max_len: usize) -> io::Result<()> {
    let mut buf = vec![0u8; max_len];
    stream.read_exact(&mut buf).await?;

    // let mut len = 0;
    // loop {
    //     let n = stream.read(&mut buf).await?;
    //     len += n;
    //     eprintln!("[{} read ] {}", name, to_string(&buf[..n]));
    //     if len >= max_len {
    //         break;
    //     }
    // }
    eprintln!("[{} read finish after {}", name, max_len);
    Ok(())
}

async fn write_loop(
    mut stream: UdxStream,
    _name: &str,
    message: Vec<u8>,
    max_len: usize,
) -> io::Result<()> {
    let mut i = 0;
    let mut len = 0;
    loop {
        // let msg = format!(" {}#{} ", message, i);
        stream.write_all(&message[..]).await?;
        len += message.len();
        i += 1;
        if len >= max_len {
            break;
        }
        // tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // eprintln!("[{} write finish after {}", name, len);
    // stream.close().await?;
    Ok(())
}
fn to_string(buf: &[u8]) -> String {
    String::from_utf8(buf.to_vec()).unwrap_or_else(|_| format!("<invalid bytes {:?}>", buf))
}

// async fn spawn2<T>(fut: T)
// where
//     T: Future<Output = io::Result<()>> + Send + 'static,
// {
//     tokio::task::spawn(async move {
//         match fut.await {
//             Ok(_) => {}
//             Err(err) => eprintln!("[{}] died: {}", "xxx", err),
//         }
//     });
// }
// async fn spawn<T>(name: &str, fut: T)
// where
//     T: Future<Output = io::Result<()>> + Send + 'static,
// {
//     let name = name.to_string();
//     tokio::task::spawn(async move {
//         eprintln!("spawn: {}", name);
//         match fut.await {
//             Ok(_) => {}
//             Err(err) => eprintln!("[{}] died: {}", name, err),
//         }
//     });
// }
// async fn stream_loop(mut stream: StreamRef) -> io::Result<()> {
//     let mut read_buf = vec![0u8; 2048];
//     let message = "hi from a";
//     loop {
//         tokio::select! {
//             read = stream.read(&mut read_buf) => {
//                 let n = read?;
//                 eprintln!("[a r] {}", to_string(&read_buf[..n]))
//             }
//             write = stream.write_all(message.as_bytes()) => {
//                 let write = write?;
//                 eprintln!("[a w] {}", message);
//             }

//         }
//         eprintln!("[a s]");
//         tokio::time::sleep(Duration::from_millis(1000));
//     }
// }
