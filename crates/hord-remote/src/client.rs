//! [`RemoteRepo`] and [`open_cache`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use hord_api::proto::repo_backend_client::RepoBackendClient;
use hord_api::{
    ApiError, ApiResult, EventStream, MAX_BATCH_BYTES, MAX_BATCH_IDS, MAX_MESSAGE_BYTES,
    RepoBackend, proto, wire,
};
use hord_core::ObjectId;
use hord_txn::{ObjectSource, Repo, RepoOptions};
use tokio_stream::StreamExt;
use tonic::transport::Endpoint;

use crate::Error;
use crate::transport::{Transport, connect_local_io, split_url};

/// A repository on a `hord serve` server, as a [`RepoBackend`] and an
/// [`ObjectSource`]. Cheap to clone; clones share the connection.
#[derive(Clone, Debug)]
pub struct RemoteRepo {
    client: RepoBackendClient<Transport>,
    transport: Transport,
    handle: tokio::runtime::Handle,
    address: Arc<str>,
}

impl RemoteRepo {
    /// Connect to `url` (`http://host:port`, or `http://host:port/r/<name>`
    /// for one repository of a `--root` server). Must be called within a
    /// tokio runtime; object reads through [`ObjectSource`] run on it.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let (origin, prefix) = split_url(url).ok_or_else(|| Error::InvalidUrl(url.to_owned()))?;
        let endpoint =
            Endpoint::from_shared(origin).map_err(|_| Error::InvalidUrl(url.to_owned()))?;
        let channel = endpoint.connect().await.map_err(|source| Error::Connect {
            url: url.to_owned(),
            source,
        })?;
        Ok(Self::from_transport(Transport::new(channel, prefix), url))
    }

    /// Connect to `url` as [`Self::connect`] does, sending `token` as a
    /// bearer token on every call (spec §10.5.4).
    pub async fn connect_with_token(url: &str, token: &str) -> Result<Self, Error> {
        let remote = Self::connect(url).await?;
        let transport = remote
            .transport
            .with_token(token)
            .ok_or(Error::InvalidToken)?;
        Ok(Self::from_transport(transport, url))
    }

    /// Connect to the daemon serving the repository at `repo_root` on its
    /// local endpoint ([`hord_api::local::endpoint`], ADR 0021).
    pub async fn connect_local(repo_root: &Path) -> Result<Self, Error> {
        let name = hord_api::local::endpoint(repo_root)
            .map_err(|err| Error::InvalidUrl(format!("{}: {err}", repo_root.display())))?;
        Self::connect_endpoint(&name).await
    }

    /// Connect to a local endpoint by name: a Unix socket path, or a pipe
    /// name on Windows.
    async fn connect_endpoint(name: &str) -> Result<Self, Error> {
        let target = name.to_owned();
        // The URI is required by the API but unused: the connector ignores it.
        let channel = Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_: http::Uri| {
                connect_local_io(target.clone())
            }))
            .await
            .map_err(|source| Error::Connect {
                url: name.to_owned(),
                source,
            })?;
        Ok(Self::from_transport(Transport::new(channel, None), name))
    }

    fn from_transport(transport: Transport, address: &str) -> Self {
        let client = RepoBackendClient::new(transport.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        Self {
            client,
            transport,
            handle: tokio::runtime::Handle::current(),
            address: Arc::from(address),
        }
    }

    /// The `Auth` service on the same connection (spec §10.5.4).
    #[must_use]
    pub fn auth(&self) -> crate::RemoteAuth {
        crate::RemoteAuth::new(self.transport.clone())
    }

    /// The `Workspaces` service on the same connection: served by a
    /// repository's daemon only (ADR 0024 amendment).
    #[must_use]
    pub fn workspaces(&self) -> crate::RemoteWorkspaces {
        crate::RemoteWorkspaces::new(self.transport.clone())
    }

    /// The read-only `Changes` service (ADR 0030) on the same connection.
    #[must_use]
    pub fn changes(&self) -> crate::RemoteChanges {
        crate::RemoteChanges::new(self.transport.clone())
    }

    /// The address this connected to.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Fetch any number of objects, in order: batches of at most
    /// [`MAX_BATCH_IDS`] ids, and the streaming RPC for a batch the server
    /// finds too large.
    pub async fn fetch(&self, ids: &[ObjectId]) -> ApiResult<Vec<proto::Object>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_BATCH_IDS) {
            let request = proto::GetObjectsRequest {
                ids: chunk.iter().copied().map(wire::id).collect(),
            };
            match RepoBackend::get_objects(self, request.clone()).await {
                Ok(batch) => out.extend(batch.objects),
                Err(ApiError::ResourceExhausted(_)) => {
                    let mut stream = self
                        .client
                        .clone()
                        .stream_objects(request)
                        .await
                        .map_err(ApiError::from)?
                        .into_inner();
                    while let Some(object) = stream.next().await {
                        out.push(object.map_err(ApiError::from)?);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }

    /// Store any number of objects: batches within [`MAX_BATCH_IDS`] and
    /// [`MAX_BATCH_BYTES`] (an object larger than that goes alone).
    pub async fn store(&self, objects: Vec<proto::Object>) -> ApiResult<()> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        for object in objects {
            if !batch.is_empty()
                && (batch.len() == MAX_BATCH_IDS || bytes + object.cbor.len() > MAX_BATCH_BYTES)
            {
                self.put_objects(proto::PutObjectsRequest {
                    objects: std::mem::take(&mut batch),
                })
                .await?;
                bytes = 0;
            }
            bytes += object.cbor.len();
            batch.push(object);
        }
        if !batch.is_empty() {
            self.put_objects(proto::PutObjectsRequest { objects: batch })
                .await?;
        }
        Ok(())
    }

    /// Which of `ids` the server has, any number at a time.
    pub async fn has_all(&self, ids: &[ObjectId]) -> ApiResult<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_BATCH_IDS) {
            let request = proto::HasRequest {
                ids: chunk.iter().copied().map(wire::id).collect(),
            };
            let reply = RepoBackend::has(self, request).await?;
            out.extend(reply.present);
        }
        Ok(out)
    }

    /// Run `future` to completion from a blocking thread.
    fn block<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.handle.block_on(future)
    }
}

