use async_udx::UdxSocket;
use pretty_bytes::converter::convert as fmtbytes;
use std::{io, time::Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let len = usize_from_env("LENGTH", 1024 * 1024 * 64);
    let num_streams = usize_from_env("STREAMS", 4);
    let iterations = usize_from_env("ITERATIONS", 1);
    for _i in 0..iterations {
        run(len, num_streams).await.unwrap();
    }
}

async fn run(total: usize, num_streams: usize) -> io::Result<()> {
    let socka = UdxSocket::bind("127.0.0.1:0").await?;
    let sockb = UdxSocket::bind("127.0.0.1:0").await?;
    let addra = socka.local_addr()?;
    let addrb = sockb.local_addr()?;

    let start = Instant::now();
    let len = total / num_streams;
    eprintln!(
        "sending {} over {} streams (each stream {})",
        fmtbytes(total as f64),
        num_streams,
        fmtbytes(len as f64),
    );
    let mut readers = vec![];
    let mut writers = vec![];
    for i in 1..=num_streams as u32 {
        let streama = socka.connect(addrb, i, i)?;
        let streamb = sockb.connect(addra, i, i)?;
        let read_buf = vec![0u8; len];
        let write_buf = vec![i as u8; len];
        if i % 2 == 0 {
            readers.push((streama, read_buf));
            writers.push((streamb, write_buf));
        } else {
            readers.push((streamb, write_buf));
            writers.push((streama, read_buf));
        }
    }

    let mut tasks: Vec<tokio::task::JoinHandle<io::Result<()>>> = vec![];

    while let Some(writer) = writers.pop() {
        let task = tokio::spawn(async move {
            let (mut writer, message) = writer;
            writer.write_all(&message).await?;
            Ok(())
        });
        tasks.push(task);
    }

    while let Some(reader) = readers.pop() {
        let task = tokio::spawn(async move {
            let (mut reader, mut read_buf) = reader;
            reader.read_exact(&mut read_buf).await?;
            Ok(())
        });
        tasks.push(task);
    }

    for task in tasks {
        task.await??;
    }
    let time = start.elapsed();
    let throughput = total as f64 / time.as_secs_f64();
    eprintln!("throughput: {}/s   time: {:?}", fmtbytes(throughput), time);
    Ok(())
}

fn usize_from_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .map(|x| {
            x.parse::<usize>()
                .expect(&format!("{} must be a number", name))
        })
        .unwrap_or(default)
}
