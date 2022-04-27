use async_udx::{UdxError, UdxSocket, UdxStream};
use std::{io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn stream_read_write() -> io::Result<()> {
    eprintln!("ok go");
    run().await?;
    // drop(streama);
    // drop(streamb);
    // drop(socka);
    // drop(sockb);
    eprintln!("wait");
    tokio::time::sleep(Duration::from_secs(1)).await;
    eprintln!("done");
    Ok(())
}

async fn run() -> io::Result<()> {
    let ((socka, sockb), (mut streama, mut streamb)) = create_pair().await?;
    assert_eq!(socka.local_addr().unwrap(), streamb.remote_addr());

    let msg = vec![1, 2, 3];
    streama.write_all(&msg).await?;
    let mut read = vec![0u8; 3];
    streamb.read_exact(&mut read).await?;
    assert_eq!(msg, read);
    eprintln!("now drop");
    Ok(())
}

#[tokio::test]
async fn stream_close() -> io::Result<()> {
    let ((socka, sockb), (mut streama, mut streamb)) = create_pair().await?;
    assert_eq!(socka.local_addr().unwrap(), streamb.remote_addr());

    // write a message.
    let msg = vec![1, 2, 3];
    streama.write_all(&msg).await?;
    assert_eq!(streama.stats().inflight_packets, 1, "inflight 1 after send");

    // close the stream
    let close = streama.close();
    assert_eq!(
        streama.stats().inflight_packets,
        1,
        "inflight still 1 after send"
    );

    // wait until closing is complete == all packages flushed
    close.await;
    assert_eq!(
        streama.stats().inflight_packets,
        0,
        "inflight 0 after close await"
    );

    // ensure reading on other end still works
    let mut read = vec![0u8; 3];
    let res = streamb.read_exact(&mut read).await;
    let res = res?;
    assert_eq!(msg, read, "read ok");

    // try to write on closed stream
    let res = streama.write_all(&msg).await;
    assert_eq!(
        res.err().unwrap().kind(),
        io::ErrorKind::ConnectionReset,
        "stream closed"
    );
    // try to read on closed stream
    let res = streama.read(&mut read).await;
    assert_eq!(
        res.err().unwrap().kind(),
        io::ErrorKind::ConnectionReset,
        "stream closed"
    );

    Ok(())
}

async fn create_pair() -> io::Result<((UdxSocket, UdxSocket), (UdxStream, UdxStream))> {
    let mut socka = UdxSocket::bind("127.0.0.1:0").await?;
    let mut sockb = UdxSocket::bind("127.0.0.1:0").await?;
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;
    let streama = socka.connect(addrb, 1, 2)?;
    let streamb = sockb.connect(addra, 2, 1)?;
    Ok(((socka, sockb), (streama, streamb)))
}
