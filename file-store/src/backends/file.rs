//! Accesses files on the local filesystem. Included with the feature "file".
//!
//! The [`FileBackend`](struct.FileBackend.html) allows access to the local
//! file system. It must be instantiated with a local directory which is then
//! used as the root of the files visible through the returned
//! [`FileStore`](../../struct.FileStore.html).
//!
//! Directories and symlinks cannot be created but will be visible through
//! [`list_objects`](../../struct.FileStore.html#method.list_objects) and
//! [`get_object`](../../struct.FileStore.html#method.get_objects).
//! [`delete_object`](../../struct.FileStore.html#method.delete_object) and
//! [`write_file_from_stream`](../../struct.FileStore.html#method.write_file_from_stream)
//! will remove these (in the directory case recursively).
use std::convert::{TryFrom, TryInto};
use std::fs::Metadata;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures::future::{ready, Future, FutureExt, TryFutureExt};
use futures::stream::{once, Stream, StreamExt, TryStreamExt};
use log::trace;
use tokio_fs::DirEntry;
use tokio_io::{AsyncRead, AsyncWriteExt, BufReader};

use super::{Backend, BackendImplementation, ObjectInternals, StorageBackend};
use crate::filestore::FileStore;
use crate::types::error;
use crate::types::stream::{MergedStreams, ResultStreamPoll};
use crate::types::*;

// When reading from a file we start requesting INITIAL_BUFFER_SIZE bytes. As
// data is read the available space is reduced until it reaches MIN_BUFFER_SIZE
// at which point we allocate a new buffer of INITIAL_BUFFER_SIZE.
const INITIAL_BUFFER_SIZE: usize = 20 * 1024 * 1024;
const MIN_BUFFER_SIZE: usize = 1 * 1024 * 1024;

async fn read_dir<P>(path: P) -> io::Result<tokio_fs::ReadDir>
where
    P: AsRef<Path> + Send + 'static,
{
    let path = path.as_ref().to_owned();
    let result = tokio_fs::read_dir(path.clone()).await;
    match result {
        Ok(_) => trace!("tokio_fs::read_dir {} success", path.display()),
        Err(ref e) => trace!("tokio_fs::read_dir {} failed: {}", path.display(), e),
    }

    result
}

async fn remove_dir<P>(path: P) -> io::Result<()>
where
    P: AsRef<Path> + Send + 'static,
{
    let path = path.as_ref().to_owned();
    let result = tokio_fs::remove_dir(path.clone()).await;
    match result {
        Ok(_) => trace!("tokio_fs::remove_dir {} success", path.display()),
        Err(ref e) => trace!("tokio_fs::remove_dir {} failed: {}", path.display(), e),
    }

    result
}

async fn remove_file<P>(path: P) -> io::Result<()>
where
    P: AsRef<Path> + Send + 'static,
{
    let path = path.as_ref().to_owned();
    let result = tokio_fs::remove_file(path.clone()).await;
    match result {
        Ok(_) => trace!("tokio_fs::remove_file {} success", path.display()),
        Err(ref e) => trace!("tokio_fs::remove_file {} failed: {}", path.display(), e),
    }

    result
}

async fn symlink_metadata<P>(path: P) -> io::Result<Metadata>
where
    P: AsRef<Path> + Send + 'static,
{
    let path = path.as_ref().to_owned();
    let result = tokio_fs::symlink_metadata(path.clone()).await;
    match result {
        Ok(_) => trace!("tokio_fs::symlink_metadata {} success", path.display()),
        Err(ref e) => trace!(
            "tokio_fs::symlink_metadata {} failed: {}",
            path.display(),
            e
        ),
    }

    result
}

struct File {}

impl File {
    pub async fn open<P>(path: P) -> io::Result<tokio_fs::File>
    where
        P: AsRef<Path> + 'static,
    {
        let path = path.as_ref().to_owned();
        let result = tokio_fs::File::open(path.clone()).await;
        match result {
            Ok(_) => trace!("tokio_fs::File::open {} success", path.display()),
            Err(ref e) => trace!("tokio_fs::File::open {} failed: {}", path.display(), e),
        }

        result
    }

    pub async fn create<P>(path: P) -> io::Result<tokio_fs::File>
    where
        P: AsRef<Path> + 'static,
    {
        let path = path.as_ref().to_owned();
        let result = tokio_fs::File::create(path.clone()).await;
        match result {
            Ok(_) => trace!("tokio_fs::File::create {} success", path.display()),
            Err(ref e) => trace!("tokio_fs::File::create {} failed: {}", path.display(), e),
        }

        result
    }
}

fn get_storage_error(error: io::Error, path: ObjectPath) -> StorageError {
    match error.kind() {
        io::ErrorKind::NotFound => error::not_found(path, Some(error)),
        _ => error::other_error(&path.to_string(), Some(error)),
    }
}

