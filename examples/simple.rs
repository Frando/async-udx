#![allow(dead_code)]

use std::time::Instant;
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
    let socka = UdxSocket::bind("127.0.0.1:20004").await?;
    let addra = socka.local_addr()?;
    eprintln!("socka {addra}");
    let sockb = UdxSocket::bind("127.0.0.1:20005").await?;
    let addrb = sockb.local_addr()?;
    eprintln!("sockb {addrb}");
    let streama = socka.connect(addrb, 1, 1)?;
    let streamb = sockb.connect(addra, 1, 1)?;

    let message = vec![3u8; 3000];
    let limit = 1024 * 1024 * 64;
    let start = Instant::now();
    let wa = spawn(
        "write a",
        write_loop(streama.clone(), "a", message.clone(), limit),
    );
    let rb = spawn("read b", read_loop(streamb.clone(), "b", limit));
    wa.await?;
    rb.await?;
    let throughput = limit as f32 / start.elapsed().as_secs_f32() / (1024. * 1024.);
    eprintln!("finish");
    eprintln!("throughput: {} MB/s", throughput);
    eprintln!("stats streamA {:?}", streama.stats());
    eprintln!("stats streamB {:?}", streamb.stats());
    eprintln!("stats sockA {:?}", socka.stats());
    eprintln!("stats sockB {:?}", sockb.stats());
    Ok(())
}

async fn read_loop(mut stream: UdxStream, name: &str, max_len: usize) -> io::Result<()> {
    let mut buf = vec![0u8; max_len];
    stream.read_exact(&mut buf).await?;
    eprintln!("[{} read finish after {}", name, max_len);
    Ok(())
}

async fn write_loop(
    mut stream: UdxStream,
    _name: &str,
    message: Vec<u8>,
    max_len: usize,
) -> io::Result<()> {
    // let mut i = 0;
    let mut len = 0;
    loop {
        stream.write_all(&message[..]).await?;
        len += message.len();
        // i += 1;
        if len >= max_len {
            break;
        }
    }
    Ok(())
}
fn to_string(buf: &[u8]) -> String {
    String::from_utf8(buf.to_vec()).unwrap_or_else(|_| format!("<invalid bytes {:?}>", buf))
}
