//! Accesses files in a Backblaze B2 bucket. Included with the feature "b2".
//!
//! The [`B2Backend`](struct.B2Backend.html) can be initialized with as little
//! as a key id and key (these can be the master key or an application key). It
//! also supports a [`builder`](struct.B2Backend.html#method.builder) pattern to
//! add additional configuration including a path prefix to restrict the files
//! visible.
//!
//! [`ObjectPath`](../../struct.ObjectPath.html)'s represent the names of files.
//! The first directory part of a path (the string up until the first `/`) is
//! used as the name of the bucket. The rest can be freeform though people
//! generally use a regular path string separated by `/` characters to form
//! a hierarchy. Attempting to write a file at the bucket level will fail
//! however writing a file inside a bucket that does not yet exist will create
//! the bucket (assuming the key has permission to do so).
//!
//! In order to be compatible with other backends, but still include some useful
//! functionality file versioning (if enabled for the bucket) is currently
//! handled as follows:
//! * Deleting a file will delete all of its versions.
//! * Replacing a file will add a new version.
//!
//! Setting a file's mimetype on uploas is not currently supported. The backend
//! will rely on B2's automatic mimetype detection to set the mimetype. This
//! uses the file's extension to set a mimetype from a [list of mappings](https://www.backblaze.com/b2/docs/content-types.html)
//! and falls back to `application/octet-stream` in case of failure.
//!
//! The last modified time of an uploaded file will be set to the time that the
//! upload began.
use std::convert::{TryFrom, TryInto};
use std::future::Future;
use std::io::Read;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::encode;
use bytes::IntoBuf;
use futures::compat::*;
use futures::future::ready;
use futures::lock::Mutex;
use futures::stream::{Stream, TryStreamExt};
use http::method::Method;
use hyper::body::Body;
use hyper::client::connect::HttpConnector;
use hyper::client::Client as HyperClient;
use hyper::Request;
use hyper_tls::HttpsConnector;
use serde_json::{from_str, to_string};

use storage_types::b2::v2::requests::*;
use storage_types::b2::v2::responses::*;

use super::{Backend, BackendImplementation, ObjectInternals, StorageBackend};
use crate::filestore::FileStore;
use crate::types::stream::{MergedStreams, ResultStreamPoll};
use crate::types::*;

type Client = HyperClient<HttpsConnector<HttpConnector>>;

const API_RETRIES: usize = 3;

impl From<http::Error> for StorageError {
    fn from(error: http::Error) -> StorageError {
        error::other_error(&error.to_string(), Some(error))
    }
}

impl From<hyper::error::Error> for StorageError {
    fn from(error: hyper::error::Error) -> StorageError {
        if error.is_parse() || error.is_user() {
            error::invalid_data(&error.to_string(), Some(error))
        } else if error.is_canceled() {
            error::cancelled(&error.to_string(), Some(error))
        } else if error.is_closed() {
            error::connection_closed(&error.to_string(), Some(error))
        } else if error.is_connect() {
            error::connection_failed(&error.to_string(), Some(error))
        } else if error.is_incomplete_message() {
            error::connection_closed(&error.to_string(), Some(error))
        } else {
            error::invalid_data(&error.to_string(), Some(error))
        }
    }
}

impl From<serde_json::error::Error> for StorageError {
    fn from(error: serde_json::error::Error) -> StorageError {
        error::internal_error("Failes to encode request data.", Some(error))
    }
}

fn new_object(bucket: &str, info: &FileInfo, prefix: &ObjectPath) -> StorageResult<Object> {
    let mut path = ObjectPath::new(&info.file_name)?;
    path.shift_part(bucket);
    let is_dir = path.is_dir_prefix();

    let o_type = if is_dir {
        path.pop_part();
        ObjectType::Directory
    } else {
        ObjectType::File
    };

    for _ in prefix.parts() {
        path.unshift_part();
    }

    Ok(Object {
        internals: ObjectInternals::B2,
        object_type: o_type,
        path,
        size: info.content_length,
    })
}

#[derive(Clone, Debug)]
struct B2Settings {
    key_id: String,
    key: String,
    host: String,
    prefix: ObjectPath,
}

macro_rules! b2_api {
    ($method:ident, $request:ident, $response:ident) => {
        #[allow(dead_code)]
        pub fn $method(
            &self,
            path: ObjectPath,
            request: $request,
        ) -> impl Future<Output = StorageResult<$response>> {
            self.clone().b2_api_call(stringify!($method), path, request)
        }
    }
}

