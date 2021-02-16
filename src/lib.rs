pub mod errors;
mod frame;
mod stream;

use crate::errors::{NcpError, Result};
use bytes::{Buf, BytesMut};
use std::collections::VecDeque;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::frame::{read_frame, write_frame, write_frame_to, NcpFrame, CMD_ACK, CMD_PUSH};
use crate::stream::WriterNoty;
pub use stream::{NcpStreamReader, NcpStreamWriter};
use tokio::net::udp::SendHalf;
use tokio::sync::mpsc::error::TrySendError;

pub struct NcpStreamInner {
    write_wnd: i16,
    send_wnd_limit: u32,
    send_sn: u32,
    send_una: u32,
    send_next: u32,
    send_queue: VecDeque<NcpSegment>,
    send_buf: VecDeque<NcpSegment>,
    recv_next: u32,
    recv_buf: VecDeque<NcpSegment>,
}

enum SegState {
    Init,
    WaitAck,
    Sent,
}

pub struct NcpSegment {
    seg_no: u32,
    state: SegState,
    data: Vec<u8>,
}

enum HandleMsg {
    RecvData(NcpFrame, SocketAddr),
    WriteData(Vec<u8>),
    CheckResend,
}

async fn run_with_log<F>(f: F) -> core::result::Result<(), ()>
where
    F: std::future::Future<Output = Result<()>>,
{
    f.await
        .map_err(|e| log::error!("loop exit with error: {:?}", e));
    log::debug!("after loop");
    Ok(())
}

pub async fn conn(remote_addr: SocketAddr) -> Result<(NcpStreamWriter, NcpStreamReader)> {
    let local_addr: SocketAddr = if remote_addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let mut socket = UdpSocket::bind(local_addr).await?;
    socket.connect(remote_addr).await?;
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let (write_noty_tx, write_noty) = mpsc::unbounded_channel();
    let (read_tx, read_rx) = mpsc::channel(16);
    tokio::spawn(run_with_log(ncp_loop(
        socket,
        write_rx,
        read_tx,
        write_noty_tx,
    )));
    Ok((
        NcpStreamWriter {
            write_wnd: 16,
            write_tx,
            write_noty,
        },
        NcpStreamReader {
            pending_buf: None,
            last_read: 0,
            read_rx,
        },
    ))
}

pub async fn serve(local_addr: SocketAddr) -> Result<(NcpStreamWriter, NcpStreamReader)> {
    let mut socket = UdpSocket::bind(local_addr).await?;
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let (write_noty_tx, write_noty) = mpsc::unbounded_channel();
    let (read_tx, read_rx) = mpsc::channel(16);
    tokio::spawn(run_with_log(ncp_loop(
        socket,
        write_rx,
        read_tx,
        write_noty_tx,
    )));
    Ok((
        NcpStreamWriter {
            write_wnd: 16,
            write_tx,
            write_noty,
        },
        NcpStreamReader {
            pending_buf: None,
            last_read: 0,
            read_rx,
        },
    ))
}

