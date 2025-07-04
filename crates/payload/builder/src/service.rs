//! Support for building payloads.
//!
//! The payload builder is responsible for building payloads.
//! Once a new payload is created, it is continuously updated.

use crate::{
    metrics::PayloadBuilderServiceMetrics, traits::PayloadJobGenerator, KeepPayloadJobAlive,
    PayloadJob,
};
use alloy_consensus::BlockHeader;
use alloy_rpc_types::engine::PayloadId;
use futures_util::{future::FutureExt, Stream, StreamExt};
use reth_chain_state::CanonStateNotification;
use reth_payload_builder_primitives::{Events, PayloadBuilderError, PayloadEvents};
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes, PayloadKind, PayloadTypes};
use reth_primitives_traits::NodePrimitives;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{
    broadcast, mpsc,
    oneshot::{self, Receiver},
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, trace, warn};

type PayloadFuture<P> = Pin<Box<dyn Future<Output = Result<P, PayloadBuilderError>> + Send + Sync>>;

/// A communication channel to the [`PayloadBuilderService`] that can retrieve payloads.
///
/// This type is intended to be used to retrieve payloads from the service (e.g. from the engine
/// API).
#[derive(Debug)]
pub struct PayloadStore<T: PayloadTypes> {
    inner: Arc<PayloadBuilderHandle<T>>,
}

impl<T> PayloadStore<T>
where
    T: PayloadTypes,
{
    /// Resolves the payload job and returns the best payload that has been built so far.
    ///
    /// Note: depending on the installed [`PayloadJobGenerator`], this may or may not terminate the
    /// job, See [`PayloadJob::resolve`].
    pub async fn resolve_kind(
        &self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        self.inner.resolve_kind(id, kind).await
    }

    /// Resolves the payload job and returns the best payload that has been built so far.
    pub async fn resolve(
        &self,
        id: PayloadId,
    ) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        self.resolve_kind(id, PayloadKind::Earliest).await
    }

    /// Returns the best payload for the given identifier.
    ///
    /// Note: this merely returns the best payload so far and does not resolve the job.
    pub async fn best_payload(
        &self,
        id: PayloadId,
    ) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        self.inner.best_payload(id).await
    }

    /// Returns the payload attributes associated with the given identifier.
    ///
    /// Note: this returns the attributes of the payload and does not resolve the job.
    pub async fn payload_attributes(
        &self,
        id: PayloadId,
    ) -> Option<Result<T::PayloadBuilderAttributes, PayloadBuilderError>> {
        self.inner.payload_attributes(id).await
    }
}

impl<T> PayloadStore<T>
where
    T: PayloadTypes,
{
    /// Create a new instance
    pub fn new(inner: PayloadBuilderHandle<T>) -> Self {
        Self { inner: Arc::new(inner) }
    }
}

impl<T> From<PayloadBuilderHandle<T>> for PayloadStore<T>
where
    T: PayloadTypes,
{
    fn from(inner: PayloadBuilderHandle<T>) -> Self {
        Self::new(inner)
    }
}

/// A communication channel to the [`PayloadBuilderService`].
///
/// This is the API used to create new payloads and to get the current state of existing ones.
#[derive(Debug)]
pub struct PayloadBuilderHandle<T: PayloadTypes> {
    /// Sender half of the message channel to the [`PayloadBuilderService`].
    to_service: mpsc::UnboundedSender<PayloadServiceCommand<T>>,
}

impl<T: PayloadTypes> PayloadBuilderHandle<T> {
    /// Creates a new payload builder handle for the given channel.
    ///
    /// Note: this is only used internally by the [`PayloadBuilderService`] to manage the payload
    /// building flow See [`PayloadBuilderService::poll`] for implementation details.
    pub const fn new(to_service: mpsc::UnboundedSender<PayloadServiceCommand<T>>) -> Self {
        Self { to_service }
    }

