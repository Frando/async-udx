use async_udx::{UdxSocket, UDX_DATA_MTU};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MSGSIZE: usize = UDX_DATA_MTU * 64;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let num_streams = 8;
    let iters = 8;

    let bytes = num_streams * iters * MSGSIZE;

    tracing::info!(
        "go ! streams {} msgsize {} iters {} total {}",
        num_streams,
        MSGSIZE,
        iters,
        bytes
    );

    let host = "127.0.0.1";
    let socka = UdxSocket::bind(format!("{host}:0")).await.unwrap();
    let sockb = UdxSocket::bind(format!("{host}:0")).await.unwrap();
    let addra = socka.local_addr().unwrap();
    let addrb = sockb.local_addr().unwrap();
    eprintln!("addra {}", addra);
    eprintln!("addrb {}", addrb);
    let mut readers = vec![];
    let mut writers = vec![];
    for i in 1..=num_streams {
        let i = i as u32;
        let streama = socka.connect(addrb, 1000 + i, i).unwrap();
        let streamb = sockb.connect(addra, i, 1000 + i).unwrap();
        let read_buf = vec![0u8; MSGSIZE as usize];
        let write_buf = vec![i as u8; MSGSIZE as usize];
        let (reader, writer) = if i % 2 == 0 {
            (streama, streamb)
        } else {
            (streamb, streama)
        };
        readers.push((reader, read_buf, i));
        writers.push((writer, write_buf, i));
    }

    let start = Instant::now();
    let writers_task = tokio::spawn(async move {
        let mut tasks = vec![];
        while let Some(writer) = writers.pop() {
            let task = tokio::spawn(async move {
                let (mut writer, message, i) = writer;
                for _i in 0..iters {
                    writer.write_all(&message).await.unwrap();
                }
                // eprintln!("wrote!");
                // eprintln!(
                //     "writer stats {:?} remote_addr {}",
                //     writer.stats(),
                //     writer.remote_addr()
                // );
                (writer, message, i)
            });
            tasks.push(task);
        }
        let mut writers = vec![];
        while let Some(task) = tasks.pop() {
            let writer = task.await.unwrap();
            writers.push(writer);
        }
        writers
    });

    let mut tasks = vec![];
    while let Some(reader) = readers.pop() {
        let task = tokio::spawn(async move {
            let (mut reader, mut read_buf, i) = reader;
            for _i in 0..iters {
                reader.read_exact(&mut read_buf).await.unwrap();
            }
            assert_eq!(read_buf.as_slice(), &[i as u8; MSGSIZE][..]);
            // eprintln!("read! {:?}", read_buf);
            // eprintln!(
            //     "reader stats {:?} remote_addr {}",
            //     reader.stats(),
            //     reader.remote_addr()
            // );
            (reader, read_buf, i)
        });
        tasks.push(task);
    }
    while let Some(task) = tasks.pop() {
        let reader = task.await.unwrap();
        readers.push(reader);
    }
    writers_task.await.unwrap();
    let dur = start.elapsed();
    eprintln!("socka stats {:#?}", socka.stats());
    eprintln!("sockb stats {:#?}", sockb.stats());
    // eprintln!(
    //     "reader stats {:#?}",
    //     readers.iter().map(|r| r.0.stats()).collect::<Vec<_>>()
    // );
    // eprintln!(
    //     "writer stats {:#?}",
    //     writers.iter().map(|r| r.0.stats()).collect::<Vec<_>>()
    // );
    eprintln!("took: {:?}", dur);

    let throughput = bytes as f32 / start.elapsed().as_secs_f32() / (1024. * 1024.);

    eprintln!("throughput: {} MB/s", throughput);
}
