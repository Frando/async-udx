use async_udx::{UdxError, UdxSocket, UdxStream};
use std::{io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn socket_dgrams() -> io::Result<()> {
    let mut socka = UdxSocket::bind("127.0.0.1:0").await?;
    let mut sockb = UdxSocket::bind("127.0.0.1:0").await?;
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;

    let msg = "hi!".as_bytes();
    socka.send(addrb, &msg);
    let (from, buf) = sockb.recv().await?;
    assert_eq!(&buf, msg);
    assert_eq!(from, addra);

    Ok(())
}