fn wrap_future<F>(future: F, path: ObjectPath) -> impl Future<Output = StorageResult<F::Ok>>
where
    F: TryFutureExt<Error = io::Error>,
{
    future.map_err(move |e| get_storage_error(e, path))
}

fn wrap_stream<S>(stream: S, path: ObjectPath) -> impl Stream<Item = StorageResult<S::Ok>>
where
    S: TryStreamExt<Error = io::Error>,
{
    stream.map_err(move |e| get_storage_error(e, path.clone()))
}

fn get_object(path: ObjectPath, metadata: Option<Metadata>) -> Object {
    let (object_type, size) = match metadata {
        Some(m) => {
            if m.is_file() {
                (ObjectType::File, m.len())
            } else if m.is_dir() {
                (ObjectType::Directory, 0)
            } else {
                (ObjectType::Symlink, 0)
            }
        }
        None => (ObjectType::Unknown, 0),
    };

    Object {
        internals: ObjectInternals::File,
        object_type,
        path,
        size,
    }
}

#[derive(Clone, Debug)]
struct FileSpace {
    base: PathBuf,
}

impl FileSpace {
    fn get_std_path(&self, path: &ObjectPath) -> StorageResult<PathBuf> {
        let mut result = self.base.clone();
        for part in path.parts() {
            result.push(part);
        }

        Ok(result)
    }
}

fn directory_stream(
    space: &FileSpace,
    path: ObjectPath,
) -> impl Stream<Item = StorageResult<(ObjectPath, Option<Metadata>)>> {
    #[allow(clippy::needless_lifetimes)]
    async fn build_base(
        space: &FileSpace,
        path: ObjectPath,
    ) -> StorageResult<impl Stream<Item = StorageResult<DirEntry>>> {
        let target = space.get_std_path(&path)?;
        Ok(wrap_stream(
            wrap_future(read_dir(target.clone()), path.clone()).await?,
            path,
        ))
    }

    async fn start_stream(
        space: FileSpace,
        path: ObjectPath,
    ) -> impl Stream<Item = StorageResult<(ObjectPath, Option<Metadata>)>> {
        let stream = match build_base(&space, path.clone()).await {
            Ok(s) => s,
            Err(e) => {
                return once(ready::<StorageResult<(ObjectPath, Option<Metadata>)>>(Err(
                    e,
                )))
                .left_stream()
            }
        };

        stream
            .and_then(move |direntry| {
                let fname = direntry.file_name();
                let mut path = path.clone();
                wrap_future(symlink_metadata(direntry.path()), path.clone()).map(move |result| {
                    let filename = match fname.into_string() {
                        Ok(f) => f,
                        Err(_) => {
                            return Err(error::invalid_data::<StorageError>(
                                "Unable to convert OSString.",
                                None,
                            ))
                        }
                    };

                    path.push_part(&filename);
                    let maybe_meta = match result {
                        Ok(m) => Some(m),
                        Err(_) => None,
                    };

                    Ok((path, maybe_meta))
                })
            })
            .right_stream()
    }

    start_stream(space.clone(), path).flatten_stream()
}

type FileList = StorageResult<(ObjectPath, Option<Metadata>)>;
struct FileLister {
    stream: Pin<Box<MergedStreams<FileList>>>,
    space: FileSpace,
    prefix: ObjectPath,
}

impl FileLister {
    fn list(space: FileSpace, mut prefix: ObjectPath) -> FileLister {
        let mut lister = FileLister {
            stream: Box::pin(MergedStreams::new()),
            space,
            prefix: prefix.clone(),
        };

        prefix.pop_part();

        lister.add_directory(prefix);
        lister
    }

    fn add_directory(&mut self, path: ObjectPath) {
        self.stream.push(directory_stream(&self.space, path));
    }
}

