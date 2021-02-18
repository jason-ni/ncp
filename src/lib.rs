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
use crate::stream::{ReadNoty, WriteCmd, WriterNoty};
pub use stream::{NcpStreamReader, NcpStreamWriter};
use tokio::net::udp::SendHalf;
use tokio::sync::mpsc::error::TrySendError;

const MAX_INTERVAL: u32 = 200;
const MIN_INTERVAL: u32 = 10;

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
    now: u32,
    closing: bool,
    closing_sn: u32,
    rto: u32,
    srtt: u32,
    rttval: u32,
}

#[derive(Debug)]
enum SegState {
    Init,
    WaitAck,
    Sent,
}

#[derive(Debug)]
pub struct NcpSegment {
    seg_no: u32,
    state: SegState,
    timestamp: u32,
    rexmit_timestamp: u32,
    data: Option<Vec<u8>>,
}

enum HandleMsg {
    RecvData(NcpFrame, SocketAddr),
    WriteData(Vec<u8>),
    CheckResend,
    Close,
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
            write_wnd: 64,
            write_tx,
            write_noty,
            closing: false,
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
            write_wnd: 64,
            write_tx,
            write_noty,
            closing: false,
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
    mut write_rx: mpsc::UnboundedReceiver<WriteCmd>,
    mut read_tx: mpsc::Sender<ReadNoty>,
    mut write_noty_tx: mpsc::UnboundedSender<WriterNoty>,
) -> Result<()> {
    let mut ncp_inner = NcpStreamInner {
        write_wnd: 64,
        send_wnd_limit: 64,
        send_sn: 0,
        send_una: 0,
        send_next: 0,
        send_queue: Default::default(),
        send_buf: Default::default(),
        recv_next: 0,
        recv_buf: Default::default(),
        now: 0,
        closing: false,
        closing_sn: 0,
        rto: 200,
        srtt: 0,
        rttval: 0,
    };
    let (mut sock_recv_hf, mut sock_send_hf) = udp_sock.split();
    let mut buf = BytesMut::with_capacity(1400);
    let mut check_arp_inverval = tokio::time::interval(std::time::Duration::from_millis(200));
    let wait = tokio::time::delay_for(std::time::Duration::from_millis(100));
    loop {
        log::trace!("loop begin");
        buf.resize(1400, 0u8);
        let msg = tokio::select! {
            res = sock_recv_hf.recv_from(buf.as_mut()) => {
                match res {
                    Ok((size, peer_addr)) => {
                        log::debug!("received data: {}", pretty_hex::pretty_hex(&buf[..size].to_vec()));
                        let frame = read_frame(&buf[..size]);
                        //log::debug!("received frame: {:?}", frame);
                        /*
                        log::debug!("whether to drop: {} - {}", ncp_inner.now, (ncp_inner.now % 50));
                        fastrand::seed(now_millis() as u64);
                        let threshold = fastrand::u32(..20);
                        if threshold.eq(&0) {
                            log::debug!("--- error injection: drop segment {} intentionally. threshold: {}", frame.sn, threshold);
                            continue
                        }
                         */
                        HandleMsg::RecvData(frame, peer_addr)
                    }
                    Err(e) => {
                        return Err(NcpError::StdIoError(e))
                    }
                }
            },
            res = write_rx.recv() => {
                match res {
                    Some(write_cmd) => {
                        log::debug!("writing segment data: {:?}",write_cmd);
                        match write_cmd {
                            WriteCmd::Data(d) => {
                                HandleMsg::WriteData(d)
                            }
                            WriteCmd::Close => {
                                HandleMsg::Close
                            }
                        }
                    }
                    None => {
                        log::debug!("writer dropped, closing loop...");
                        return Ok(())
                    }
                }
            }
            _ = check_arp_inverval.tick() => {
                HandleMsg::CheckResend
            }
        };

        ncp_inner.now = now_millis();

        match msg {
            HandleMsg::RecvData(frame, peer_addr) => match frame.cmd {
                CMD_PUSH => {
                    log::debug!("received push data: {}", &frame.sn);
                    log::debug!("recv_buf: {:?}", ncp_inner.recv_buf);
                    if frame.sn.lt(&ncp_inner.recv_next) {
                        send_ack(frame.sn, ncp_inner.recv_next, &mut sock_send_hf, &peer_addr)
                            .await?;
                        continue;
                    }
                    let mut idx = 0;
                    let mut insert = true;
                    for i in ncp_inner.recv_buf.iter() {
                        if frame.sn.eq(&i.seg_no) {
                            insert = false;
                            send_ack(frame.sn, ncp_inner.recv_next, &mut sock_send_hf, &peer_addr)
                                .await?;
                            break;
                        } else {
                            if frame.sn.lt(&i.seg_no) {
                                break;
                            } else {
                                idx += 1;
                            }
                        }
                    }
                    if insert {
                        ncp_inner.recv_buf.insert(
                            idx,
                            NcpSegment {
                                seg_no: frame.sn,
                                state: SegState::Init,
                                timestamp: frame.ts,
                                rexmit_timestamp: 0,
                                data: if frame.data.len().eq(&0) {
                                    None
                                } else {
                                    Some(frame.data)
                                },
                            },
                        );
                        log::debug!("inserted seg to recv_buf: {:?}", ncp_inner.recv_buf);
                        send_ack(frame.sn, ncp_inner.recv_next, &mut sock_send_hf, &peer_addr)
                            .await?;
                    }
                }
                CMD_ACK => {
                    log::debug!("send_buf: {:?}", ncp_inner.send_buf);
                    log::debug!("send_queue: {:?}", ncp_inner.send_queue);
                    let mut seg_rtt = 0;
                    for i in ncp_inner.send_buf.iter_mut() {
                        log::debug!(
                            "checking ack seg_no: {}, frame.sn: {}, frame.len: {}",
                            i.seg_no,
                            frame.sn,
                            frame.len
                        );
                        if i.seg_no.eq(&frame.sn) || i.seg_no.lt(&frame.una) {
                            log::debug!("=== setting segment {} to sent state", i.seg_no);
                            i.state = SegState::Sent;
                            seg_rtt = ncp_inner.now - i.timestamp;
                        }
                    }
                    if seg_rtt.gt(&0) {
                        update_rtt(&mut ncp_inner, seg_rtt);
                        log::debug!(
                            "--- segment {} rtt: {}, rto: {}",
                            frame.sn,
                            seg_rtt,
                            ncp_inner.rto
                        );
                    }
                    let mut pop_count = 0;
                    for i in ncp_inner.send_buf.iter_mut() {
                        match i.state {
                            SegState::Sent => {
                                if i.seg_no.eq(&ncp_inner.send_una) {
                                    ncp_inner.send_una = i.seg_no + 1;
                                    ncp_inner.write_wnd += 1;
                                    pop_count += 1;
                                    match i.data {
                                        Some(ref _d) => {
                                            log::debug!("=== send increasing write wnd noty");
                                            let _ = write_noty_tx
                                                .send(WriterNoty::WindowInc(1))
                                                .map_err(|e| {
                                                    log::error!("send WindowInc error: {:?}", e);
                                                });
                                        }
                                        None => {
                                            log::debug!("=== send write close noty");
                                            let _ = write_noty_tx.send(WriterNoty::Closed).map_err(
                                                |e| {
                                                    log::error!("send WindowInc error: {:?}", e);
                                                },
                                            );
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                    while pop_count > 0 {
                        ncp_inner.send_buf.pop_front();
                        pop_count -= 1;
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
                    timestamp: ncp_inner.now,
                    rexmit_timestamp: ncp_inner.now + ncp_inner.rto + (ncp_inner.rto >> 3),
                    data: Some(write_data),
                });
                ncp_inner.write_wnd -= 1;
            }
            HandleMsg::CheckResend => {
                log::trace!("checking resend: recv_next: {}", ncp_inner.recv_next);
                for i in ncp_inner.send_buf.iter_mut() {
                    let diff = i32diff(ncp_inner.now, i.rexmit_timestamp);
                    if diff.ge(&0) {
                        send_segment(i, &mut sock_send_hf).await?;
                    }
                }
            }
            HandleMsg::Close => {
                log::debug!("closing stream");
                let seg_no = ncp_inner.send_sn;
                ncp_inner.send_sn += 1;
                ncp_inner.send_queue.push_back(NcpSegment {
                    seg_no,
                    state: SegState::Init,
                    timestamp: ncp_inner.now,
                    rexmit_timestamp: ncp_inner.now + ncp_inner.rto + (ncp_inner.rto >> 3),
                    data: None,
                });
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
                        seg.timestamp = ncp_inner.now;
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

        let mut recv_cnt = 0;
        for seg in ncp_inner.recv_buf.iter_mut() {
            log::debug!("checking recv seg: {:?}", seg);
            if seg.seg_no.eq(&ncp_inner.recv_next) {
                let mut closing = false;
                let read_noty = match seg.data {
                    Some(ref _data) => {
                        let data = seg.data.take().unwrap();
                        ReadNoty::Data(data)
                    }
                    None => {
                        log::debug!("set closing...");
                        closing = true;
                        ReadNoty::Eof
                    }
                };
                match read_tx.try_send(read_noty) {
                    Ok(()) => {
                        ncp_inner.recv_next += 1;
                        recv_cnt += 1;
                        log::debug!(
                            "advanced recv_next: {}, sent to reader",
                            ncp_inner.recv_next
                        );
                        if closing {
                            let _ = write_noty_tx.send(WriterNoty::Closed).map_err(|e| {
                                log::error!("failed to send WriterNoty::Closed");
                            });
                            //TODO: maybe we need delay exiting loop
                            log::debug!("exiting loop as closed");
                            return Ok(());
                        }
                    }
                    Err(TrySendError::Closed(_)) => unimplemented!(),
                    Err(TrySendError::Full(noty)) => {
                        match noty {
                            ReadNoty::Data(d) => {
                                seg.data.replace(d);
                            }
                            ReadNoty::Eof => (),
                        }
                        break;
                    }
                }
            } else {
                break;
            }
        }
        while recv_cnt.gt(&0) {
            ncp_inner.recv_buf.pop_front();
            recv_cnt -= 1;
        }
    }
}

fn update_rtt(ncp_inner: &mut NcpStreamInner, rtt: u32) {
    if ncp_inner.srtt == 0 {
        ncp_inner.srtt = rtt;
        ncp_inner.rttval = rtt / 2;
    } else {
        let delta = if rtt > ncp_inner.srtt {
            rtt - ncp_inner.srtt
        } else {
            ncp_inner.srtt - rtt
        };
        ncp_inner.rttval = (3 * ncp_inner.rttval + delta) / 4;
        ncp_inner.srtt = (7 * ncp_inner.srtt + rtt) / 8;
        if ncp_inner.srtt < 1 {
            ncp_inner.srtt = 1;
        }
    }
    let rto = ncp_inner.srtt + std::cmp::max(MAX_INTERVAL, 4 * ncp_inner.rttval);
    ncp_inner.rto = std::cmp::min(std::cmp::max(20, rto), 30000);
}

async fn send_segment(seg: &NcpSegment, write_stream: &mut SendHalf) -> Result<()> {
    let (len, data) = match &seg.data {
        Some(d) => (d.len(), d.clone()),
        None => (0, Vec::new()),
    };
    let frame = NcpFrame {
        conv: 0,
        cmd: 0,
        frg: 0,
        wnd: 0,
        ts: seg.timestamp,
        sn: seg.seg_no,
        una: 0,
        len: len as u32,
        data,
    };
    log::debug!("before write frame");
    write_frame(write_stream, &frame).await?;
    log::debug!("after write frame");
    Ok(())
}

async fn send_ack(
    seg_no: u32,
    recv_next: u32,
    write_stream: &mut SendHalf,
    peer_addr: &SocketAddr,
) -> Result<()> {
    log::debug!("sending ack: {}", seg_no);
    let frame = NcpFrame {
        conv: 0,
        cmd: CMD_ACK,
        frg: 0,
        wnd: 0,
        ts: 0,
        sn: seg_no,
        una: recv_next,
        len: 0,
        data: Vec::new(),
    };
    write_frame_to(write_stream, &frame, peer_addr).await?;
    Ok(())
}

#[inline(always)]
fn now_millis() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32
}

#[inline(always)]
fn i32diff(a: u32, b: u32) -> i32 {
    a as i32 - b as i32
}
