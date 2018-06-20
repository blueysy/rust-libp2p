// Copyright 2018 Parity Technologies (UK) Ltd.
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

extern crate bytes;
#[macro_use]
extern crate futures;
extern crate libp2p_core as core;
#[macro_use]
extern crate log;
extern crate parking_lot;
extern crate tokio_io;
extern crate varint;

mod codec;

use std::{cmp, iter};
use std::io::{Read, Write, Error as IoError, ErrorKind as IoErrorKind};
use std::sync::Arc;
use bytes::Bytes;
use core::{ConnectionUpgrade, Endpoint, StreamMuxer};
use parking_lot::Mutex;
use futures::prelude::*;
use futures::{future, task};
use tokio_io::{AsyncRead, AsyncWrite, codec::Framed};

#[derive(Debug, Clone)]
pub struct MultiplexConfig;

impl MultiplexConfig {
    #[inline]
    pub fn new() -> MultiplexConfig {
        MultiplexConfig
    }
}

impl<C, Maf> ConnectionUpgrade<C, Maf> for MultiplexConfig
where
    C: AsyncRead + AsyncWrite,
{
    type Output = Multiplex<C>;
    type MultiaddrFuture = Maf;
    type Future = future::FutureResult<(Self::Output, Self::MultiaddrFuture), IoError>;
    type UpgradeIdentifier = ();
    type NamesIter = iter::Once<(Bytes, ())>;

    #[inline]
    fn upgrade(self, i: C, _: (), endpoint: Endpoint, remote_addr: Maf) -> Self::Future {
        let out = Multiplex {
            inner: Arc::new(Mutex::new(MultiplexInner {
                inner: i.framed(codec::Codec::new(endpoint)),
                buffer: Vec::with_capacity(32),
                next_outbound_stream_id: if endpoint == Endpoint::Dialer { 0 } else { 1 },
                to_notify: Vec::new(),
            }))
        };

        future::ok((out, remote_addr))
    }

    #[inline]
    fn protocol_names(&self) -> Self::NamesIter {
        iter::once((Bytes::from("/mplex/6.7.0"), ()))
    }
}

pub struct Multiplex<C> {
    inner: Arc<Mutex<MultiplexInner<C>>>,
}

impl<C> Clone for Multiplex<C> {
    #[inline]
    fn clone(&self) -> Self {
        Multiplex {
            inner: self.inner.clone(),
        }
    }
}

struct MultiplexInner<C> {
    inner: Framed<C, codec::Codec>,
    buffer: Vec<codec::Elem>,
    next_outbound_stream_id: u32,
    to_notify: Vec<task::Task>,
}

// Processes elements in `inner` until one matching `filter` is found.
fn next_match<C, F, O>(inner: &mut MultiplexInner<C>, mut filter: F) -> Poll<Option<O>, IoError>
where C: AsyncRead + AsyncWrite,
      F: FnMut(&codec::Elem) -> Option<O>,
{
    if let Some((offset, out)) = inner.buffer.iter().enumerate().filter_map(|(n, v)| filter(v).map(|v| (n, v))).next() {
        inner.buffer.remove(offset);
        return Ok(Async::Ready(Some(out)));
    }

    loop {
        let elem = match inner.inner.poll() {
            Ok(Async::Ready(item)) => item,
            Ok(Async::NotReady) => {
                inner.to_notify.push(task::current());
                return Ok(Async::NotReady);
            },
            Err(err) => {
                return Err(err);
            },
        };

        if let Some(elem) = elem {
            if let Some(out) = filter(&elem) {
                return Ok(Async::Ready(Some(out)));
            } else {
                inner.buffer.push(elem);
                for task in inner.to_notify.drain(..) {
                    task.notify();
                }
            }
        } else {
            return Ok(Async::Ready(None));
        }
    }
}

fn poll_send<C>(inner: &mut MultiplexInner<C>, elem: codec::Elem) -> Poll<(), IoError>
where C: AsyncRead + AsyncWrite
{
    match inner.inner.start_send(elem) {
        Ok(AsyncSink::Ready) => {
            Ok(Async::Ready(()))
        },
        Ok(AsyncSink::NotReady(_)) => {
            Ok(Async::NotReady)
        },
        Err(err) => Err(err)
    }
}

