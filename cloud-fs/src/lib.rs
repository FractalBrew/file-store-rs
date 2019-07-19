//! An abstract asynchronous API for accessing a filesystem that could be on any of a number of different local and cloud storage backends.
//!
//! The API offers functions for listing, reading, writing and deleting files
//! from a storage backend. Each backend offers the same API plus in some cases
//! some additional backend specific functionality.
//!
//! Obviously offering the same API across all backends means the API is fairly
//! basic, but if all you want to do is write, read or list files it should be
//! plenty. Past that some of the backends provide access to internal functions
//! via a `from_fs` function.
//!
//! Which backend is available depends on the features cloud-fs is compiled
//! with. See the [`backends`](backends/index.html) module.
//!
//! The [`Fs`](struct.Fs.html) is the main API used to access storage.
#![warn(missing_docs)]
#![feature(async_await)]

extern crate bytes;
extern crate tokio;

pub mod backends;
pub mod executor;
mod futures;
mod types;

use std::future::Future;
use std::io;

use ::futures::stream::{Stream, TryStreamExt};
use bytes::buf::FromBuf;
use bytes::{Bytes, IntoBuf};

use crate::futures::*;
use backends::Backend;
use backends::BackendImplementation;

pub use types::{Data, FsError, FsErrorKind, FsFile, FsFileType, FsPath, FsResult};

/// The trait that every storage backend must implement at a minimum.
trait FsImpl {
    /// Returns the type of backend.
    fn backend_type(&self) -> Backend;

    /// Lists the files that start with the given path.
    ///
    /// See [Fs.list_files](struct.Fs.html#method.list_files).
    fn list_files(&self, path: FsPath) -> FileListFuture;

    /// Gets info about the file at the given path.
    ///
    /// See [Fs.get_file](struct.Fs.html#method.get_file).
    fn get_file(&self, path: FsPath) -> FileFuture;

    /// Gets a stream of data for the file at the given path.
    ///
    /// See [Fs.get_file](struct.Fs.html#method.get_file_stream).
    fn get_file_stream(&self, path: FsPath) -> DataStreamFuture;

    /// Deletes the file at the given path.
    ///
    /// See [Fs.get_file](struct.Fs.html#method.delete_file).
    fn delete_file(&self, path: FsPath) -> OperationCompleteFuture;

    /// Writes a stram of data the the file at the given path.
    ///
    /// See [Fs.get_file](struct.Fs.html#method.write_from_stream).
    fn write_from_stream(
        &self,
        path: FsPath,
        stream: StreamHolder<Result<Bytes, io::Error>>,
    ) -> OperationCompleteFuture;
}

/// The main implementation used to interact with a storage backend.
///
/// Create an `Fs` from one of the alternative [`backends`](backends/index.html).
#[derive(Debug)]
pub struct Fs {
    backend: BackendImplementation,
}

impl Fs {
    fn check_path(&self, path: &FsPath, should_be_dir: bool) -> FsResult<()> {
        if !path.is_absolute() {
            Err(FsError::invalid_path(
                path.clone(),
                "Requests must use an absolute path.",
            ))
        } else if should_be_dir && !path.is_directory() {
            Err(FsError::invalid_path(
                path.clone(),
                "This request requires the path to a directory.",
            ))
        } else if !should_be_dir && path.is_directory() {
            Err(FsError::invalid_path(
                path.clone(),
                "This request requires the path to a file.",
            ))
        } else if path.is_windows() {
            Err(FsError::invalid_path(
                path.clone(),
                "Paths should not include windows prefixes.",
            ))
        } else {
            Ok(())
        }
    }

    /// Retrieves the back-end that this `Fs` is using.
    pub(crate) fn backend_implementation(&self) -> &BackendImplementation {
        &self.backend
    }

    /// Retrieves the type of back-end that this `Fs` is using.
    pub fn backend_type(&self) -> Backend {
        self.backend.get().backend_type()
    }

    /// Lists the files that start with the given path.
    ///
    /// Because the majority of cloud storage systems do not really have a
    /// notion of directories and files, just file identifiers, this function
    /// will return any files that have an identifier prefixed by `path`.
    pub fn list_files(&self, path: FsPath) -> FileListFuture {
        if let Err(e) = self.check_path(&path, true) {
            return FileListFuture::from_error(e);
        }

        self.backend.get().list_files(path)
    }

    /// Gets info about the file at the given path.
    ///
    /// This will return a [`NotFound`](enum.FsErrorKind.html#variant.NotFound)
    /// error if the file does not exist.
    pub fn get_file(&self, path: FsPath) -> FileFuture {
        if let Err(e) = self.check_path(&path, false) {
            return FileFuture::from_error(e);
        }

        self.backend.get().get_file(path)
    }

    /// Gets a stream of data for the file at the given path.
    ///
    /// The data returned is not necessarily in any particular chunk size.
    /// Dropping the stream at any point before completion should be considered
    /// to be safe.
    ///
    /// This will return a [`NotFound`](enum.FsErrorKind.html#variant.NotFound)
    /// error if the file does not exist.
    pub fn get_file_stream(&self, path: FsPath) -> DataStreamFuture {
        if let Err(e) = self.check_path(&path, false) {
            return DataStreamFuture::from_error(e);
        }

        self.backend.get().get_file_stream(path)
    }

    /// Deletes the file at the given path.
    ///
    /// For backends that support physical directories this will also delete the
    /// directory and its contents.
    ///
    /// This will return a [`NotFound`](enum.FsErrorKind.html#variant.NotFound)
    /// error if the file does not exist.
    pub fn delete_file(&self, path: FsPath) -> OperationCompleteFuture {
        if let Err(e) = self.check_path(&path, false) {
            return OperationCompleteFuture::from_error(e);
        }

        self.backend.get().delete_file(path)
    }

    /// Writes a stream of data to the file at the given path.
    ///
    /// Calling this will overwrite anything at the given path (notably on
    /// backends that support symlinks or directories those will be deleted
    /// along with their contents and replaced with a file). The rationale for
    /// this is that for network based backends not overwriting generally
    /// involves more API calls to check if something is there first. If you
    /// care about overwriting, call [`get_file`](struct.Fs.html#method.get_file)
    /// first.
    ///
    /// If this operation fails there are no guarantees about the state of the
    /// file. If that is an issue then you should consider always calling
    /// [`delete_file`](struct.Fs.html#method.delete_file) after a failure.
    ///
    /// The future returned will only resolve once all the data from the stream
    /// is succesfully written to storage. If the provided stream resolves to
    /// None at any point this will be considered the end of the data to be
    /// written.
    pub fn write_from_stream<S, I>(&self, path: FsPath, stream: S) -> OperationCompleteFuture
    where
        S: Stream<Item = Result<I, io::Error>> + Send + 'static,
        I: IntoBuf,
    {
        if let Err(e) = self.check_path(&path, false) {
            return OperationCompleteFuture::from_error(e);
        }

        #[allow(clippy::redundant_closure)]
        let mapped = stream.map_ok(|b| Bytes::from_buf(b));

        self.backend
            .get()
            .write_from_stream(path, StreamHolder::new(mapped))
    }
}
