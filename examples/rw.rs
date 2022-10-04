use std::net::{SocketAddr, ToSocketAddrs};

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
    let args = std::env::args().collect::<Vec<_>>();
    let listen_port = args.get(1).expect("listen port is required");
    let connect_port = args.get(2).expect("connect port is required");
    let listen_addr = format!("127.0.0.1:{}", listen_port);
    let connect_addr = format!("127.0.0.1:{}", connect_port);
    eprintln!("{} -> {}", listen_addr, connect_addr);
    let connect_addr: SocketAddr = connect_addr
        .to_socket_addrs()?
        .next()
        .expect("invalid connect addr");
    eprintln!("{} -> {}", listen_addr, connect_addr);
    let mut sock = UdxSocket::bind(listen_addr).await?;
    let stream = sock.connect(connect_addr, 1, 1)?;
    let max_len = 100;
    let read = spawn("read", read_loop(stream.clone(), "read", max_len));
    let write = spawn(
        "write",
        write_loop(stream.clone(), "write", "olamundo", max_len),
    );
    write.await;
    eprintln!("write finished");
    read.await;
    eprintln!("read finished");
    eprintln!("finish {:?}", stream);
    Ok(())
}

async fn read_loop(mut stream: UdxStream, name: &str, max_len: usize) -> io::Result<()> {
    let mut buf = vec![0u8; 2048];
    let mut len = 0;
    loop {
        let n = stream.read(&mut buf).await?;
        len += n;
        eprintln!("[{} read ] {}", name, to_string(&buf[..n]));
        if len >= max_len {
            break;
        }
    }
    eprintln!("[{} read finish after {}", name, len);
    Ok(())
}

async fn write_loop(
    mut stream: UdxStream,
    name: &str,
    message: &str,
    max_len: usize,
) -> io::Result<()> {
    let mut i = 0;
    let mut len = 0;
    loop {
        let msg = format!(" {}#{} ", message, i);
        stream.write_all(msg.as_bytes()).await?;
        len += msg.len();
        i += 1;
        if len > max_len {
            break;
        }
    }
    eprintln!("[{} write finish after {}", name, len);
    // stream.close().await?;
    Ok(())
}
fn to_string(buf: &[u8]) -> String {
    String::from_utf8(buf.to_vec()).unwrap_or_else(|_| format!("<invalid bytes {:?}>", buf))
}
