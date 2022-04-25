use async_udx::{UdxError, UdxSocket, UdxStream};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn stream_read_write() -> io::Result<()> {
    let ((socka, sockb), (mut streama, mut streamb)) = create_pair().await?;
    assert_eq!(socka.local_addr().unwrap(), streamb.remote_addr());

    let msg = vec![1, 2, 3];
    streama.write_all(&msg).await?;
    let mut read = vec![0u8; 3];
    streamb.read_exact(&mut read).await?;
    assert_eq!(msg, read);
    Ok(())
}

#[tokio::test]
async fn stream_close() -> io::Result<()> {
    let ((socka, sockb), (mut streama, mut streamb)) = create_pair().await?;
    assert_eq!(socka.local_addr().unwrap(), streamb.remote_addr());

    // write a message.
    let msg = vec![1, 2, 3];
    streama.write_all(&msg).await?;
    assert_eq!(streama.stats().inflight_packets, 1);

    // close the stream
    let close = streama.close();
    assert_eq!(streama.stats().inflight_packets, 1);

    // wait until closing is complete == all packages flushed
    close.await;
    assert_eq!(streama.stats().inflight_packets, 0);

    // ensure reading on other end still works
    let mut read = vec![0u8; 3];
    streamb.read_exact(&mut read).await?;
    assert_eq!(msg, read);

    // try to write on closed stream
    let res = streama.write_all(&msg).await;
    assert_eq!(res.err().unwrap().kind(), io::ErrorKind::ConnectionReset);

    eprintln!("streamb {:#?}", streamb);

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
