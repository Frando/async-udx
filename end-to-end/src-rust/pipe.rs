use async_udx::UdxSocket;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let socket = UdxSocket::bind("127.0.0.1:0").await?;
    let local_port = socket.local_addr()?.port();
    println!("{}", local_port);
    eprintln!("listening on localhost:{local_port}");
    let peer_port = match std::env::var("PEER_PORT") {
        Ok(port) => port.parse::<u16>()?,
        Err(_) => read_port().await?,
    };
    eprintln!("connect to localhost:{peer_port}");
    let peer_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), peer_port);
    let mut stream = socket.connect(peer_addr, 1, 1)?;

    let writer_task = tokio::spawn({
        let mut stream = stream.clone();
        async move {
            for i in 0..10 {
                let message = format!("hello from rust {}", i);
                eprintln!("[send] {}", message);
                stream.write_all(message.as_bytes()).await?;
            }
            anyhow::Result::<_, anyhow::Error>::Ok(())
        }
    });

    let mut buf = vec![0u8; 1024];
    while !stream.closed() {
        let len = stream.read(&mut buf).await?;
        eprintln!("[recv] {}", std::str::from_utf8(&buf[..len])?);
    }
    eprintln!("closing");
    writer_task.await??;
    stream.close().await;
    Ok(())
}

async fn read_port() -> anyhow::Result<u16> {
    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();
    let line = lines.next_line().await?.unwrap();
    let port = line.parse::<u16>()?;
    Ok(port)
}