impl Stream for FileLister {
    type Item = StorageResult<Object>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> ResultStreamPoll<Object> {
        loop {
            match self.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok((path, maybe_metadata)))) => {
                    if path.starts_with(&self.prefix) {
                        if let Some(ref metadata) = maybe_metadata {
                            if metadata.is_dir() {
                                self.add_directory(path.clone());
                            }
                        }

                        return Poll::Ready(Some(Ok(get_object(path, maybe_metadata))));
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

struct ReadStream<R>
where
    R: AsyncRead,
{
    path: ObjectPath,
    reader: Pin<Box<R>>,
    buffer: BytesMut,
}

impl<R> ReadStream<R>
where
    R: AsyncRead,
{
    fn build<T>(path: ObjectPath, reader: T) -> DataStream
    where
        T: AsyncRead + Send + 'static,
    {
        let buf_reader = BufReader::new(reader);

        let mut buffer = BytesMut::with_capacity(INITIAL_BUFFER_SIZE);
        unsafe {
            buffer.set_len(INITIAL_BUFFER_SIZE);
            buf_reader.prepare_uninitialized_buffer(&mut buffer);
        }

        let stream = ReadStream {
            path,
            reader: Box::pin(buf_reader),
            buffer,
        };

        DataStream::from_stream(stream)
    }

    fn inner_poll(&mut self, cx: &mut Context) -> ResultStreamPoll<Data> {
        match self.reader.as_mut().poll_read(cx, &mut self.buffer) {
            Poll::Ready(Ok(0)) => Poll::Ready(None),
            Poll::Ready(Ok(size)) => {
                let data = self.buffer.split_to(size);

                if self.buffer.len() < MIN_BUFFER_SIZE {
                    self.buffer = BytesMut::with_capacity(INITIAL_BUFFER_SIZE);
                    unsafe {
                        self.buffer.set_len(INITIAL_BUFFER_SIZE);
                        self.reader.prepare_uninitialized_buffer(&mut self.buffer);
                    }
                }

                Poll::Ready(Some(Ok(data.freeze())))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(get_storage_error(e, self.path.clone())))),
        }
    }
}

impl<R> Stream for ReadStream<R>
where
    R: AsyncRead,
{
    type Item = StorageResult<Data>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> ResultStreamPoll<Data> {
        self.inner_poll(cx)
    }
}

#[allow(clippy::needless_lifetimes)]
async fn delete_directory(space: FileSpace, path: ObjectPath) -> StorageResult<()> {
    let mut dir_path = path.clone();
    dir_path.push_part("");

    let allfiles = FileLister::list(space.clone(), dir_path)
        .try_collect::<Vec<Object>>()
        .await?;
    let nondirectories = allfiles
        .iter()
        .filter(|file| file.object_type() != ObjectType::Directory);
    let directories = allfiles
        .iter()
        .filter(|file| file.object_type() == ObjectType::Directory);

    for file in nondirectories {
        let target = space.get_std_path(&file.path())?;
        wrap_future(remove_file(target), file.path()).await?;
    }

    for dir in directories {
        let target = space.get_std_path(&dir.path())?;
        wrap_future(remove_dir(target), dir.path()).await?;
    }

    let target = space.get_std_path(&path)?;
    wrap_future(remove_dir(target), path).await
}

/// The backend implementation for local file storage. Only included when the
/// `file` feature is enabled.
#[derive(Clone, Debug)]
pub struct FileBackend {
    space: FileSpace,
}

impl FileBackend {
    /// Creates a new [`FileStore`](../../struct.FileStore.html) instance using the
    /// file backend.
    ///
    /// The root path provided must be a directory and is used as the base of
    /// the visible storage.
    pub fn connect(root: &Path) -> ConnectFuture {
        let target = root.to_owned();
        ConnectFuture::from_future(async move {
            let metadata =
                wrap_future(symlink_metadata(target.clone()), ObjectPath::empty()).await?;
            if !metadata.is_dir() {
                Err(error::invalid_settings::<StorageError>(
                    "Root path is not a directory.",
                    None,
                ))
            } else {
                Ok(FileStore {
                    backend: BackendImplementation::File(Box::new(FileBackend {
                        space: FileSpace { base: target },
                    })),
                })
            }
        })
    }
}

impl TryFrom<FileStore> for FileBackend {
    type Error = StorageError;

    fn try_from(file_store: FileStore) -> StorageResult<FileBackend> {
        if let BackendImplementation::File(b) = file_store.backend {
            Ok(b.deref().clone())
        } else {
            Err(error::invalid_settings::<StorageError>(
                "FileStore does not hold a FileBackend",
                None,
            ))
        }
    }
}

impl StorageBackend for FileBackend {
    fn backend_type(&self) -> Backend {
        Backend::File
    }

    fn list_objects<P>(&self, prefix: P) -> ObjectStreamFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        async fn list(space: FileSpace, prefix: ObjectPath) -> StorageResult<ObjectStream> {
            Ok(ObjectStream::from_stream(FileLister::list(space, prefix)))
        }

        let path = match prefix.try_into() {
            Ok(p) => p,
            Err(e) => return ObjectStreamFuture::from_value(Err(e.into())),
        };

        ObjectStreamFuture::from_future(list(self.space.clone(), path))
    }

    fn list_directory<P>(&self, dir: P) -> ObjectStreamFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        async fn list(space: FileSpace, directory: ObjectPath) -> StorageResult<ObjectStream> {
            let _path = space.get_std_path(&directory)?;

            unimplemented!();
        }

        let mut path = match dir.try_into() {
            Ok(p) => p,
            Err(e) => return ObjectStreamFuture::from_value(Err(e.into())),
        };

        if !path.is_empty() && path.is_dir_prefix() {
            path.pop_part();
        }

        ObjectStreamFuture::from_future(list(self.space.clone(), path))
    }