#[derive(Clone, Debug)]
struct B2Client {
    client: Client,
    settings: B2Settings,
    session: Arc<Mutex<Option<AuthorizeAccountResponse>>>,
}

impl B2Client {
    fn api_url(&self, host: &str, method: &str) -> String {
        format!("{}/b2api/{}/{}", host, B2_VERSION, method)
    }

    async fn account_id(&self) -> StorageResult<String> {
        let session = self.session().await?;
        Ok(session.account_id)
    }

    async fn request<R>(
        &self,
        method: &str,
        path: ObjectPath,
        request: Request<Body>,
    ) -> StorageResult<R>
    where
        for<'de> R: serde::de::Deserialize<'de>,
    {
        let response = self.client.request(request).compat().await?;
        let (meta, body) = response.into_parts();

        let mut data: String = String::new();
        BlockingStreamReader::from_stream(body.compat())
            .read_to_string(&mut data)
            .unwrap();

        if meta.status.is_success() {
            match from_str(&data) {
                Ok(r) => Ok(r),
                Err(e) => Err(error::invalid_data(
                    &format!("Unable to parse response from {}.", method),
                    Some(e),
                )),
            }
        } else {
            Err(generate_error(method, &path, &data))
        }
    }

    async fn b2_authorize_account(&self) -> StorageResult<AuthorizeAccountResponse> {
        let secret = format!(
            "Basic {}",
            encode(&format!("{}:{}", self.settings.key_id, self.settings.key))
        );

        let request = Request::builder()
            .method(Method::GET)
            .uri(self.api_url(&self.settings.host, "b2_authorize_account"))
            .header("Authorization", secret)
            .body(Body::empty())?;

        let empty = ObjectPath::empty();
        self.request("b2_authorize_account", empty, request).await
    }

