// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Noise protocol I/O.

pub mod handshake;

use futures::Poll;
use futures::prelude::*;
use futures::io::AsyncReadExt;
use log::{debug, trace};
use snow;
use std::{fmt, io, pin::Pin};

const MAX_NOISE_PKG_LEN: usize = 65535;
const MAX_WRITE_BUF_LEN: usize = 16384;
const TOTAL_BUFFER_LEN: usize = 2 * MAX_NOISE_PKG_LEN + 3 * MAX_WRITE_BUF_LEN;

/// A single `Buffer` contains multiple non-overlapping byte buffers.
struct Buffer {
    inner: Box<[u8; TOTAL_BUFFER_LEN]>
}

/// A mutable borrow of all byte buffers, backed by `Buffer`.
struct BufferBorrow<'a> {
    read: &'a mut [u8],
    read_crypto: &'a mut [u8],
    write: &'a mut [u8],
    write_crypto: &'a mut [u8]
}

impl Buffer {
    /// Create a mutable borrow by splitting the buffer slice.
    fn borrow_mut(&mut self) -> BufferBorrow<'_> {
        let (r, w) = self.inner.split_at_mut(2 * MAX_NOISE_PKG_LEN);
        let (read, read_crypto) = r.split_at_mut(MAX_NOISE_PKG_LEN);
        let (write, write_crypto) = w.split_at_mut(MAX_WRITE_BUF_LEN);
        BufferBorrow { read, read_crypto, write, write_crypto }
    }
}

/// A noise session to a remote.
///
/// `T` is the type of the underlying I/O resource.
pub struct NoiseOutput<T> {
    io: T,
    session: snow::Session,
    buffer: Buffer,
    read_state: ReadState,
    write_state: WriteState
}

impl<T> fmt::Debug for NoiseOutput<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoiseOutput")
            .field("read_state", &self.read_state)
            .field("write_state", &self.write_state)
            .finish()
    }
}

impl<T> NoiseOutput<T> {
    fn new(io: T, session: snow::Session) -> Self {
        NoiseOutput {
            io, session,
            buffer: Buffer { inner: Box::new([0; TOTAL_BUFFER_LEN]) },
            read_state: ReadState::Init,
            write_state: WriteState::Init
        }
    }
}

/// The various states of reading a noise session transitions through.
#[derive(Debug)]
enum ReadState {
    /// initial state
    Init,
    /// read frame length
    ReadLen { buf: [u8; 2], off: usize },
    /// read encrypted frame data
    ReadData { len: usize, off: usize },
    /// copy decrypted frame data
    CopyData { len: usize, off: usize },
    /// end of file has been reached (terminal state)
    /// The associated result signals if the EOF was unexpected or not.
    Eof(Result<(), ()>),
    /// decryption error (terminal state)
    DecErr
}

/// The various states of writing a noise session transitions through.
#[derive(Debug)]
enum WriteState {
    /// initial state
    Init,
    /// accumulate write data
    BufferData { off: usize },
    /// write frame length
    WriteLen { len: usize, buf: [u8; 2], off: usize },
    /// write out encrypted data
    WriteData { len: usize, off: usize },
    /// end of file has been reached (terminal state)
    Eof,
    /// encryption error (terminal state)
    EncErr
}

