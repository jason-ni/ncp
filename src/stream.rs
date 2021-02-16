use bytes::{Buf, BufMut};
use core::pin::Pin;
use core::task::Poll;
use std::cmp::min;
use std::mem::MaybeUninit;
use std::task::Context;
use tokio::io::{AsyncRead, AsyncWrite, Error, ErrorKind};
use tokio::stream::Stream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

pub struct NcpStreamWriter {
    pub write_wnd: i16,
    pub write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub write_noty: mpsc::UnboundedReceiver<WriterNoty>,
}

#[derive(Debug)]
pub enum WriterNoty {
    WindowInc(i16),
}

pub struct NcpStreamReader {
    pub pending_buf: Option<Vec<u8>>,
    pub last_read: usize,
    pub read_rx: mpsc::Receiver<Vec<u8>>,
}

impl AsyncRead for NcpStreamReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        let me = &mut *self;
        if let Some(ref pending_buf) = me.pending_buf {
            let sendable = (pending_buf.len() - me.last_read).min(buf.len());
            buf.put_slice(&pending_buf[me.last_read..(me.last_read + sendable)]);
            me.last_read += sendable;
            Poll::Ready(Ok(sendable))
        } else {
            match me.read_rx.poll_recv(cx) {
                Poll::Ready(Some(s)) => {
                    if s.len().le(&buf.len()) {
                        buf.put_slice(s.as_slice());
                        Poll::Ready(Ok(s.len()))
                    } else {
                        me.last_read = buf.len();
                        buf.put_slice(&s.as_slice()[..me.last_read]);
                        me.pending_buf.replace(s);
                        Poll::Ready(Ok(me.last_read))
                    }
                }
                Poll::Ready(None) => Poll::Ready(Ok(0)),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for NcpStreamWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let me = &mut *self;
        log::debug!("===== write_wnd: {}", me.write_wnd);
        if me.write_wnd.le(&0) {
            loop {
                log::debug!("=== try read noty");
                match me.write_noty.try_recv() {
                    Ok(n) => match n {
                        WriterNoty::WindowInc(inc) => me.write_wnd += inc,
                    },
                    Err(TryRecvError::Empty) => {
                        log::debug!("=== read noty empty");
                        if me.write_wnd.gt(&0) {
                            log::debug!("=== already increased: {}", me.write_wnd);
                            break;
                        } else {
                            match me.write_noty.poll_recv(cx) {
                                Poll::Ready(Some(n)) => match n {
                                    WriterNoty::WindowInc(inc) => {
                                        me.write_wnd += inc;
                                        break;
                                    }
                                },
                                Poll::Ready(None) => {
                                    return Poll::Ready(Err(Error::from(ErrorKind::UnexpectedEof)));
                                }
                                Poll::Pending => {
                                    log::debug!("=== read again noty pending");
                                    return Poll::Pending;
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Closed) => {
                        return Poll::Ready(Err(Error::from(ErrorKind::UnexpectedEof)));
                    }
                }
            }
        }
        let mut cursor = buf;
        let mut sent: usize = 0;
        let mut last_seg: Option<Vec<u8>> = None;
        while cursor.has_remaining() {
            let seg_data = match last_seg {
                Some(_) => last_seg.take().unwrap(),
                None => {
                    let seg_len = min(cursor.remaining(), crate::frame::MSS as usize);
                    let seg_data = cursor[..seg_len].to_vec();
                    cursor.advance(seg_len);
                    seg_data
                }
            };
            let seg_len = seg_data.len();
            match me.write_tx.send(seg_data) {
                Ok(()) => {
                    sent += seg_len;
                    me.write_wnd -= 1;
                    if me.write_wnd.eq(&0) {
                        return Poll::Ready(Ok(sent));
                    }
                }
                Err(_) => {
                    return Poll::Ready(Err(Error::from(ErrorKind::UnexpectedEof)));
                }
            };
        }
        Poll::Ready(Ok(sent))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        unimplemented!()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        unimplemented!()
    }
}