    async fn b2_api_call<S, Q>(self, method: &str, path: ObjectPath, request: S) -> StorageResult<Q>
    where
        S: serde::ser::Serialize + Clone,
        for<'de> Q: serde::de::Deserialize<'de>,
    {
        let mut tries: usize = 0;
        loop {
            let (api_url, authorization) = {
                let session = self.session().await?;
                (session.api_url.clone(), session.authorization_token.clone())
            };

            let data = to_string(&request)?;

            let request = Request::builder()
                .method(Method::POST)
                .uri(self.api_url(&api_url, method))
                .header("Authorization", &authorization)
                .body(data.into())?;

            match self.request(method, path.clone(), request).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    if e.kind() == error::StorageErrorKind::AccessExpired {
                        self.reset_session(&authorization).await;

                        tries += 1;
                        if tries < API_RETRIES {
                            continue;
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    b2_api!(b2_list_buckets, ListBucketsRequest, ListBucketsResponse);
    b2_api!(b2_get_file_info, GetFileInfoRequest, GetFileInfoResponse);
    b2_api!(
        b2_list_file_names,
        ListFileNamesRequest,
        ListFileNamesResponse
    );
    b2_api!(
        b2_list_file_versions,
        ListFileVersionsRequest,
        ListFileVersionsResponse
    );

    async fn reset_session(&self, auth_token: &str) {
        let mut session = self.session.lock().await;
        if let Some(ref s) = session.deref() {
            if s.authorization_token == auth_token {
                session.take();
            }
        }
    }

    async fn session(&self) -> StorageResult<AuthorizeAccountResponse> {
        let mut session = self.session.lock().await;
        if let Some(ref s) = session.deref() {
            Ok(s.clone())
        } else {
            let new_session = self.b2_authorize_account().await?;
            session.replace(new_session.clone());
            Ok(new_session)
        }
    }
}

/// A stream of objects from B2.
struct ListFileNamesStream {
    client: B2Client,
    options: ListFileNamesRequest,
    path: ObjectPath,
    results: Vec<FileInfo>,
    future: Option<
        Pin<Box<dyn Future<Output = StorageResult<ListFileNamesResponse>> + Send + 'static>>,
    >,
}

impl ListFileNamesStream {
    fn new(
        path: ObjectPath,
        client: B2Client,
        options: ListFileNamesRequest,
    ) -> ListFileNamesStream {
        let list_client = client.clone();
        ListFileNamesStream {
            future: Some(Box::pin(
                list_client.b2_list_file_names(path.clone(), options.clone()),
            )),
            path,
            client,
            options,
            results: Vec::new(),
        }
    }
}

impl Stream for ListFileNamesStream {
    type Item = StorageResult<FileInfo>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> ResultStreamPoll<FileInfo> {
        if let Some(ref mut f) = self.future {
            match f.as_mut().poll(cx) {
                Poll::Pending => (),
                Poll::Ready(Ok(result)) => {
                    self.results.extend(result.files);
                    match result.next_file_name {
                        Some(fname) => {
                            let mut next = self.options.clone();
                            next.start_file_name = Some(fname);
                            self.future = Some(Box::pin(
                                self.client.b2_list_file_names(self.path.clone(), next),
                            ));
                        }
                        None => {
                            self.future = None;
                        }
                    }
                }
                Poll::Ready(Err(e)) => {
                    self.future = None;
                    return Poll::Ready(Some(Err(e)));
                }
            }
        }

        if !self.results.is_empty() {
            return Poll::Ready(Some(Ok(self.results.remove(0))));
        }

        if self.results.is_empty() && self.future.is_none() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

/// The backend implementation for B2 storage.
#[derive(Debug, Clone)]
pub struct B2Backend {
    settings: B2Settings,
    client: Client,
    session: Arc<Mutex<Option<AuthorizeAccountResponse>>>,
}

impl B2Backend {
    /// Creates a new [`FileStore`](../../struct.FileStore.html) instance using the
    /// b2 backend.
    ///
    /// When constructed in this manner the root for all paths will be at the
    /// account level.
    pub fn connect(key_id: &str, key: &str) -> ConnectFuture {
        B2Backend::builder(key_id, key).connect()
    }

    /// Creates a new [`B2BackendBuilder`](struct.B2BackendBuilder.html).
    pub fn builder(key_id: &str, key: &str) -> B2BackendBuilder {
        B2BackendBuilder {
            settings: B2Settings {
                key_id: key_id.to_owned(),
                key: key.to_owned(),
                host: B2_API_HOST.to_owned(),
                prefix: ObjectPath::empty(),
            },
        }
    }

    /// Creates a new [`B2Client`](struct.B2Client.html) that can be used for
    /// making B2 API calls.
    fn client(&self) -> B2Client {
        B2Client {
            settings: self.settings.clone(),
            client: self.client.clone(),
            session: self.session.clone(),
        }
    }
}

#[derive(Debug, Clone)]
/// Used to build a [`B2Backend`](struct.B2Backend.html) with some custom
/// settings.
pub struct B2BackendBuilder {
    settings: B2Settings,
}

impl B2BackendBuilder {
    /// Sets the API host for B2.
    ///
    /// This is generally only used for testing purposes.
    pub fn host(mut self, host: &str) -> B2BackendBuilder {
        self.settings.host = host.to_owned();
        self
    }

    /// Sets a path prefix for this storage.
    ///
    /// Essentially sets the 'root directory' for this storage, any paths
    /// requested will be joined with this with a `/` character in between, so
    /// this can be either the name of a bucket or a bucket followed by some
    /// directory parts within that bucket.
    pub fn prefix(mut self, prefix: ObjectPath) -> B2BackendBuilder {
        self.settings.prefix = prefix;
        self
    }

    /// Creates a new B2 based [`FileStore`](../../struct.FileStore.html) using
    /// this builder's settings.
    pub fn connect(self) -> ConnectFuture {
        ConnectFuture::from_future(async {
            let connector = match HttpsConnector::new(4) {
                Ok(c) => c,
                Err(e) => {
                    return Err(error::connection_failed(
                        "Could not create http connection.",
                        Some(e),
                    ))
                }
            };

            let client = HyperClient::builder().build(connector);

            let backend = B2Backend {
                settings: self.settings,
                client,
                session: Arc::new(Mutex::new(None)),
            };

            // Make sure we can connect.
            let b2_client = backend.client();
            b2_client.session().await?;

            Ok(FileStore {
                backend: BackendImplementation::B2(Box::new(backend)),
            })
        })
    }
}

impl TryFrom<FileStore> for B2Backend {
    type Error = StorageError;

    fn try_from(file_store: FileStore) -> StorageResult<B2Backend> {
        if let BackendImplementation::B2(b) = file_store.backend {
            Ok(b.deref().clone())
        } else {
            Err(error::invalid_settings::<StorageError>(
                "FileStore does not hold a FileBackend",
                None,
            ))
        }
    }
}

impl StorageBackend for B2Backend {
    fn backend_type(&self) -> Backend {
        Backend::B2
    }

    fn list_objects<P>(&self, prefix: P) -> ObjectStreamFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        async fn list(
            client: B2Client,
            backend_prefix: ObjectPath,
            prefix: ObjectPath,
        ) -> StorageResult<ObjectStream> {
            let mut file_part = backend_prefix.join(&prefix);
            let is_dir = file_part.is_dir_prefix();
            let bucket = file_part.unshift_part().unwrap_or_else(String::new);

            let mut request = ListBucketsRequest {
                account_id: client.account_id().await?,
                bucket_id: None,
                bucket_name: None,
                bucket_types: Default::default(),
            };

            if !file_part.is_empty() || is_dir {
                // Only include the bucket named `bucket`.
                request.bucket_name = Some(bucket.clone());
            }

            let path = ObjectPath::new(&bucket)?;
            let listers = client
                .b2_list_buckets(path, request)
                .await?
                .buckets
                .drain(..)
                .filter(|b| b.bucket_name.starts_with(&bucket))
                .map(move |b| {
                    let request = ListFileNamesRequest {
                        bucket_id: b.bucket_id.clone(),
                        start_file_name: None,
                        max_file_count: None,
                        prefix: Some(file_part.to_string()),
                        delimiter: None,
                    };

                    let temp_prefix = backend_prefix.clone();
                    ListFileNamesStream::new(prefix.clone(), client.clone(), request)
                        .and_then(move |f| ready(new_object(&b.bucket_name, &f, &temp_prefix)))
                })
                .fold(MergedStreams::new(), |mut m, s| {
                    m.push(s);
                    m
                });

            Ok(ObjectStream::from_stream(listers))
        }

        let prefix = match prefix.try_into() {
            Ok(p) => p,
            Err(e) => return ObjectStreamFuture::from_value(Err(e.into())),
        };

        ObjectStreamFuture::from_future(list(self.client(), self.settings.prefix.clone(), prefix))
    }

    fn list_directory<P>(&self, dir: P) -> ObjectStreamFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        let mut path = match dir.try_into() {
            Ok(p) => p,
            Err(e) => return ObjectStreamFuture::from_value(Err(e.into())),
        };

        if !path.is_empty() && path.is_dir_prefix() {
            path.pop_part();
        }

        unimplemented!();
    }

    fn get_object<P>(&self, path: P) -> ObjectFuture
    where
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
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

        unimplemented!();
    }

    fn get_file_stream<O>(&self, _reference: O) -> DataStreamFuture
    where
        O: ObjectReference,
    {
        unimplemented!();
    }

    fn delete_object<O>(&self, _reference: O) -> OperationCompleteFuture
    where
        O: ObjectReference,
    {
        unimplemented!();
    }

    fn write_file_from_stream<S, I, E, P>(&self, _path: P, _stream: S) -> WriteCompleteFuture
    where
        S: Stream<Item = Result<I, E>> + Send + 'static,
        I: IntoBuf + 'static,
        E: 'static + std::error::Error + Send + Sync,
        P: TryInto<ObjectPath>,
        P::Error: Into<StorageError>,
    {
        unimplemented!();
    }
}

fn generate_error(method: &str, path: &ObjectPath, response: &str) -> StorageError {
    let error: ErrorResponse = match from_str(response) {
        Ok(r) => r,
        Err(e) => {
            return error::invalid_data(
                &format!("Unable to parse error response from {}.", method),
                Some(e),
            )
        }
    };

    match (method, error.status, error.code.as_str()) {
        ("b2_authorize_account", 401, "bad_auth_token") => error::access_denied::<StorageError>(
            "The application key id or key were not recognized.",
            None,
        ),
        (_, 400, "bad_request") => error::internal_error::<StorageError>(&error.message, None),
        (_, 400, "invalid_bucket_id") => error::not_found::<StorageError>(path.to_owned(), None),
        (_, 400, "out_of_range") => error::internal_error::<StorageError>(&error.message, None),
        (_, 401, "unauthorized") => error::access_denied::<StorageError>(
            "The application key id or key were not recognized.",
            None,
        ),
        (_, 401, "bad_auth_token") => {
            error::access_expired::<StorageError>("The authentication token is invalid.", None)
        }
        (_, 401, "expired_auth_token") => {
            error::access_expired::<StorageError>("The authentication token has expired.", None)
        }
        (_, 401, "unsupported") => error::internal_error::<StorageError>(&error.message, None),
        (_, 503, "bad_request") => error::connection_failed::<StorageError>(&error.message, None),
        _ => error::other_error::<StorageError>(
            &format!(
                "Unknown B2 API failure {}: {}, {}",
                error.status, error.code, error.message
            ),
            None,
        ),
    }
}