    /// Sends a message to the service to start building a new payload for the given payload.
    ///
    /// Returns a receiver that will receive the payload id.
    pub fn send_new_payload(
        &self,
        attr: T::PayloadBuilderAttributes,
    ) -> Receiver<Result<PayloadId, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_service.send(PayloadServiceCommand::BuildNewPayload(attr, tx));
        rx
    }

    /// Returns the best payload for the given identifier.
    /// Note: this does not resolve the job if it's still in progress.
    pub async fn best_payload(
        &self,
        id: PayloadId,
    ) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        self.to_service.send(PayloadServiceCommand::BestPayload(id, tx)).ok()?;
        rx.await.ok()?
    }

    /// Resolves the payload job and returns the best payload that has been built so far.
    pub async fn resolve_kind(
        &self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        self.to_service.send(PayloadServiceCommand::Resolve(id, kind, tx)).ok()?;
        match rx.await.transpose()? {
            Ok(fut) => Some(fut.await),
            Err(e) => Some(Err(e.into())),
        }
    }

    /// Sends a message to the service to subscribe to payload events.
    /// Returns a receiver that will receive them.
    pub async fn subscribe(&self) -> Result<PayloadEvents<T>, PayloadBuilderError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_service.send(PayloadServiceCommand::Subscribe(tx));
        Ok(PayloadEvents { receiver: rx.await? })
    }

    /// Returns the payload attributes associated with the given identifier.
    ///
    /// Note: this returns the attributes of the payload and does not resolve the job.
    pub async fn payload_attributes(
        &self,
        id: PayloadId,
    ) -> Option<Result<T::PayloadBuilderAttributes, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        self.to_service.send(PayloadServiceCommand::PayloadAttributes(id, tx)).ok()?;
        rx.await.ok()?
    }
}

impl<T> Clone for PayloadBuilderHandle<T>
where
    T: PayloadTypes,
{
    fn clone(&self) -> Self {
        Self { to_service: self.to_service.clone() }
    }
}

/// A service that manages payload building tasks.
///
/// This type is an endless future that manages the building of payloads.
///
/// It tracks active payloads and their build jobs that run in a worker pool.
///
/// By design, this type relies entirely on the [`PayloadJobGenerator`] to create new payloads and
/// does know nothing about how to build them, it just drives their jobs to completion.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct PayloadBuilderService<Gen, St, T>
where
    T: PayloadTypes,
    Gen: PayloadJobGenerator,
    Gen::Job: PayloadJob<PayloadAttributes = T::PayloadBuilderAttributes>,
{
    /// The type that knows how to create new payloads.
    generator: Gen,
    /// All active payload jobs.
    payload_jobs: Vec<(Gen::Job, PayloadId)>,
    /// Copy of the sender half, so new [`PayloadBuilderHandle`] can be created on demand.
    service_tx: mpsc::UnboundedSender<PayloadServiceCommand<T>>,
    /// Receiver half of the command channel.
    command_rx: UnboundedReceiverStream<PayloadServiceCommand<T>>,
    /// Metrics for the payload builder service
    metrics: PayloadBuilderServiceMetrics,
    /// Chain events notification stream
    chain_events: St,
    /// Payload events handler, used to broadcast and subscribe to payload events.
    payload_events: broadcast::Sender<Events<T>>,
}

const PAYLOAD_EVENTS_BUFFER_SIZE: usize = 20;

// === impl PayloadBuilderService ===

