use async_udx::*;
use std::time::Duration;
use std::{future::Future, io};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

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
    let socka = UdxSocket::bind("127.0.0.1:20004").await?;
    let addra = socka.local_addr()?;
    eprintln!("socka {addra}");
    let sockb = UdxSocket::bind("127.0.0.1:20005").await?;
    let addrb = sockb.local_addr()?;
    eprintln!("sockb {addrb}");

    let mut socka = SocketRef::new(socka);
    let mut sockb = SocketRef::new(sockb);
    // // spawn2(async move {
    // spawn3("yayy", async move {
    //     eprintln!("OK!!!");
    //     Ok(())
    // });

    // eprintln!("ok");
    // let tf = spawn("foo", {
    //     let socka = socka.clone();
    //     async move { drive_loop(socka, "socka2").await }
    // });
    // tokio::task::spawn({
    //     let socka = socka.clone();
    //     async move {
    //         let name = "foo";
    //         eprintln!("spawn: {}", name);
    //         let fut = async move { drive_loop(socka, "socka2").await };
    //         match fut.await {
    //             Ok(_) => {}
    //             Err(err) => eprintln!("[{}] died: {}", name, err),
    //         }
    //     }
    // });
    let mut streama = socka.connect(addrb, 1, 2)?;
    let mut streamb = sockb.connect(addra, 2, 1)?;

    spawn("read a", read_loop(streama.clone(), "a"));
    let wa = spawn("write a", write_loop(streama.clone(), "a", "olamundo"));
    spawn("read b", read_loop(streamb.clone(), "b"));
    let wb = spawn("write b", write_loop(streamb.clone(), "b", "helloworld"));

    // tokio::time::sleep(Duration::from_secs(1)).await;
    spawn("drive a", drive_loop(socka.clone(), "a"));
    spawn("drive b", drive_loop(sockb.clone(), "b"));
    wa.await;
    wb.await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    eprintln!("streama {:#?}", *streama.lock(""));
    eprintln!("streamb {:#?}", *streamb.lock(""));

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

async fn drive_loop(mut socket: SocketRef, name: &str) -> io::Result<()> {
    let name = name.to_string();
    loop {
        eprintln!("[{}] drive start", name);
        let res = futures::future::poll_fn(|cx| {
            // eprintln!("[{}] poll in", name);
            let res = socket.lock("sock:outer poll_fn").poll(cx);
            // eprintln!("[{}] poll out {:?}", name, res);
            res
        })
        .await;
        eprintln!("[{}] drive res {:?}", name, res);
    }
}

async fn read_loop(mut stream: StreamRef, name: &str) -> io::Result<()> {
    let mut buf = vec![0u8; 2048];
    loop {
        let n = stream.read(&mut buf).await?;
        eprintln!("[{} read ] {}", name, to_string(&buf[..n]));
    }
}

async fn write_loop(mut stream: StreamRef, name: &str, message: &str) -> io::Result<()> {
    let mut i = 0;
    loop {
        let msg = format!(" {}#{} ", message, i);
        stream.write_all(&msg.as_bytes()).await?;
        if i % 100 == 0 {
            eprintln!("[{} wrote {}", name, i);
        }
        i += 1;
        if i > 100 {
            break;
        }
    }
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