    fn get_object<P>(&self, path: P) -> ObjectFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        async fn get(space: FileSpace, path: ObjectPath) -> StorageResult<Object> {
            let target = space.get_std_path(&path)?;

            match symlink_metadata(target.clone()).await {
                Ok(m) => Ok(get_object(path, Some(m))),
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        Err(error::not_found(path, Some(e)))
                    } else {
                        Ok(get_object(path, None))
                    }
                }
            }
        }

        let path = match path.try_into() {
            Ok(p) => p,
            Err(e) => return ObjectFuture::from_value(Err(e.into())),
        };

        if path.is_dir_prefix() {
            return ObjectFuture::from_value(Err(error::invalid_path(
                path,
                "Object paths cannot be empty or end with a '/' character.",
            )));
        }

        ObjectFuture::from_future(get(self.space.clone(), path))
    }

    fn get_file_stream<O>(&self, reference: O) -> DataStreamFuture
    where
        O: ObjectReference,
    {
        async fn read(space: FileSpace, path: ObjectPath) -> StorageResult<DataStream> {
            let target = space.get_std_path(&path)?;

            let metadata = wrap_future(symlink_metadata(target.clone()), path.clone()).await?;
            if !metadata.is_file() {
                return Err(error::not_found::<StorageError>(path, None));
            }

            let file = wrap_future(File::open(target), path.clone()).await?;
            Ok(ReadStream::<tokio_fs::File>::build(path, file))
        }

        match reference.into_path() {
            Ok(p) => DataStreamFuture::from_future(read(self.space.clone(), p)),
            Err(e) => DataStreamFuture::from_value(Err(e)),
        }
    }

    fn delete_object<O>(&self, reference: O) -> OperationCompleteFuture
    where
        O: ObjectReference,
    {
        async fn delete(space: FileSpace, path: ObjectPath) -> StorageResult<()> {
            let target = space.get_std_path(&path)?;
            let metadata = wrap_future(symlink_metadata(target.clone()), path.clone()).await?;

            if !metadata.is_dir() {
                wrap_future(remove_file(target.clone()), path.clone()).await
            } else {
                delete_directory(space, path).await
            }
        }

        match reference.into_path() {
            Ok(p) => OperationCompleteFuture::from_future(delete(self.space.clone(), p)),
            Err(e) => OperationCompleteFuture::from_value(Err(e)),
        }
    }

    fn write_file_from_stream<S, P>(&self, path: P, stream: S) -> WriteCompleteFuture
    where
        S: Stream<Item = StorageResult<Data>> + Send + 'static,
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        async fn write<S>(
            space: FileSpace,
            path: ObjectPath,
            mut stream: S,
        ) -> Result<(), TransferError>
        where
            S: Stream<Item = StorageResult<Data>> + Send + Unpin + 'static,
        {
            let target = space
                .get_std_path(&path)
                .map_err(TransferError::TargetError)?;

            match symlink_metadata(target.clone()).await {
                Ok(m) => {
                    if m.is_dir() {
                        delete_directory(space, path.clone())
                            .await
                            .map_err(TransferError::TargetError)?;
                    } else {
                        wrap_future(remove_file(target.clone()), path.clone())
                            .await
                            .map_err(TransferError::TargetError)?;
                    }
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        return Err(TransferError::TargetError(get_storage_error(e, path)));
                    }
                }
            };

            let mut file = wrap_future(File::create(target), path.clone())
                .await
                .map_err(TransferError::TargetError)?;

            let mut pos = 0;
            loop {
                println!("Polling for data at {}", pos);
                let option = stream.next().await;
                if let Some(result) = option {
                    let data = result.map_err(TransferError::SourceError)?;
                    println!("Got {} bytes", data.len());
                    match file.write_all(&data).await {
                        Ok(()) => (),
                        Err(e) => {
                            return Err(TransferError::TargetError(get_storage_error(
                                e,
                                path.clone(),
                            )))
                        }
                    };
                    pos += data.len();
                } else {
                    println!("Finished at {}", pos);
                    break;
                }
            }

            match file.flush().await {
                Ok(()) => (),
                Err(e) => {
                    return Err(TransferError::TargetError(get_storage_error(
                        e,
                        path.clone(),
                    )))
                }
            }
            match file.shutdown().await {
                Ok(()) => (),
                Err(e) => {
                    return Err(TransferError::TargetError(get_storage_error(
                        e,
                        path.clone(),
                    )))
                }
            }

            Ok(())
        }

        let path = match path.try_into() {
            Ok(t) => t,
            Err(e) => {
                return WriteCompleteFuture::from_value(Err(TransferError::TargetError(e.into())))
            }
        };

        WriteCompleteFuture::from_future(write(self.space.clone(), path, Box::pin(stream)))
    }
}
