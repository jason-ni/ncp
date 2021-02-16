use bytes::BytesMut;
use ncp::{conn, errors::Result};
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;

async fn run() -> Result<()> {
    let remote_addr: SocketAddr = "127.0.0.1:7774".parse()?;
    let (mut write_stream, _) = conn(remote_addr).await?;
    let mut buf = BytesMut::new();
    buf.extend_from_slice("0abcdefghijklmnopqrstuvwxyz1abcdefghijklmnopqrstuvwxyz2abcdefghijklmnopqrstuvwxyz3abcdefghijklmnopqrstuvwxyz4abcdefghijklmnopqrstuvwxyz".as_bytes());
    write_stream.write_all(buf.as_ref()).await?;
    tokio::time::delay_for(std::time::Duration::from_secs(100)).await;
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
    let _ = rt.block_on(run());
}