impl<Gen, St, T> PayloadBuilderService<Gen, St, T>
where
    T: PayloadTypes,
    Gen: PayloadJobGenerator,
    Gen::Job: PayloadJob<PayloadAttributes = T::PayloadBuilderAttributes>,
    <Gen::Job as PayloadJob>::BuiltPayload: Into<T::BuiltPayload>,
{
    /// Creates a new payload builder service and returns the [`PayloadBuilderHandle`] to interact
    /// with it.
    ///
    /// This also takes a stream of chain events that will be forwarded to the generator to apply
    /// additional logic when new state is committed. See also
    /// [`PayloadJobGenerator::on_new_state`].
    pub fn new(generator: Gen, chain_events: St) -> (Self, PayloadBuilderHandle<T>) {
        let (service_tx, command_rx) = mpsc::unbounded_channel();
        let (payload_events, _) = broadcast::channel(PAYLOAD_EVENTS_BUFFER_SIZE);

        let service = Self {
            generator,
            payload_jobs: Vec::new(),
            service_tx,
            command_rx: UnboundedReceiverStream::new(command_rx),
            metrics: Default::default(),
            chain_events,
            payload_events,
        };

        let handle = service.handle();
        (service, handle)
    }

    /// Returns a handle to the service.
    pub fn handle(&self) -> PayloadBuilderHandle<T> {
        PayloadBuilderHandle::new(self.service_tx.clone())
    }

    /// Returns true if the given payload is currently being built.
    fn contains_payload(&self, id: PayloadId) -> bool {
        self.payload_jobs.iter().any(|(_, job_id)| *job_id == id)
    }

    /// Returns the best payload for the given identifier that has been built so far.
    fn best_payload(&self, id: PayloadId) -> Option<Result<T::BuiltPayload, PayloadBuilderError>> {
        let res = self
            .payload_jobs
            .iter()
            .find(|(_, job_id)| *job_id == id)
            .map(|(j, _)| j.best_payload().map(|p| p.into()));
        if let Some(Ok(ref best)) = res {
            self.metrics.set_best_revenue(best.block().number(), f64::from(best.fees()));
        }

        res
    }

    /// Returns the best payload for the given identifier that has been built so far and terminates
    /// the job if requested.
    fn resolve(
        &mut self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> Option<PayloadFuture<T::BuiltPayload>> {
        debug!(target: "payload_builder", %id, "resolving payload job");

        let job = self.payload_jobs.iter().position(|(_, job_id)| *job_id == id)?;
        let (fut, keep_alive) = self.payload_jobs[job].0.resolve_kind(kind);

        if keep_alive == KeepPayloadJobAlive::No {
            let (_, id) = self.payload_jobs.swap_remove(job);
            debug!(target: "payload_builder", %id, "terminated resolved job");
        }

        // Since the fees will not be known until the payload future is resolved / awaited, we wrap
        // the future in a new future that will update the metrics.
        let resolved_metrics = self.metrics.clone();
        let payload_events = self.payload_events.clone();

        let fut = async move {
            let res = fut.await;
            if let Ok(payload) = &res {
                if payload_events.receiver_count() > 0 {
                    payload_events.send(Events::BuiltPayload(payload.clone().into())).ok();
                }

                resolved_metrics
                    .set_resolved_revenue(payload.block().number(), f64::from(payload.fees()));
            }
            res.map(|p| p.into())
        };

        Some(Box::pin(fut))
    }
}

impl<Gen, St, T> PayloadBuilderService<Gen, St, T>
where
    T: PayloadTypes,
    Gen: PayloadJobGenerator,
    Gen::Job: PayloadJob<PayloadAttributes = T::PayloadBuilderAttributes>,
    <Gen::Job as PayloadJob>::BuiltPayload: Into<T::BuiltPayload>,
{
    /// Returns the payload attributes for the given payload.
    fn payload_attributes(
        &self,
        id: PayloadId,
    ) -> Option<Result<<Gen::Job as PayloadJob>::PayloadAttributes, PayloadBuilderError>> {
        let attributes = self
            .payload_jobs
            .iter()
            .find(|(_, job_id)| *job_id == id)
            .map(|(j, _)| j.payload_attributes());

        if attributes.is_none() {
            trace!(target: "payload_builder", %id, "no matching payload job found to get attributes for");
        }

        attributes
    }
}

impl<Gen, St, T, N> Future for PayloadBuilderService<Gen, St, T>
where
    T: PayloadTypes,
    N: NodePrimitives,
    Gen: PayloadJobGenerator + Unpin + 'static,
    <Gen as PayloadJobGenerator>::Job: Unpin + 'static,
    St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
    Gen::Job: PayloadJob<PayloadAttributes = T::PayloadBuilderAttributes>,
    <Gen::Job as PayloadJob>::BuiltPayload: Into<T::BuiltPayload>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            // notify the generator of new chain events
            while let Poll::Ready(Some(new_head)) = this.chain_events.poll_next_unpin(cx) {
                this.generator.on_new_state(new_head);
            }

            // we poll all jobs first, so we always have the latest payload that we can report if
            // requests
            // we don't care about the order of the jobs, so we can just swap_remove them
            for idx in (0..this.payload_jobs.len()).rev() {
                let (mut job, id) = this.payload_jobs.swap_remove(idx);

                // drain better payloads from the job
                match job.poll_unpin(cx) {
                    Poll::Ready(Ok(_)) => {
                        this.metrics.set_active_jobs(this.payload_jobs.len());
                        trace!(target: "payload_builder", %id, "payload job finished");
                    }
                    Poll::Ready(Err(err)) => {
                        warn!(target: "payload_builder",%err, ?id, "Payload builder job failed; resolving payload");
                        this.metrics.inc_failed_jobs();
                        this.metrics.set_active_jobs(this.payload_jobs.len());
                    }
                    Poll::Pending => {
                        // still pending, put it back
                        this.payload_jobs.push((job, id));
                    }
                }
            }

            // marker for exit condition
            let mut new_job = false;

            // drain all requests
            while let Poll::Ready(Some(cmd)) = this.command_rx.poll_next_unpin(cx) {
                match cmd {
                    PayloadServiceCommand::BuildNewPayload(attr, tx) => {
                        let id = attr.payload_id();
                        let mut res = Ok(id);

                        if this.contains_payload(id) {
                            debug!(target: "payload_builder",%id, parent = %attr.parent(), "Payload job already in progress, ignoring.");
                        } else {
                            // no job for this payload yet, create one
                            let parent = attr.parent();
                            match this.generator.new_payload_job(attr.clone()) {
                                Ok(job) => {
                                    info!(target: "payload_builder", %id, %parent, "New payload job created");
                                    this.metrics.inc_initiated_jobs();
                                    new_job = true;
                                    this.payload_jobs.push((job, id));
                                    this.payload_events.send(Events::Attributes(attr.clone())).ok();
                                }
                                Err(err) => {
                                    this.metrics.inc_failed_jobs();
                                    warn!(target: "payload_builder", %err, %id, "Failed to create payload builder job");
                                    res = Err(err);
                                }
                            }
                        }

                        // return the id of the payload
                        let _ = tx.send(res);
                    }
                    PayloadServiceCommand::BestPayload(id, tx) => {
                        let _ = tx.send(this.best_payload(id));
                    }
                    PayloadServiceCommand::PayloadAttributes(id, tx) => {
                        let attributes = this.payload_attributes(id);
                        let _ = tx.send(attributes);
                    }
                    PayloadServiceCommand::Resolve(id, strategy, tx) => {
                        let _ = tx.send(this.resolve(id, strategy));
                    }
                    PayloadServiceCommand::Subscribe(tx) => {
                        let new_rx = this.payload_events.subscribe();
                        let _ = tx.send(new_rx);
                    }
                }
            }

            if !new_job {
                return Poll::Pending
            }
        }
    }
}