impl<C> StreamMuxer for Multiplex<C>
where C: AsyncRead + AsyncWrite
{
    type Substream = Substream<C>;
    type InboundSubstream = InboundSubstream<C>;
    type OutboundSubstream = OutboundSubstream<C>;

    #[inline]
    fn inbound(self) -> Self::InboundSubstream {
        InboundSubstream { inner: self.inner }
    }

    #[inline]
    fn outbound(self) -> Self::OutboundSubstream {
        let substream_id = {
            let mut inner = self.inner.lock();
            let n = inner.next_outbound_stream_id;
            inner.next_outbound_stream_id += 2;
            n
        };

        OutboundSubstream { inner: self.inner, substream_id }
    }
}

pub struct InboundSubstream<C> {
    inner: Arc<Mutex<MultiplexInner<C>>>,
}

impl<C> Future for InboundSubstream<C>
where C: AsyncRead + AsyncWrite
{
    type Item = Option<Substream<C>>;
    type Error = IoError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut inner = self.inner.lock();
        let num = try_ready!(next_match(&mut inner, |elem| {
            match elem {
                codec::Elem::Open { substream_id } => Some(*substream_id),       // TODO: check even/uneven?
                _ => None,
            }
        }));

        if let Some(num) = num {
            Ok(Async::Ready(Some(Substream {
                inner: self.inner.clone(),
                current_data: Bytes::new(),
                num,
            })))
        } else {
            Ok(Async::Ready(None))
        }
    }
}

pub struct OutboundSubstream<C> {
    inner: Arc<Mutex<MultiplexInner<C>>>,
    substream_id: u32,
}

impl<C> Future for OutboundSubstream<C>
where C: AsyncRead + AsyncWrite
{
    type Item = Option<Substream<C>>;
    type Error = IoError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let elem = codec::Elem::Open { substream_id: self.substream_id };
        let mut inner = self.inner.lock();
        try_ready!(poll_send(&mut inner, elem));
        let _ = inner.inner.poll_complete();        // TODO: block on this?
        Ok(Async::Ready(Some(Substream {
            inner: self.inner.clone(),
            num: self.substream_id,
            current_data: Bytes::new(),
        })))
    }
}

pub struct Substream<C>
where C: AsyncRead + AsyncWrite
{
    inner: Arc<Mutex<MultiplexInner<C>>>,
    num: u32,
    // Read buffer. Contains data read from `inner` but not yet dispatched by a call to `read()`.
    current_data: Bytes,
}

impl<C> Read for Substream<C>
where C: AsyncRead + AsyncWrite
{
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        loop {
            if self.current_data.len() != 0 {
                let len = cmp::min(self.current_data.len(), buf.len());
                buf[..len].copy_from_slice(&self.current_data.split_to(len));
                return Ok(len);
            }

            let mut inner = self.inner.lock();

            let next_data_poll = next_match(&mut inner, |elem| {
                match elem {
                    &codec::Elem::Data { ref substream_id, ref data } if *substream_id == self.num => {
                        Some(data.clone())
                    },
                    _ => None,
                }
            });

            match next_data_poll {
                Ok(Async::Ready(Some(data))) => self.current_data = data.freeze(),
                Ok(Async::Ready(None)) => return Ok(0),
                Ok(Async::NotReady) => return Err(IoErrorKind::WouldBlock.into()),
                Err(err) => return Err(err),
            }
        }
    }
}

impl<C> AsyncRead for Substream<C>
where C: AsyncRead + AsyncWrite
{
}

impl<C> Write for Substream<C>
where C: AsyncRead + AsyncWrite
{
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        let elem = codec::Elem::Data {
            substream_id: self.num,
            data: From::from(buf),
        };

        let mut inner = self.inner.lock();
        match poll_send(&mut inner, elem) {
            Ok(Async::Ready(())) => {
                let _ = inner.inner.poll_complete();
                Ok(buf.len())
            },
            Ok(Async::NotReady) => Err(IoErrorKind::WouldBlock.into()),
            Err(err) => Err(err),
        }
    }

    fn flush(&mut self) -> Result<(), IoError> {
        let mut inner = self.inner.lock();
        match inner.inner.poll_complete() {
            Ok(Async::Ready(())) => Ok(()),
            Ok(Async::NotReady) => Err(IoErrorKind::WouldBlock.into()),
            Err(err) => Err(err),
        }
    }
}

impl<C> AsyncWrite for Substream<C>
where C: AsyncRead + AsyncWrite
{
    fn shutdown(&mut self) -> Poll<(), IoError> {
        let elem = codec::Elem::Close {
            substream_id: self.num,
        };

        let mut inner = self.inner.lock();
        poll_send(&mut inner, elem)
    }
}
