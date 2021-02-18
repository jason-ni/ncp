use bytes::BytesMut;
use ncp::{errors::Result, serve};
use std::env;
use std::net::SocketAddr;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn run() -> Result<()> {
    let mut args = env::args();
    args.next();
    let listen_addr_arg = args.next().expect("listen address must input");
    let listen_addr: SocketAddr = listen_addr_arg.parse()?;
    let file_name = args.next().expect("recv file name must input");
    let mut f = fs::File::create(file_name).await?;
    let (_write_stream, mut read_stream) = serve(listen_addr).await?;
    let mut buf = BytesMut::with_capacity(1500);
    loop {
        buf.resize(1500, 0u8);
        log::debug!("before read");
        let size = read_stream.read(buf.as_mut()).await?;
        if size.eq(&0) {
            log::debug!("read eof");
            break;
        }
        f.write_all(&buf[..size]).await?;
    }
    f.flush().await?;
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