/// A [`hord_txn::Error`] for a failed remote call: a missing object keeps
/// its meaning, anything else is I/O (transient).
fn txn_error(err: ApiError, missing: Option<ObjectId>) -> hord_txn::Error {
    match (err, missing) {
        (ApiError::NotFound(_), Some(id)) => {
            hord_txn::Error::Store(hord_store::Error::MissingObject(id))
        }
        (err, _) => hord_txn::Error::Io(std::io::Error::other(format!("remote: {err}"))),
    }
}

/// Reads block: `hord-txn` calls them on tokio's blocking pool, never from
/// an async task (which would panic).
impl ObjectSource for RemoteRepo {
    fn get_objects(&self, ids: &[ObjectId]) -> hord_txn::Result<Vec<Vec<u8>>> {
        match self.block(self.fetch(ids)) {
            Ok(objects) => Ok(objects.into_iter().map(|o| o.cbor).collect()),
            Err(err @ ApiError::NotFound(_)) => {
                let present = self
                    .block(self.has_all(ids))
                    .map_err(|e| txn_error(e, None))?;
                let missing = ids.iter().zip(present).find(|(_, p)| !p).map(|(id, _)| *id);
                Err(txn_error(err, missing))
            }
            Err(err) => Err(txn_error(err, None)),
        }
    }

    fn has(&self, ids: &[ObjectId]) -> hord_txn::Result<Vec<bool>> {
        self.block(self.has_all(ids))
            .map_err(|e| txn_error(e, None))
    }
}

#[async_trait]
impl RepoBackend for RemoteRepo {
    async fn get_objects(
        &self,
        request: proto::GetObjectsRequest,
    ) -> ApiResult<proto::GetObjectsResponse> {
        call!(self, get_objects, request)
    }

    async fn put_objects(
        &self,
        request: proto::PutObjectsRequest,
    ) -> ApiResult<proto::PutObjectsResponse> {
        call!(self, put_objects, request)
    }

    async fn has(&self, request: proto::HasRequest) -> ApiResult<proto::HasResponse> {
        call!(self, has, request)
    }

    async fn head(&self, request: proto::HeadRequest) -> ApiResult<proto::HeadResponse> {
        call!(self, head, request)
    }

    async fn log(&self, request: proto::LogQuery) -> ApiResult<proto::LogPage> {
        call!(self, log, request)
    }

    async fn refs(&self, request: proto::RefsRequest) -> ApiResult<proto::RefsResponse> {
        call!(self, refs, request)
    }

    async fn submit(&self, request: proto::SubmitRequest) -> ApiResult<proto::SubmitResponse> {
        call!(self, submit, request)
    }

    async fn queue(&self, request: proto::QueueQuery) -> ApiResult<proto::QueueResponse> {
        call!(self, queue, request)
    }

    async fn arbitrate(
        &self,
        request: proto::ArbitrateRequest,
    ) -> ApiResult<proto::ArbitrateResponse> {
        call!(self, arbitrate, request)
    }

    async fn node_history(
        &self,
        request: proto::NodeHistoryRequest,
    ) -> ApiResult<proto::NodeHistoryResponse> {
        call!(self, node_history, request)
    }

    async fn edges(&self, request: proto::EdgesRequest) -> ApiResult<proto::EdgesResponse> {
        call!(self, edges, request)
    }

    async fn resolve_name(
        &self,
        request: proto::ResolveNameRequest,
    ) -> ApiResult<proto::ResolveNameResponse> {
        call!(self, resolve_name, request)
    }

    async fn attach_evidence(
        &self,
        request: proto::AttachEvidenceRequest,
    ) -> ApiResult<proto::AttachEvidenceResponse> {
        call!(self, attach_evidence, request)
    }

    async fn events(&self, request: proto::EventsRequest) -> ApiResult<EventStream> {
        let stream = call!(self, events, request)?;
        Ok(Box::pin(stream.map(|item| item.map_err(ApiError::from))))
    }
}

/// Open the client-side repository at `root` (created if missing) whose
/// store is `remote`'s local object cache: reads the store lacks go to
/// `remote` and are kept (spec §8.3). `options.objects` is replaced.
pub async fn open_cache(
    root: &Path,
    remote: RemoteRepo,
    options: RepoOptions,
) -> hord_txn::Result<Repo> {
    let options = RepoOptions {
        objects: Some(Arc::new(remote)),
        ..options
    };
    if root.join(hord_store::HORD_DIR).is_dir() {
        Repo::open_with(root, options).await
    } else {
        Repo::create_with(root, options).await
    }
}
