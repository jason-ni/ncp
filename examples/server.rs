use bytes::BytesMut;
use ncp::{errors::Result, serve};
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;

async fn run() -> Result<()> {
    let listen_addr: SocketAddr = "127.0.0.1:7774".parse()?;
    let (_write_stream, mut read_stream) = serve(listen_addr).await?;
    let mut buf = BytesMut::with_capacity(1500);
    loop {
        buf.resize(1500, 0u8);
        log::debug!("before read");
        let size = read_stream.read(buf.as_mut()).await?;
        let msg = buf.as_ref()[..size].to_vec();
        log::debug!("after read: {}", pretty_hex::pretty_hex(&msg));
    }
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
    let _ = rt.block_on(run()).map_err(|e| {
        log::error!("failed to run: {:?}", e);
    });
}