/// Message type for the [`PayloadBuilderService`].
pub enum PayloadServiceCommand<T: PayloadTypes> {
    /// Start building a new payload.
    BuildNewPayload(
        T::PayloadBuilderAttributes,
        oneshot::Sender<Result<PayloadId, PayloadBuilderError>>,
    ),
    /// Get the best payload so far
    BestPayload(PayloadId, oneshot::Sender<Option<Result<T::BuiltPayload, PayloadBuilderError>>>),
    /// Get the payload attributes for the given payload
    PayloadAttributes(
        PayloadId,
        oneshot::Sender<Option<Result<T::PayloadBuilderAttributes, PayloadBuilderError>>>,
    ),
    /// Resolve the payload and return the payload
    Resolve(
        PayloadId,
        /* kind: */ PayloadKind,
        oneshot::Sender<Option<PayloadFuture<T::BuiltPayload>>>,
    ),
    /// Payload service events
    Subscribe(oneshot::Sender<broadcast::Receiver<Events<T>>>),
}

impl<T> fmt::Debug for PayloadServiceCommand<T>
where
    T: PayloadTypes,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuildNewPayload(f0, f1) => {
                f.debug_tuple("BuildNewPayload").field(&f0).field(&f1).finish()
            }
            Self::BestPayload(f0, f1) => {
                f.debug_tuple("BestPayload").field(&f0).field(&f1).finish()
            }
            Self::PayloadAttributes(f0, f1) => {
                f.debug_tuple("PayloadAttributes").field(&f0).field(&f1).finish()
            }
            Self::Resolve(f0, f1, _f2) => f.debug_tuple("Resolve").field(&f0).field(&f1).finish(),
            Self::Subscribe(f0) => f.debug_tuple("Subscribe").field(&f0).finish(),
        }
    }
}
