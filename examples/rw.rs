use std::net::{SocketAddr, ToSocketAddrs};

use std::{future::Future, io};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use async_udx::{UdxSocket, UdxStream, UDX_DATA_MTU};

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
    let max_len = UDX_DATA_MTU * 64;
    let read = spawn("read", read_loop(stream.clone()));
    let msg = vec![1u8; UDX_DATA_MTU * 8];
    let write = spawn("write", write_loop(stream.clone(), msg, max_len));
    write.await;
    eprintln!("write finished");
    read.await;
    eprintln!("read finished");
    eprintln!("finish {:?}", stream);
    Ok(())
}

async fn read_loop(mut stream: UdxStream) -> io::Result<()> {
    let mut buf = vec![0u8; UDX_DATA_MTU * 8];
    let mut len = 0;
    loop {
        let n = stream.read(&mut buf).await?;
        len += n;
        eprintln!("read {} total {}", n, len);
    }
}

async fn write_loop(mut stream: UdxStream, msg: Vec<u8>, limit: usize) -> io::Result<()> {
    let mut i = 0;
    let mut written = 0;
    loop {
        stream.write_all(&msg).await?;
        eprintln!("wrote {} total {}", msg.len(), written);
        written += msg.len();
        i += 1;
        if written > limit {
            break;
        }
    }
    eprintln!("write finish after {}", written);
    // stream.close().await?;
    Ok(())
}
fn to_string(buf: &[u8]) -> String {
    String::from_utf8(buf.to_vec()).unwrap_or_else(|_| format!("<invalid bytes {:?}>", buf))
}
