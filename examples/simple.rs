use async_udx::UdxSocket;
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt::init();

    // Bind two sockets
    let socka = UdxSocket::bind("127.0.0.1:20004").await?;
    let addra = socka.local_addr()?;
    eprintln!("Socket A bound to {addra}");
    let sockb = UdxSocket::bind("127.0.0.1:20005").await?;
    let addrb = sockb.local_addr()?;
    eprintln!("Socket B bound to {addrb}");

    // On each socket, create a stream connected to the other socket.
    let streama = socka.connect(addrb, 1, 1)?;
    let streamb = sockb.connect(addra, 1, 1)?;

    // Spawn a task that writes data from A to B.
    let count = 16;
    let write_task = tokio::spawn({
        let mut streama = streama.clone();
        async move {
            for i in 0..count {
                let message = format!("hello from udx! this is message no. {}\n", i);
                streama.write_all(message.as_bytes()).await?;
            }
            Ok::<_, io::Error>(())
        }
    });

    // Read incoming data on the other end, line by line.
    let mut reader = BufReader::new(streamb.clone());
    for _i in 0..count {
        let mut buf = String::new();
        let _n = reader.read_line(&mut buf).await?;
        eprintln!("recv: {}", &buf[..buf.len() - 1]);
    }

    // Wait a bit so that all acks are processed.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    eprintln!("stats stream A {:#?}", streama.stats());
    eprintln!("stats stream B {:#?}", streamb.stats());
    eprintln!("stats socket A {:#?}", socka.stats());
    eprintln!("stats socket B {:#?}", sockb.stats());
    write_task.await??;
    Ok(())
}