async fn ncp_loop(
    udp_sock: UdpSocket,
    mut write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut read_tx: mpsc::Sender<Vec<u8>>,
    mut write_noty_tx: mpsc::UnboundedSender<WriterNoty>,
) -> Result<()> {
    let mut ncp_inner = NcpStreamInner {
        write_wnd: 16,
        send_wnd_limit: 16,
        send_sn: 0,
        send_una: 0,
        send_next: 0,
        send_queue: Default::default(),
        send_buf: Default::default(),
        recv_next: 0,
        recv_buf: Default::default(),
    };
    let (mut sock_recv_hf, mut sock_send_hf) = udp_sock.split();
    let mut buf = BytesMut::with_capacity(1024);
    let mut check_arp_inverval = tokio::time::interval(std::time::Duration::from_secs(3));

    loop {
        log::debug!("loop begin");
        buf.resize(1024, 0u8);
        let msg = tokio::select! {
            res = sock_recv_hf.recv_from(buf.as_mut()) => {
                match res {
                    Ok((size, peer_addr)) => {
                        log::debug!("received data: {:?}", &buf[..size]);
                        let frame = read_frame(&buf[..size]);
                        HandleMsg::RecvData(frame, peer_addr)
                    }
                    Err(e) => {
                        return Err(NcpError::StdIoError(e))
                    }
                }
            },
            res = write_rx.recv() => {
                match res {
                    Some(write_data) => {
                        log::debug!("writing segment data: {:?}",write_data);
                        HandleMsg::WriteData(write_data)
                    }
                    None => continue
                }
            }
            _ = check_arp_inverval.tick() => {
                HandleMsg::CheckResend
            }
        };
        match msg {
            HandleMsg::RecvData(frame, peer_addr) => match frame.cmd {
                CMD_PUSH => {
                    log::debug!("received push data: {:?}", &frame.data);
                    if frame.sn.lt(&ncp_inner.recv_next) {
                        continue;
                    }
                    let mut idx = 0;
                    let mut insert = true;
                    for i in ncp_inner.recv_buf.iter() {
                        if frame.sn.eq(&i.seg_no) {
                            insert = false;
                            send_ack(frame.sn, &mut sock_send_hf, &peer_addr).await?;
                            break;
                        } else {
                            if frame.sn.lt(&i.seg_no) {
                                break;
                            }
                        }
                    }
                    if insert {
                        ncp_inner.recv_buf.insert(
                            idx,
                            NcpSegment {
                                seg_no: frame.sn,
                                state: SegState::Init,
                                data: frame.data,
                            },
                        );
                        send_ack(frame.sn, &mut sock_send_hf, &peer_addr).await?;
                    }
                }
                CMD_ACK => {
                    for i in ncp_inner.send_buf.iter_mut() {
                        if i.seg_no.eq(&frame.sn) {
                            log::debug!("=== setting segment {} to sent state", i.seg_no);
                            i.state = SegState::Sent;
                        }
                    }
                    while let Some(i) = ncp_inner.send_buf.pop_front() {
                        log::debug!("=== send_una: {}, seg_no: {}", ncp_inner.send_una, i.seg_no);
                        if i.seg_no.eq(&ncp_inner.send_una) {
                            ncp_inner.send_una = i.seg_no + 1;
                            ncp_inner.write_wnd += 1;
                            log::debug!("=== sendint increasing write wnd noty");
                            let _ = write_noty_tx.send(WriterNoty::WindowInc(1)).map_err(|e| {
                                log::error!("send WindowInc error: {:?}", e);
                            });
                        } else {
                            ncp_inner.send_buf.push_front(i);
                            break;
                        }
                    }
                }
                other_cmd => unimplemented!(),
            },
            HandleMsg::WriteData(write_data) => {
                let seg_no = ncp_inner.send_sn;
                ncp_inner.send_sn += 1;
                ncp_inner.send_queue.push_back(NcpSegment {
                    seg_no,
                    state: SegState::Init,
                    data: write_data,
                });
                ncp_inner.write_wnd -= 1;
            }
            HandleMsg::CheckResend => {
                log::debug!("checking resend: recv_next: {}", ncp_inner.recv_next);
                continue;
            }
        };

        // try to move segment from send queue to send buf
        while let seg = ncp_inner.send_queue.pop_front() {
            match seg {
                Some(mut seg) => {
                    let pending_seg_size = ncp_inner.send_queue.len() + ncp_inner.send_buf.len();
                    if pending_seg_size.le(&(1 + ncp_inner.send_wnd_limit as usize)) {
                        log::debug!("pop send queue to send buf: {}", seg.seg_no);
                        seg.state = SegState::WaitAck;
                        send_segment(&seg, &mut sock_send_hf).await?;
                        ncp_inner.send_buf.push_back(seg);
                    } else {
                        log::debug!("hit send window up bound: {}", pending_seg_size);
                        ncp_inner.send_queue.push_front(seg);
                        break;
                    }
                }
                None => {
                    break;
                }
            }
        }

        while let Some(mut seg) = ncp_inner.recv_buf.pop_front() {
            if seg.seg_no.eq(&ncp_inner.recv_next) {
                match read_tx.try_send(seg.data) {
                    Ok(()) => ncp_inner.recv_next += 1,
                    Err(TrySendError::Closed(_)) => unimplemented!(),
                    Err(TrySendError::Full(data)) => {
                        seg.data = data;
                        ncp_inner.recv_buf.push_front(seg);
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }
}

async fn send_segment(seg: &NcpSegment, write_stream: &mut SendHalf) -> Result<()> {
    let frame = NcpFrame {
        conv: 0,
        cmd: 0,
        frg: 0,
        wnd: 0,
        ts: 0,
        sn: seg.seg_no,
        una: 0,
        len: seg.data.len() as u32,
        data: seg.data.clone(),
    };
    log::debug!("before write frame");
    write_frame(write_stream, &frame).await?;
    log::debug!("after write frame");
    Ok(())
}

async fn send_ack(seg_no: u32, write_stream: &mut SendHalf, peer_addr: &SocketAddr) -> Result<()> {
    log::debug!("sending ack: {}", seg_no);
    let frame = NcpFrame {
        conv: 0,
        cmd: CMD_ACK,
        frg: 0,
        wnd: 0,
        ts: 0,
        sn: seg_no,
        una: 0,
        len: 0,
        data: Vec::new(),
    };
    write_frame_to(write_stream, &frame, peer_addr).await?;
    Ok(())
}
