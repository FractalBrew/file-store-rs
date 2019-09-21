// Copyright 2019 Dave Townsend
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A set of useful utilities for converting between the different asynchronous
//! types that this crate uses.
use std::io;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::buf::FromBuf;
use bytes::{BytesMut, IntoBuf};
use futures::future::poll_fn;
use futures::stream::{Stream, StreamExt};
use tokio_io::{AsyncRead, BufReader};
use tokio_sync::semaphore::{Permit, Semaphore};

use crate::types::{Data, StorageError};

/// Converts an AsyncRead into a stream that emits [`Data`](../type.Data.html).
pub struct ReaderStream<R>
where
    R: AsyncRead,
{
    reader: Pin<Box<R>>,
    buffer: BytesMut,
    initial_buffer_size: usize,
    minimum_buffer_size: usize,
}

impl<R> ReaderStream<R>
where
    R: AsyncRead,
{
    /// Creates a stream that emits [`Data`](../type.Data.html) from an `AsynRead`.
    ///
    /// Passed a reader this will generate a stream that emits buffers of data
    /// asynchronously. The stream will attempt to read a buffer's worth of data
    /// from the reader. Initially it will use a buffer of `initial_buffer_size`
    /// size. As data is read the read buffer decreases in size until it reaches
    /// `minimum_buffer_size` at which point a new buffer of
    /// `initial_buffer_size` is used.
    pub fn stream<T>(
        reader: T,
        initial_buffer_size: usize,
        minimum_buffer_size: usize,
    ) -> impl Stream<Item = io::Result<Data>>
    where
        T: AsyncRead + Send + 'static,
    {
        let buf_reader = BufReader::new(reader);

        let mut buffer = BytesMut::with_capacity(initial_buffer_size);
        unsafe {
            buffer.set_len(initial_buffer_size);
            buf_reader.prepare_uninitialized_buffer(&mut buffer);
        }

        ReaderStream {
            reader: Box::pin(buf_reader),
            buffer,
            initial_buffer_size,
            minimum_buffer_size,
        }
    }

    fn inner_poll(&mut self, cx: &mut Context) -> Poll<Option<io::Result<Data>>> {
        match self.reader.as_mut().poll_read(cx, &mut self.buffer) {
            Poll::Ready(Ok(0)) => Poll::Ready(None),
            Poll::Ready(Ok(size)) => {
                let data = self.buffer.split_to(size);

                if self.buffer.len() < self.minimum_buffer_size {
                    self.buffer = BytesMut::with_capacity(self.initial_buffer_size);
                    unsafe {
                        self.buffer.set_len(self.initial_buffer_size);
                        self.reader.prepare_uninitialized_buffer(&mut self.buffer);
                    }
                }

                Poll::Ready(Some(Ok(data.freeze())))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
        }
    }
}

impl<R> Stream for ReaderStream<R>
where
    R: AsyncRead,
{
    type Item = io::Result<Data>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<io::Result<Data>>> {
        self.inner_poll(cx)
    }
}

pub(crate) fn into_data_stream<S, I, E>(stream: S) -> impl Stream<Item = Result<Data, StorageError>>
where
    S: Stream<Item = Result<I, E>> + Send + 'static,
    I: IntoBuf,
    E: Into<StorageError>,
{
    stream.map(|r| match r {
        Ok(d) => Ok(Data::from_buf(d)),
        Err(e) => Err(e.into()),
    })
}

#[derive(Debug, Clone)]
pub(crate) struct Limited<T>
where
    T: Clone,
{
    semaphore: Arc<Semaphore>,
    inner: Arc<Mutex<T>>,
}

impl<T> Limited<T>
where
    T: Clone,
{
    pub fn new(base: T, count: usize) -> Limited<T> {
        Limited {
            semaphore: Arc::new(Semaphore::new(count)),
            inner: Arc::new(Mutex::new(base)),
        }
    }

    pub async fn take(&self) -> InUse<T> {
        let semaphore = self.semaphore.clone();
        let mut permit = Permit::new();
        poll_fn(|cx| permit.poll_acquire(cx, &semaphore))
            .await
            .unwrap();

        // Permit is now acquired.
        let inner = self.inner.lock().unwrap();

        InUse {
            semaphore: self.semaphore.clone(),
            permit,
            inner: inner.deref().clone(),
        }
    }
}

pub(crate) struct InUse<T> {
    semaphore: Arc<Semaphore>,
    permit: Permit,
    inner: T,
}

impl<T> InUse<T> {
    pub fn release(&mut self) {
        if !self.permit.is_acquired() {
            return;
        }

        self.permit.release(&self.semaphore);
    }
}

impl<T> Deref for InUse<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for InUse<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> Drop for InUse<T> {
    fn drop(&mut self) {
        self.release();
    }
}