impl<T: AsyncRead + Unpin> AsyncRead for NoiseOutput<T> {
    fn poll_read(mut self: core::pin::Pin<&mut Self>, cx: &mut futures::task::Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, futures::io::Error>> {
        // We use a `this` because the compiler isn't smart enough to allow
        // mutably borrowing multiple different fields from the `Pin` at the
        // same time.
        let mut this = &mut *self;

        let buffer = this.buffer.borrow_mut();

        loop {
            trace!("read state: {:?}", this.read_state);
            match this.read_state {
                ReadState::Init => {
                    this.read_state = ReadState::ReadLen { buf: [0, 0], off: 0 };
                }
                ReadState::ReadLen { mut buf, mut off } => {
                    let n = match read_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(Some(n))) => n,
                        Poll::Ready(Ok(None)) => {
                            trace!("read: eof");
                            this.read_state = ReadState::Eof(Ok(()));
                            return Poll::Ready(Ok(0))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            // TODO: Is this needed?
                            this.read_state = ReadState::ReadLen { buf, off };

                            return Poll::Pending;
                        }
                    };
                    trace!("read: next frame len = {}", n);
                    if n == 0 {
                        trace!("read: empty frame");
                        this.read_state = ReadState::Init;
                        continue
                    }
                    this.read_state = ReadState::ReadData { len: usize::from(n), off: 0 }
                }
                ReadState::ReadData { len, ref mut off } => {
                    let n = match AsyncRead::poll_read(
                        Pin::new(&mut this.io),
                        cx,
                        &mut buffer.read[*off ..len]
                    ) {
                        Poll::Ready(Ok(n)) => n,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => return Poll::Pending,
                    };

                    trace!("read: read {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("read: eof");
                        this.read_state = ReadState::Eof(Err(()));
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                    }

                    *off += n;
                    if len == *off {
                        trace!("read: decrypting {} bytes", len);
                        if let Ok(n) = this.session.read_message(&buffer.read[.. len], buffer.read_crypto) {
                            trace!("read: payload len = {} bytes", n);
                            this.read_state = ReadState::CopyData { len: n, off: 0 }
                        } else {
                            debug!("decryption error");
                            this.read_state = ReadState::DecErr;
                            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                        }
                    }
                }
                ReadState::CopyData { len, ref mut off } => {
                    let n = std::cmp::min(len - *off, buf.len());
                    buf[.. n].copy_from_slice(&buffer.read_crypto[*off .. *off + n]);
                    trace!("read: copied {}/{} bytes", *off + n, len);
                    *off += n;
                    if len == *off {
                        this.read_state = ReadState::ReadLen { buf: [0, 0], off: 0 };
                    }
                    return Poll::Ready(Ok(n))
                }
                ReadState::Eof(Ok(())) => {
                    trace!("read: eof");
                    return Poll::Ready(Ok(0))
                }
                ReadState::Eof(Err(())) => {
                    trace!("read: eof (unexpected)");
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                }
                ReadState::DecErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for NoiseOutput<T> {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> futures::task::Poll<std::result::Result<usize, std::io::Error>>{
        // We use a `this` because the compiler isn't smart enough to allow
        // mutably borrowing multiple different fields from the `Pin` at the
        // same time.
        let mut this = &mut *self;

        let buffer = this.buffer.borrow_mut();

        loop {
            trace!("write state: {:?}", this.write_state);
            match this.write_state {
                WriteState::Init => {
                    this.write_state = WriteState::BufferData { off: 0 }
                }
                WriteState::BufferData { ref mut off } => {
                    let n = std::cmp::min(MAX_WRITE_BUF_LEN - *off, buf.len());
                    buffer.write[*off .. *off + n].copy_from_slice(&buf[.. n]);
                    trace!("write: buffered {} bytes", *off + n);
                    *off += n;
                    if *off == MAX_WRITE_BUF_LEN {
                        trace!("write: encrypting {} bytes", *off);
                        if let Ok(n) = this.session.write_message(buffer.write, buffer.write_crypto) {
                            trace!("write: cipher text len = {} bytes", n);
                            this.write_state = WriteState::WriteLen {
                                len: n,
                                buf: u16::to_be_bytes(n as u16),
                                off: 0
                            }
                        } else {
                            debug!("encryption error");
                            this.write_state = WriteState::EncErr;
                            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                        }
                    }
                    return Poll::Ready(Ok(n))
                }
                WriteState::WriteLen { len, mut buf, mut off } => {
                    trace!("write: writing len ({}, {:?}, {}/2)", len, buf, off);
                    match write_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(true)) => (),
                        Poll::Ready(Ok(false)) => {
                            trace!("write: eof");
                            this.write_state = WriteState::Eof;
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            // TODO: Is this needed?
                            this.write_state = WriteState::WriteLen{ len, buf, off };

                            return Poll::Pending
                        }
                    }
                    this.write_state = WriteState::WriteData { len, off: 0 }
                }
                WriteState::WriteData { len, ref mut off } => {
                    let n = match Pin::new(&mut this.io).poll_write( cx, &buffer.write_crypto[*off .. len]) {
                        Poll::Ready(Ok(n)) => n,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        // TODO: Do we need to persist the state like we do in
                        // in Poll::Pending on `WriteState::WriteLen`?
                        Poll::Pending => return Poll::Pending,
                    };
                    trace!("write: wrote {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("write: eof");
                        this.write_state = WriteState::Eof;
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                    }
                    *off += n;
                    if len == *off {
                        trace!("write: finished writing {} bytes", len);
                        this.write_state = WriteState::Init
                    }
                }
                WriteState::Eof => {
                    trace!("write: eof");
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                }
                WriteState::EncErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> futures::task::Poll<std::result::Result<(), std::io::Error>> {
        // We use a `this` because the compiler isn't smart enough to allow
        // mutably borrowing multiple different fields from the `Pin` at the
        // same time.
        let mut this = &mut *self;

        let buffer = this.buffer.borrow_mut();

        loop {
            match this.write_state {
                WriteState::Init => return Poll::Ready(Ok(())),
                WriteState::BufferData { off } => {
                    trace!("flush: encrypting {} bytes", off);
                    if let Ok(n) = this.session.write_message(&buffer.write[.. off], buffer.write_crypto) {
                        trace!("flush: cipher text len = {} bytes", n);
                        this.write_state = WriteState::WriteLen {
                            len: n,
                            buf: u16::to_be_bytes(n as u16),
                            off: 0
                        }
                    } else {
                        debug!("encryption error");
                        this.write_state = WriteState::EncErr;
                        return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                    }
                }
                // TODO: `WriteLen` and `WriteData` duplicate lots of logic from
                // their respective `poll_write` state transition functions.
                // Should we deduplicate this?
                WriteState::WriteLen { len, mut buf, mut off } => {
                    trace!("flush: writing len ({}, {:?}, {}/2)", len, buf, off);
                    match write_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(true)) => (),
                        Poll::Ready(Ok(false)) => {
                            trace!("write: eof");
                            this.write_state = WriteState::Eof;
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            // Preserve write state
                            // TODO: Do we need to persist the state here? We
                            // pass these by reference to any other function,
                            // right?
                            this.write_state = WriteState::WriteLen { len, buf, off };

                            return Poll::Pending
                        }
                    }
                    this.write_state = WriteState::WriteData { len, off: 0 }
                }
                WriteState::WriteData { len, ref mut off } => {
                    let n = match Pin::new(&mut this.io).poll_write(cx, &buffer.write_crypto[*off .. len]) {
                        Poll::Ready(Ok(n)) => n,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        // TODO: Do we need to persist the state like we do in
                        // in Poll::Pending on `WriteState::WriteLen`?
                        Poll::Pending => return Poll::Pending,
                    };
                    trace!("flush: wrote {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("flush: eof");
                        this.write_state = WriteState::Eof;
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                    }
                    *off += n;
                    if len == *off {
                        trace!("flush: finished writing {} bytes", len);
                        this.write_state = WriteState::Init;
                        return Poll::Ready(Ok(()))
                    }
                }
                WriteState::Eof => {
                    trace!("flush: eof");
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                }
                WriteState::EncErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_close(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> futures::task::Poll<std::result::Result<(), std::io::Error>>{
        Pin::new(&mut self.io).poll_close(cx)

    }

}

/// Read 2 bytes as frame length from the given source into the given buffer.
///
// TODO: This is not the case, right?
/// Panics if `off >= 2`.
///
/// When [`io::ErrorKind::WouldBlock`] is returned, the given buffer and offset
/// may have been updated (i.e. a byte may have been read) and must be preserved
/// for the next invocation.
///
/// Returns `None` if EOF has been encountered.
fn read_frame_len<R: AsyncRead + Unpin>(mut io: &mut R, cx: &mut futures::task::Context<'_>, buf: &mut [u8; 2], off: &mut usize)
    -> Poll<Result<Option<u16>, futures::io::Error>>
{
    loop {
        match AsyncRead::poll_read(Pin::new(&mut io), cx, &mut buf[*off ..]) {
            Poll::Ready(Ok(n)) => {
                // TODO: Why does a reader signal eof via returning "0 written
                // bytes" instead of just throwing an eof error?
                if n == 0 {
                    return Poll::Ready(Ok(None));
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(Some(u16::from_be_bytes(*buf))));
                }
            },
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            },
            Poll::Pending => {
                return Poll::Pending;
            }
        }
    }
}

/// Write 2 bytes as frame length from the given buffer into the given sink.
///
/// Panics if `off >= 2`.
///
/// When [`io::ErrorKind::WouldBlock`] is returned, the given offset
/// may have been updated (i.e. a byte may have been written) and must
/// be preserved for the next invocation.
///
/// Returns `false` if EOF has been encountered.
fn write_frame_len<W: AsyncWrite + Unpin>(mut io: &mut W, cx: &mut futures::task::Context<'_>, buf: &[u8; 2], off: &mut usize)
    -> futures::task::Poll<std::result::Result<bool, futures::io::Error>>
{
    loop {
        match Pin::new(&mut io).poll_write(cx, &buf[*off ..]) {
            Poll::Ready(Ok(n)) => {
                if n == 0 {
                    return Poll::Ready(Ok(false))
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(true))
                }
            }
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }
            Poll::Pending => {
                return Poll::Pending;
            }
        }
    }
}

