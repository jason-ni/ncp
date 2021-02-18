use bytes::BytesMut;
use ncp::conn;
use std::env;
use std::io::Read;
use std::net::SocketAddr;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn run() -> Result<(), anyhow::Error> {
    let mut args = env::args();
    args.next();
    let remote_addr_arg = args.next().expect("remote address must input");
    let remote_addr: SocketAddr = remote_addr_arg.parse()?;
    let file_name = args.next().expect("send file name must input");
    log::debug!("before open");
    let mut f = fs::File::open(file_name).await?;
    log::debug!("before connect");
    let (mut write_stream, _) = conn(remote_addr).await?;
    log::debug!("after connect");
    let mut buf = BytesMut::new();
    let mut total_size: usize = 0;
    loop {
        buf.resize(1024, 0u8);
        log::debug!("before read");
        let read_size = f.read(buf.as_mut()).await?;
        log::debug!("after read, {}", read_size);
        if read_size.eq(&0) {
            break;
        }
        write_stream.write_all(&buf[..read_size]).await?;
        log::debug!("after write");
        total_size += read_size;
    }
    write_stream.shutdown().await?;
    log::info!("total write: {}", total_size);
    Ok(())
}

fn main() {
    env_logger::init();
    let mut rt = tokio::runtime::Builder::new()
        .threaded_scheduler()
        .max_threads(8)
        .enable_all()
        .build()
        .unwrap();
    let _ = rt
        .block_on(run())
        .map_err(|e| log::error!("run error: {:?}", e));
}
