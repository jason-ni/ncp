use crate::errors::Result;
use bytes::{Buf, BytesMut};
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::net::udp::SendHalf;

pub const MTU: u16 = 1400; //TODO: assuming transport layer MTU is larger than 1400
                           //pub const MSS: u16 = MTU - 24;
pub const MSS: u16 = MTU - 1398;

pub const CMD_PUSH: u8 = 0;
pub const CMD_ACK: u8 = 1;

#[derive(Debug)]
pub struct NcpFrame {
    pub conv: u32,
    pub cmd: u8,
    pub frg: u8,
    pub wnd: u16,
    pub ts: u32,
    pub sn: u32,
    pub una: u32,
    pub len: u32,
    pub data: Vec<u8>,
}

pub fn read_frame(recv_frame: &[u8]) -> NcpFrame {
    let mut frame_raw = recv_frame;
    let conv = frame_raw.get_u32();
    let cmd = frame_raw.get_u8();
    let frg = frame_raw.get_u8();
    let wnd = frame_raw.get_u16();
    let ts = frame_raw.get_u32();
    let sn = frame_raw.get_u32();
    let una = frame_raw.get_u32();
    let len = frame_raw.get_u32();
    let data = if len.gt(&0) {
        frame_raw.to_vec()
    } else {
        Vec::new()
    };
    NcpFrame {
        conv,
        cmd,
        frg,
        wnd,
        ts,
        sn,
        una,
        len,
        data,
    }
}

pub async fn write_frame(writer: &mut SendHalf, frame: &NcpFrame) -> Result<()> {
    let mut buf = BytesMut::with_capacity(1400);
    buf.extend_from_slice(&frame.conv.to_be_bytes());
    buf.extend_from_slice(&frame.cmd.to_be_bytes());
    buf.extend_from_slice(&frame.frg.to_be_bytes());
    buf.extend_from_slice(&frame.wnd.to_be_bytes());
    buf.extend_from_slice(&frame.ts.to_be_bytes());
    buf.extend_from_slice(&frame.sn.to_be_bytes());
    buf.extend_from_slice(&frame.una.to_be_bytes());
    buf.extend_from_slice(&frame.len.to_be_bytes());
    if frame.len.ne(&0) {
        buf.extend_from_slice(frame.data.as_slice());
    }
    let sent_size = writer.send(buf.as_ref()).await?;
    if sent_size.ne(&buf.len()) {
        panic!(format!("UDP mtu is smaller than {}", MTU));
    }
    Ok(())
}

pub async fn write_frame_to(
    writer: &mut SendHalf,
    frame: &NcpFrame,
    tgt: &SocketAddr,
) -> Result<()> {
    let mut buf = BytesMut::with_capacity(1400);
    buf.extend_from_slice(&frame.conv.to_be_bytes());
    buf.extend_from_slice(&frame.cmd.to_be_bytes());
    buf.extend_from_slice(&frame.frg.to_be_bytes());
    buf.extend_from_slice(&frame.wnd.to_be_bytes());
    buf.extend_from_slice(&frame.ts.to_be_bytes());
    buf.extend_from_slice(&frame.sn.to_be_bytes());
    buf.extend_from_slice(&frame.una.to_be_bytes());
    buf.extend_from_slice(&frame.len.to_be_bytes());
    if frame.len.ne(&0) {
        buf.extend_from_slice(frame.data.as_slice());
    }
    let sent_size = writer.send_to(buf.as_ref(), tgt).await?;
    if sent_size.ne(&buf.len()) {
        panic!(format!("UDP mtu is smaller than {}", MTU));
    }
    Ok(())
}
