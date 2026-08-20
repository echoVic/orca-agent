use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

pub(crate) const ACP_MAX_INBOUND_LINE_BYTES: usize = 8_388_608;
pub(crate) const ACP_MAX_OUTBOUND_FRAME_BYTES: usize = 8_388_608;
pub(crate) const ACP_INGRESS_MESSAGE_LIMIT: usize = 64;
pub(crate) const ACP_INGRESS_BYTE_LIMIT: usize = 16_777_216;
pub(crate) const ACP_OUTGOING_MESSAGE_LIMIT: usize = 256;
pub(crate) const ACP_OUTGOING_BYTE_LIMIT: usize = 33_554_432;
pub(crate) const ACP_WRITE_FLUSH_DEADLINE_MS: u64 = 30_000;
pub(crate) const ACP_SUPERVISOR_JOIN_DEADLINE_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameDirection {
    ClientToAgent,
    AgentToClient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaneKind {
    Ingress,
    Outgoing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundedLaneBudget {
    Messages,
    Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimeoutPhase {
    WriteFlush,
    SupervisorJoin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SequenceScope {
    InboundGlobal,
    InboundSession,
    Outbound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpcFacadeError {
    Oversize {
        direction: FrameDirection,
        encoded_bytes: usize,
        limit: usize,
    },
    Direction {
        expected: FrameDirection,
        actual: FrameDirection,
    },
    Protocol {
        message: String,
    },
    SequenceExhausted {
        scope: SequenceScope,
    },
    Sealed,
    Closed {
        lane: LaneKind,
    },
    Capacity {
        lane: LaneKind,
        budget: BoundedLaneBudget,
    },
    Read {
        kind: io::ErrorKind,
        message: String,
    },
    Write {
        sequence: u64,
        kind: io::ErrorKind,
        message: String,
    },
    Flush {
        sequence: u64,
        kind: io::ErrorKind,
        message: String,
    },
    Timeout {
        phase: TimeoutPhase,
        sequence: Option<u64>,
    },
    Task {
        task: &'static str,
        message: String,
    },
}

impl fmt::Display for RpcFacadeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize {
                direction,
                encoded_bytes,
                limit,
            } => write!(
                formatter,
                "{direction:?} frame is {encoded_bytes} bytes, exceeding {limit}"
            ),
            Self::Direction { expected, actual } => {
                write!(formatter, "expected {expected:?} frame, got {actual:?}")
            }
            Self::Protocol { message } => write!(formatter, "protocol error: {message}"),
            Self::SequenceExhausted { scope } => {
                write!(formatter, "{scope:?} sequence is exhausted")
            }
            Self::Sealed => formatter.write_str("RPC facade is sealed"),
            Self::Closed { lane } => write!(formatter, "{lane:?} lane is closed"),
            Self::Capacity { lane, budget } => {
                write!(formatter, "{lane:?} lane exceeded its {budget:?} budget")
            }
            Self::Read { message, .. } => write!(formatter, "read failed: {message}"),
            Self::Write {
                sequence, message, ..
            } => write!(formatter, "write {sequence} failed: {message}"),
            Self::Flush {
                sequence, message, ..
            } => write!(formatter, "flush {sequence} failed: {message}"),
            Self::Timeout { phase, sequence } => {
                write!(formatter, "{phase:?} timed out for sequence {sequence:?}")
            }
            Self::Task { task, message } => write!(formatter, "{task} task failed: {message}"),
        }
    }
}

impl Error for RpcFacadeError {}

struct SequenceCounter {
    next: AtomicU64,
    exhausted: AtomicBool,
}

impl SequenceCounter {
    fn new(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
            exhausted: AtomicBool::new(false),
        }
    }

    fn reserve(&self, scope: SequenceScope) -> Result<u64, RpcFacadeError> {
        loop {
            if self.exhausted.load(Ordering::Acquire) {
                return Err(RpcFacadeError::SequenceExhausted { scope });
            }
            let current = self.next.load(Ordering::Acquire);
            if current == u64::MAX {
                if self
                    .exhausted
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(current);
                }
                continue;
            }
            if self
                .next
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(current);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SequenceSeeds {
    pub(crate) inbound_global: u64,
    pub(crate) inbound_session: u64,
    pub(crate) outbound: u64,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ReaderAdmissionBarrier {
    sequence: u64,
    reached: Arc<AtomicBool>,
    reached_notify: Arc<Notify>,
    released: Arc<AtomicBool>,
    released_notify: Arc<Notify>,
}

#[cfg(test)]
impl ReaderAdmissionBarrier {
    pub(crate) fn new(sequence: u64) -> Self {
        Self {
            sequence,
            reached: Arc::new(AtomicBool::new(false)),
            reached_notify: Arc::new(Notify::new()),
            released: Arc::new(AtomicBool::new(false)),
            released_notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) async fn wait_reached(&self) {
        loop {
            let notified = self.reached_notify.notified();
            if self.reached.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.released_notify.notify_waiters();
    }

    async fn before_admission(&self, sequence: u64) {
        if sequence != self.sequence {
            return;
        }
        self.reached.store(true, Ordering::Release);
        self.reached_notify.notify_waiters();
        loop {
            let notified = self.released_notify.notified();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct OutboundReservationBarrier {
    sequence: u64,
    reached: Arc<AtomicBool>,
    reached_notify: Arc<Notify>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

#[cfg(test)]
impl OutboundReservationBarrier {
    pub(crate) fn new(sequence: u64) -> Self {
        Self {
            sequence,
            reached: Arc::new(AtomicBool::new(false)),
            reached_notify: Arc::new(Notify::new()),
            release: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    pub(crate) async fn wait_reached(&self) {
        loop {
            let notified = self.reached_notify.notified();
            if self.reached.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release(&self) {
        let (released, notify) = &*self.release;
        *released
            .lock()
            .expect("outbound reservation barrier lock poisoned") = true;
        notify.notify_all();
    }

    fn after_reservation(&self, sequence: u64) {
        if sequence != self.sequence {
            return;
        }
        self.reached.store(true, Ordering::Release);
        self.reached_notify.notify_waiters();
        let (released, notify) = &*self.release;
        let mut released = released
            .lock()
            .expect("outbound reservation barrier lock poisoned");
        while !*released {
            released = notify
                .wait(released)
                .expect("outbound reservation barrier lock poisoned");
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RpcFacadeConfig {
    pub(crate) write_flush_deadline: Duration,
    pub(crate) supervisor_join_deadline: Duration,
}

impl Default for RpcFacadeConfig {
    fn default() -> Self {
        Self {
            write_flush_deadline: Duration::from_millis(ACP_WRITE_FLUSH_DEADLINE_MS),
            supervisor_join_deadline: Duration::from_millis(ACP_SUPERVISOR_JOIN_DEADLINE_MS),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TransportFrame {
    direction: FrameDirection,
    encoded: Vec<u8>,
}

impl TransportFrame {
    pub(crate) fn new(direction: FrameDirection, encoded: Vec<u8>) -> Self {
        Self { direction, encoded }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InboundFrame {
    sequence: u64,
    session_sequence: Option<u64>,
    session_id: Option<String>,
    method: Option<String>,
    encoded: Arc<[u8]>,
}

impl InboundFrame {
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn session_sequence(&self) -> Option<u64> {
        self.session_sequence
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn json_value(&self) -> Result<Value, RpcFacadeError> {
        validate_jsonrpc_frame(&self.encoded)
    }
}

pub(crate) type HandlerCompletion = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub(crate) type HandlerFuture =
    Pin<Box<dyn Future<Output = Result<HandlerCompletion, RpcFacadeError>> + Send + 'static>>;
type InboundHandler = Arc<dyn Fn(InboundFrame) -> HandlerFuture + Send + Sync>;
pub(crate) type LocalHandlerCompletion = Pin<Box<dyn Future<Output = ()> + 'static>>;
pub(crate) type LocalHandlerFuture =
    Pin<Box<dyn Future<Output = Result<LocalHandlerCompletion, RpcFacadeError>> + 'static>>;
type LocalInboundHandler = Rc<dyn Fn(InboundFrame) -> LocalHandlerFuture>;
pub(crate) type ResponseSessionResolver = Arc<dyn Fn(i64) -> Option<String> + Send + Sync>;

struct LaneState {
    admission_gate: Mutex<()>,
    messages: AtomicUsize,
    message_limit: usize,
    bytes: AtomicUsize,
    byte_limit: usize,
    sealed: AtomicBool,
}

#[derive(Clone)]
struct BoundedLaneControl {
    state: Arc<LaneState>,
}

impl BoundedLaneControl {
    fn seal(&self) {
        let _admission = self
            .state
            .admission_gate
            .lock()
            .expect("bounded lane admission lock poisoned");
        self.state.sealed.store(true, Ordering::Release);
    }
}

struct Weighted<T> {
    value: Option<T>,
    bytes: usize,
    state: Arc<LaneState>,
    released: bool,
}

impl<T> Weighted<T> {
    fn into_value(mut self) -> T {
        self.release();
        self.value.take().expect("weighted lane value present")
    }

    fn take_value(&mut self) -> T {
        self.value.take().expect("weighted lane value present")
    }

    fn release(&mut self) {
        if !self.released {
            self.state.messages.fetch_sub(1, Ordering::AcqRel);
            self.state.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
            self.released = true;
        }
    }
}

impl<T> Drop for Weighted<T> {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct BoundedSender<T> {
    sender: mpsc::Sender<Weighted<T>>,
    state: Arc<LaneState>,
    lane: LaneKind,
}

impl<T> Clone for BoundedSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            state: self.state.clone(),
            lane: self.lane,
        }
    }
}

impl<T> BoundedSender<T> {
    pub(crate) fn try_send(&self, value: T, encoded_bytes: usize) -> Result<(), RpcFacadeError> {
        self.try_send_with(encoded_bytes, || Ok((value, ())))
    }

    fn try_send_with<R>(
        &self,
        encoded_bytes: usize,
        make_value: impl FnOnce() -> Result<(T, R), RpcFacadeError>,
    ) -> Result<R, RpcFacadeError> {
        let _admission = self
            .state
            .admission_gate
            .lock()
            .expect("bounded lane admission lock poisoned");
        if self.state.sealed.load(Ordering::Acquire) {
            return Err(RpcFacadeError::Sealed);
        }
        let (value, result) = make_value()?;
        reserve_message(&self.state, self.lane)?;
        if let Err(error) = reserve_bytes(&self.state, self.lane, encoded_bytes) {
            self.state.messages.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(error) => {
                release_lane_reservation(&self.state, encoded_bytes);
                return Err(match error {
                    mpsc::error::TrySendError::Full(_) => RpcFacadeError::Capacity {
                        lane: self.lane,
                        budget: BoundedLaneBudget::Messages,
                    },
                    mpsc::error::TrySendError::Closed(_) => {
                        RpcFacadeError::Closed { lane: self.lane }
                    }
                });
            }
        };
        permit.send(Weighted {
            value: Some(value),
            bytes: encoded_bytes,
            state: self.state.clone(),
            released: false,
        });
        Ok(result)
    }

    fn seal(&self) {
        self.control().seal();
    }

    fn control(&self) -> BoundedLaneControl {
        BoundedLaneControl {
            state: self.state.clone(),
        }
    }

    #[cfg(test)]
    fn admission_gate_is_held_for_test(&self) -> bool {
        match self.state.admission_gate.try_lock() {
            Ok(admission) => {
                drop(admission);
                false
            }
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("bounded lane admission lock poisoned")
            }
        }
    }
}

pub(crate) struct BoundedReceiver<T> {
    receiver: mpsc::Receiver<Weighted<T>>,
}

impl<T> BoundedReceiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await.map(Weighted::into_value)
    }

    async fn recv_held(&mut self) -> Option<Weighted<T>> {
        self.receiver.recv().await
    }

    fn close(&mut self) {
        self.receiver.close();
    }
}

pub(crate) fn bounded_lane<T>(
    lane: LaneKind,
    message_limit: usize,
    byte_limit: usize,
) -> (BoundedSender<T>, BoundedReceiver<T>) {
    assert!(
        message_limit > 0,
        "bounded lane message limit must be non-zero"
    );
    let (sender, receiver) = mpsc::channel(message_limit);
    let state = Arc::new(LaneState {
        admission_gate: Mutex::new(()),
        messages: AtomicUsize::new(0),
        message_limit,
        bytes: AtomicUsize::new(0),
        byte_limit,
        sealed: AtomicBool::new(false),
    });
    (
        BoundedSender {
            sender,
            state,
            lane,
        },
        BoundedReceiver { receiver },
    )
}

fn reserve_message(state: &LaneState, lane: LaneKind) -> Result<(), RpcFacadeError> {
    let mut current = state.messages.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(1) else {
            return Err(RpcFacadeError::Capacity {
                lane,
                budget: BoundedLaneBudget::Messages,
            });
        };
        if next > state.message_limit {
            return Err(RpcFacadeError::Capacity {
                lane,
                budget: BoundedLaneBudget::Messages,
            });
        }
        match state.messages.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn reserve_bytes(
    state: &LaneState,
    lane: LaneKind,
    encoded_bytes: usize,
) -> Result<(), RpcFacadeError> {
    let mut current = state.bytes.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(encoded_bytes) else {
            return Err(RpcFacadeError::Capacity {
                lane,
                budget: BoundedLaneBudget::Bytes,
            });
        };
        if next > state.byte_limit {
            return Err(RpcFacadeError::Capacity {
                lane,
                budget: BoundedLaneBudget::Bytes,
            });
        }
        match state
            .bytes
            .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn release_lane_reservation(state: &LaneState, encoded_bytes: usize) {
    state.messages.fetch_sub(1, Ordering::AcqRel);
    state.bytes.fetch_sub(encoded_bytes, Ordering::AcqRel);
}

struct ConnectionState {
    sealed: AtomicBool,
    outbound_sequence: SequenceCounter,
    transport_failure: Mutex<Option<RpcFacadeError>>,
    transport_failure_tx: mpsc::Sender<RpcFacadeError>,
    cleanup_complete: AtomicBool,
    cleanup_notify: Notify,
}

impl ConnectionState {
    fn fail_transport(
        &self,
        error: RpcFacadeError,
        ingress: &BoundedLaneControl,
        outgoing: &BoundedSender<OutboundRequest>,
    ) {
        let mut failure = self
            .transport_failure
            .lock()
            .expect("transport failure lock poisoned");
        if failure.is_some() {
            return;
        }
        *failure = Some(error.clone());
        self.sealed.store(true, Ordering::Release);
        ingress.seal();
        outgoing.seal();
        let _ = self.transport_failure_tx.try_send(error);
    }

    fn transport_failure(&self) -> Option<RpcFacadeError> {
        self.transport_failure
            .lock()
            .expect("transport failure lock poisoned")
            .clone()
    }

    fn mark_cleanup_complete(&self) {
        self.cleanup_complete.store(true, Ordering::Release);
        self.cleanup_notify.notify_waiters();
    }

    async fn wait_cleanup_complete(&self) {
        loop {
            let notified = self.cleanup_notify.notified();
            if self.cleanup_complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct CleanupCompleteGuard(Arc<ConnectionState>);

impl Drop for CleanupCompleteGuard {
    fn drop(&mut self) {
        self.0.mark_cleanup_complete();
    }
}

struct OutboundRequest {
    sequence: u64,
    encoded: Vec<u8>,
    acknowledgement: oneshot::Sender<Result<WriteAck, RpcFacadeError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WriteAck {
    pub(crate) sequence: u64,
    pub(crate) encoded_bytes: usize,
}

pub(crate) struct WriteReceipt {
    sequence: u64,
    acknowledgement: oneshot::Receiver<Result<WriteAck, RpcFacadeError>>,
}

impl WriteReceipt {
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) async fn ack(self) -> Result<WriteAck, RpcFacadeError> {
        self.acknowledgement
            .await
            .unwrap_or(Err(RpcFacadeError::Sealed))
    }
}

#[derive(Clone)]
pub(crate) struct RpcFacadeHandle {
    ingress: BoundedLaneControl,
    outgoing: BoundedSender<OutboundRequest>,
    state: Arc<ConnectionState>,
    #[cfg(test)]
    outbound_reservation_barrier: Option<OutboundReservationBarrier>,
}

impl RpcFacadeHandle {
    pub(crate) fn enqueue(&self, frame: TransportFrame) -> Result<WriteReceipt, RpcFacadeError> {
        if self.state.sealed.load(Ordering::Acquire) {
            return Err(RpcFacadeError::Sealed);
        }
        if frame.direction != FrameDirection::AgentToClient {
            return Err(RpcFacadeError::Direction {
                expected: FrameDirection::AgentToClient,
                actual: frame.direction,
            });
        }
        if frame.encoded.len() > ACP_MAX_OUTBOUND_FRAME_BYTES {
            let error = RpcFacadeError::Oversize {
                direction: FrameDirection::AgentToClient,
                encoded_bytes: frame.encoded.len(),
                limit: ACP_MAX_OUTBOUND_FRAME_BYTES,
            };
            self.state
                .fail_transport(error.clone(), &self.ingress, &self.outgoing);
            return Err(error);
        }
        validate_jsonrpc_frame(&frame.encoded)?;

        let encoded_bytes = frame.encoded.len();
        let result = self.outgoing.try_send_with(encoded_bytes, || {
            let sequence = self
                .state
                .outbound_sequence
                .reserve(SequenceScope::Outbound)?;
            #[cfg(test)]
            if let Some(barrier) = &self.outbound_reservation_barrier {
                barrier.after_reservation(sequence);
            }
            let (acknowledgement, receiver) = oneshot::channel();
            Ok((
                OutboundRequest {
                    sequence,
                    encoded: frame.encoded,
                    acknowledgement,
                },
                WriteReceipt {
                    sequence,
                    acknowledgement: receiver,
                },
            ))
        });
        match result {
            Err(
                error @ RpcFacadeError::SequenceExhausted {
                    scope: SequenceScope::Outbound,
                },
            )
            | Err(
                error @ RpcFacadeError::Capacity {
                    lane: LaneKind::Outgoing,
                    ..
                },
            ) => {
                self.state
                    .fail_transport(error.clone(), &self.ingress, &self.outgoing);
                Err(error)
            }
            result => result,
        }
    }

    pub(crate) async fn wait_closed(&self) {
        self.state.wait_cleanup_complete().await;
    }

    #[cfg(test)]
    pub(crate) fn outgoing_admission_gate_is_held_for_test(&self) -> bool {
        self.outgoing.admission_gate_is_held_for_test()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShutdownReport {
    pub(crate) eof: bool,
    pub(crate) reader_joined: bool,
    pub(crate) scheduler_joined: bool,
    pub(crate) writer_joined: bool,
}

pub(crate) struct RpcSupervisor {
    shutdown: CancellationToken,
    join: Option<JoinHandle<Result<ShutdownReport, RpcFacadeError>>>,
}

impl RpcSupervisor {
    pub(crate) async fn wait(mut self) -> Result<ShutdownReport, RpcFacadeError> {
        let join = self
            .join
            .take()
            .expect("RPC supervisor join handle present");
        join_result(join, "supervisor").await?
    }

    pub(crate) async fn shutdown(self) -> Result<ShutdownReport, RpcFacadeError> {
        self.shutdown.cancel();
        self.wait().await
    }
}

impl Drop for RpcSupervisor {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct ReaderExit {
    eof: bool,
    error: Option<RpcFacadeError>,
}

struct SessionGate {
    next: AtomicU64,
    notify: Notify,
}

impl SessionGate {
    fn new(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
            notify: Notify::new(),
        }
    }

    async fn enter(self: &Arc<Self>, sequence: u64) -> SessionTurn {
        loop {
            let notified = self.notify.notified();
            if self.next.load(Ordering::Acquire) == sequence {
                return SessionTurn { gate: self.clone() };
            }
            notified.await;
        }
    }
}

struct SessionTurn {
    gate: Arc<SessionGate>,
}

impl Drop for SessionTurn {
    fn drop(&mut self) {
        self.gate.next.fetch_add(1, Ordering::AcqRel);
        self.gate.notify.notify_waiters();
    }
}

pub(crate) fn spawn_rpc_facade<R, W>(
    reader: R,
    writer: W,
    handler: InboundHandler,
    config: RpcFacadeConfig,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_rpc_facade_seeded(
        reader,
        writer,
        handler,
        config,
        SequenceSeeds::default(),
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
}

/// Starts the same bounded facade on a [`tokio::task::LocalSet`].
///
/// ACP 0.10.4 exposes `?Send` handler futures, so the production adapter uses
/// this entry point while retaining the same reader, writer, budgets,
/// acknowledgements and joined shutdown as the `Send` test facade.
pub(crate) fn spawn_local_rpc_facade<R, W>(
    reader: R,
    writer: W,
    handler: LocalInboundHandler,
    config: RpcFacadeConfig,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_local_rpc_facade_inner(reader, writer, handler, None, config)
}

pub(crate) fn spawn_local_rpc_facade_with_response_session_resolver<R, W>(
    reader: R,
    writer: W,
    handler: LocalInboundHandler,
    response_session_resolver: ResponseSessionResolver,
    config: RpcFacadeConfig,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_local_rpc_facade_inner(
        reader,
        writer,
        handler,
        Some(response_session_resolver),
        config,
    )
}

fn spawn_local_rpc_facade_inner<R, W>(
    reader: R,
    writer: W,
    handler: LocalInboundHandler,
    response_session_resolver: Option<ResponseSessionResolver>,
    config: RpcFacadeConfig,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (ingress_tx, ingress_rx) = bounded_lane(
        LaneKind::Ingress,
        ACP_INGRESS_MESSAGE_LIMIT,
        ACP_INGRESS_BYTE_LIMIT,
    );
    let (outgoing_tx, outgoing_rx) = bounded_lane(
        LaneKind::Outgoing,
        ACP_OUTGOING_MESSAGE_LIMIT,
        ACP_OUTGOING_BYTE_LIMIT,
    );
    let (transport_failure_tx, transport_failure_rx) = mpsc::channel(1);
    let ingress_control = ingress_tx.control();
    let state = Arc::new(ConnectionState {
        sealed: AtomicBool::new(false),
        outbound_sequence: SequenceCounter::new(0),
        transport_failure: Mutex::new(None),
        transport_failure_tx,
        cleanup_complete: AtomicBool::new(false),
        cleanup_notify: Notify::new(),
    });
    let shutdown = CancellationToken::new();
    let reader_cancel = CancellationToken::new();
    let writer_cancel = CancellationToken::new();
    let handler_cancel = CancellationToken::new();

    let reader_join = tokio::spawn(reader_loop(
        reader,
        ingress_tx.clone(),
        state.clone(),
        reader_cancel.clone(),
        0,
        0,
        response_session_resolver,
        #[cfg(test)]
        None,
    ));
    let scheduler_join = tokio::task::spawn_local(local_scheduler_loop(
        ingress_rx,
        handler,
        handler_cancel.clone(),
    ));
    let writer_join = tokio::spawn(writer_loop(
        writer,
        outgoing_rx,
        writer_cancel.clone(),
        config.write_flush_deadline,
    ));

    let coordinator_state = state.clone();
    let coordinator_outgoing = outgoing_tx.clone();
    let coordinator_shutdown = shutdown.clone();
    let join = tokio::task::spawn_local(async move {
        let _cleanup_complete = CleanupCompleteGuard(coordinator_state.clone());
        supervise(
            reader_join,
            scheduler_join,
            writer_join,
            ingress_tx,
            coordinator_outgoing,
            coordinator_state,
            coordinator_shutdown,
            transport_failure_rx,
            reader_cancel,
            writer_cancel,
            handler_cancel,
            config.supervisor_join_deadline,
        )
        .await
    });

    (
        RpcFacadeHandle {
            ingress: ingress_control,
            outgoing: outgoing_tx,
            state,
            #[cfg(test)]
            outbound_reservation_barrier: None,
        },
        RpcSupervisor {
            shutdown,
            join: Some(join),
        },
    )
}

#[cfg(test)]
pub(crate) fn spawn_rpc_facade_with_sequence_seeds<R, W>(
    reader: R,
    writer: W,
    handler: InboundHandler,
    config: RpcFacadeConfig,
    seeds: SequenceSeeds,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_rpc_facade_seeded(reader, writer, handler, config, seeds, None, None)
}

#[cfg(test)]
pub(crate) fn spawn_rpc_facade_with_sequence_seeds_and_reader_admission_barrier<R, W>(
    reader: R,
    writer: W,
    handler: InboundHandler,
    config: RpcFacadeConfig,
    seeds: SequenceSeeds,
    reader_admission_barrier: ReaderAdmissionBarrier,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_rpc_facade_seeded(
        reader,
        writer,
        handler,
        config,
        seeds,
        Some(reader_admission_barrier),
        None,
    )
}

#[cfg(test)]
pub(crate) fn spawn_rpc_facade_with_sequence_seeds_and_outbound_reservation_barrier<R, W>(
    reader: R,
    writer: W,
    handler: InboundHandler,
    config: RpcFacadeConfig,
    seeds: SequenceSeeds,
    outbound_reservation_barrier: OutboundReservationBarrier,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_rpc_facade_seeded(
        reader,
        writer,
        handler,
        config,
        seeds,
        None,
        Some(outbound_reservation_barrier),
    )
}

fn spawn_rpc_facade_seeded<R, W>(
    reader: R,
    writer: W,
    handler: InboundHandler,
    config: RpcFacadeConfig,
    seeds: SequenceSeeds,
    #[cfg(test)] reader_admission_barrier: Option<ReaderAdmissionBarrier>,
    #[cfg(test)] outbound_reservation_barrier: Option<OutboundReservationBarrier>,
) -> (RpcFacadeHandle, RpcSupervisor)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (ingress_tx, ingress_rx) = bounded_lane(
        LaneKind::Ingress,
        ACP_INGRESS_MESSAGE_LIMIT,
        ACP_INGRESS_BYTE_LIMIT,
    );
    let (outgoing_tx, outgoing_rx) = bounded_lane(
        LaneKind::Outgoing,
        ACP_OUTGOING_MESSAGE_LIMIT,
        ACP_OUTGOING_BYTE_LIMIT,
    );
    let (transport_failure_tx, transport_failure_rx) = mpsc::channel(1);
    let ingress_control = ingress_tx.control();
    let state = Arc::new(ConnectionState {
        sealed: AtomicBool::new(false),
        outbound_sequence: SequenceCounter::new(seeds.outbound),
        transport_failure: Mutex::new(None),
        transport_failure_tx,
        cleanup_complete: AtomicBool::new(false),
        cleanup_notify: Notify::new(),
    });
    let shutdown = CancellationToken::new();
    let reader_cancel = CancellationToken::new();
    let writer_cancel = CancellationToken::new();
    let handler_cancel = CancellationToken::new();

    let reader_join = tokio::spawn(reader_loop(
        reader,
        ingress_tx.clone(),
        state.clone(),
        reader_cancel.clone(),
        seeds.inbound_global,
        seeds.inbound_session,
        None,
        #[cfg(test)]
        reader_admission_barrier,
    ));
    let scheduler_join = tokio::spawn(scheduler_loop(ingress_rx, handler, handler_cancel.clone()));
    let writer_join = tokio::spawn(writer_loop(
        writer,
        outgoing_rx,
        writer_cancel.clone(),
        config.write_flush_deadline,
    ));

    let coordinator_state = state.clone();
    let coordinator_outgoing = outgoing_tx.clone();
    let coordinator_shutdown = shutdown.clone();
    let join = tokio::spawn(async move {
        let _cleanup_complete = CleanupCompleteGuard(coordinator_state.clone());
        supervise(
            reader_join,
            scheduler_join,
            writer_join,
            ingress_tx,
            coordinator_outgoing,
            coordinator_state,
            coordinator_shutdown,
            transport_failure_rx,
            reader_cancel,
            writer_cancel,
            handler_cancel,
            config.supervisor_join_deadline,
        )
        .await
    });

    (
        RpcFacadeHandle {
            ingress: ingress_control,
            outgoing: outgoing_tx,
            state,
            #[cfg(test)]
            outbound_reservation_barrier,
        },
        RpcSupervisor {
            shutdown,
            join: Some(join),
        },
    )
}

async fn reader_loop<R>(
    reader: R,
    ingress: BoundedSender<InboundFrame>,
    state: Arc<ConnectionState>,
    cancel: CancellationToken,
    global_sequence_seed: u64,
    session_sequence_seed: u64,
    response_session_resolver: Option<ResponseSessionResolver>,
    #[cfg(test)] reader_admission_barrier: Option<ReaderAdmissionBarrier>,
) -> ReaderExit
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let global_sequence = SequenceCounter::new(global_sequence_seed);
    let mut session_sequences = HashMap::<String, SequenceCounter>::new();
    loop {
        let read = tokio::select! {
            _ = cancel.cancelled() => {
                state.sealed.store(true, Ordering::Release);
                ingress.seal();
                return ReaderExit { eof: false, error: None };
            }
            read = read_bounded_line(&mut reader) => read,
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                state.sealed.store(true, Ordering::Release);
                ingress.seal();
                return ReaderExit {
                    eof: false,
                    error: Some(RpcFacadeError::Read {
                        kind: error.kind(),
                        message: error.to_string(),
                    }),
                };
            }
        };
        let encoded = match read {
            BoundedLine::Eof => {
                state.sealed.store(true, Ordering::Release);
                ingress.seal();
                return ReaderExit {
                    eof: true,
                    error: None,
                };
            }
            BoundedLine::Frame(encoded) => encoded,
            BoundedLine::Oversize(encoded_bytes) => {
                state.sealed.store(true, Ordering::Release);
                ingress.seal();
                return ReaderExit {
                    eof: false,
                    error: Some(RpcFacadeError::Oversize {
                        direction: FrameDirection::ClientToAgent,
                        encoded_bytes,
                        limit: ACP_MAX_INBOUND_LINE_BYTES,
                    }),
                };
            }
        };
        let parsed = match parse_inbound_frame(
            encoded,
            &global_sequence,
            session_sequence_seed,
            &mut session_sequences,
            response_session_resolver.as_deref(),
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                state.sealed.store(true, Ordering::Release);
                ingress.seal();
                return ReaderExit {
                    eof: false,
                    error: Some(error),
                };
            }
        };
        let encoded_bytes = parsed.encoded.len();
        #[cfg(test)]
        if let Some(barrier) = &reader_admission_barrier {
            barrier.before_admission(parsed.sequence()).await;
        }
        if let Err(error) = ingress.try_send(parsed, encoded_bytes) {
            state.sealed.store(true, Ordering::Release);
            ingress.seal();
            let error = (error != RpcFacadeError::Sealed).then_some(error);
            return ReaderExit { eof: false, error };
        }
    }
}

enum BoundedLine {
    Eof,
    Frame(Vec<u8>),
    Oversize(usize),
}

async fn read_bounded_line<R>(reader: &mut R) -> io::Result<BoundedLine>
where
    R: AsyncBufRead + Unpin,
{
    let mut encoded = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if encoded.is_empty() {
                Ok(BoundedLine::Eof)
            } else {
                Ok(BoundedLine::Frame(encoded))
            };
        }
        let delimiter = available.iter().position(|byte| *byte == b'\n');
        let take = delimiter.map_or(available.len(), |position| position + 1);
        let observed_bytes = encoded.len().saturating_add(take);
        if observed_bytes > ACP_MAX_INBOUND_LINE_BYTES {
            return Ok(BoundedLine::Oversize(observed_bytes));
        }
        encoded.extend_from_slice(&available[..take]);
        reader.consume(take);
        if delimiter.is_some() {
            return Ok(BoundedLine::Frame(encoded));
        }
    }
}

fn parse_inbound_frame(
    encoded: Vec<u8>,
    global_sequence: &SequenceCounter,
    session_sequence_seed: u64,
    session_sequences: &mut HashMap<String, SequenceCounter>,
    response_session_resolver: Option<&(dyn Fn(i64) -> Option<String> + Send + Sync)>,
) -> Result<InboundFrame, RpcFacadeError> {
    let value = validate_jsonrpc_frame(&encoded)?;
    let sequence = global_sequence.reserve(SequenceScope::InboundGlobal)?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let session_id = value
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| params.get("sessionId").or_else(|| params.get("session_id")))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            method
                .is_none()
                .then(|| value.get("id").and_then(Value::as_i64))
                .flatten()
                .and_then(|request_id| {
                    response_session_resolver.and_then(|resolver| resolver(request_id))
                })
        });
    let session_sequence = session_id
        .as_ref()
        .map(|session_id| {
            session_sequences
                .entry(session_id.clone())
                .or_insert_with(|| SequenceCounter::new(session_sequence_seed))
                .reserve(SequenceScope::InboundSession)
        })
        .transpose()?;
    Ok(InboundFrame {
        sequence,
        session_sequence,
        session_id,
        method,
        encoded: encoded.into(),
    })
}

fn validate_jsonrpc_frame(encoded: &[u8]) -> Result<Value, RpcFacadeError> {
    if encoded.last() != Some(&b'\n') {
        return Err(RpcFacadeError::Protocol {
            message: "frame is not newline terminated".to_owned(),
        });
    }
    let value: Value = serde_json::from_slice(&encoded[..encoded.len() - 1]).map_err(|error| {
        RpcFacadeError::Protocol {
            message: error.to_string(),
        }
    })?;
    let object = value.as_object().ok_or_else(|| RpcFacadeError::Protocol {
        message: "JSON-RPC frame must be an object".to_owned(),
    })?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcFacadeError::Protocol {
            message: "JSON-RPC version must be 2.0".to_owned(),
        });
    }
    let request_or_notification = object.get("method").and_then(Value::as_str).is_some();
    let response =
        object.contains_key("id") && (object.contains_key("result") ^ object.contains_key("error"));
    if !request_or_notification && !response {
        return Err(RpcFacadeError::Protocol {
            message: "frame is neither a request, notification, nor response".to_owned(),
        });
    }
    Ok(value)
}

async fn scheduler_loop(
    mut ingress: BoundedReceiver<InboundFrame>,
    handler: InboundHandler,
    cancel: CancellationToken,
) -> Result<(), RpcFacadeError> {
    let mut gates = HashMap::<String, Arc<SessionGate>>::new();
    let mut handlers =
        FuturesUnordered::<Pin<Box<dyn Future<Output = Result<(), RpcFacadeError>> + Send>>>::new();
    let mut ingress_open = true;
    loop {
        if !ingress_open && handlers.is_empty() {
            return Ok(());
        }
        tokio::select! {
            held = ingress.recv_held(), if ingress_open => {
                let Some(mut held) = held else {
                    ingress_open = false;
                    continue;
                };
                let frame = held.take_value();
                let gate = frame.session_id.as_ref().map(|session_id| {
                    let first_sequence = frame.session_sequence.unwrap_or(0);
                    gates
                        .entry(session_id.clone())
                        .or_insert_with(|| Arc::new(SessionGate::new(first_sequence)))
                        .clone()
                });
                let handler = handler.clone();
                let handler_cancel = cancel.clone();
                handlers.push(Box::pin(async move {
                    let turn = match (gate, frame.session_sequence) {
                        (Some(gate), Some(sequence)) => Some(gate.enter(sequence).await),
                        _ => None,
                    };
                    let completion = tokio::select! {
                        _ = handler_cancel.cancelled() => return Ok(()),
                        admission = handler(frame) => admission?,
                    };
                    drop(turn);
                    tokio::select! {
                        _ = handler_cancel.cancelled() => {}
                        _ = completion => {}
                    }
                    drop(held);
                    Ok(())
                }));
            }
            result = handlers.next(), if !handlers.is_empty() => {
                result.expect("handler future present")?;
            }
        }
    }
}

async fn local_scheduler_loop(
    mut ingress: BoundedReceiver<InboundFrame>,
    handler: LocalInboundHandler,
    cancel: CancellationToken,
) -> Result<(), RpcFacadeError> {
    let mut gates = HashMap::<String, Arc<SessionGate>>::new();
    let mut handlers =
        FuturesUnordered::<Pin<Box<dyn Future<Output = Result<(), RpcFacadeError>>>>>::new();
    let mut ingress_open = true;
    loop {
        if !ingress_open && handlers.is_empty() {
            return Ok(());
        }
        tokio::select! {
            held = ingress.recv_held(), if ingress_open => {
                let Some(mut held) = held else {
                    ingress_open = false;
                    continue;
                };
                let frame = held.take_value();
                let gate = frame.session_id.as_ref().map(|session_id| {
                    let first_sequence = frame.session_sequence.unwrap_or(0);
                    gates
                        .entry(session_id.clone())
                        .or_insert_with(|| Arc::new(SessionGate::new(first_sequence)))
                        .clone()
                });
                let handler = handler.clone();
                let handler_cancel = cancel.clone();
                handlers.push(Box::pin(async move {
                    let turn = match (gate, frame.session_sequence) {
                        (Some(gate), Some(sequence)) => Some(gate.enter(sequence).await),
                        _ => None,
                    };
                    let completion = tokio::select! {
                        _ = handler_cancel.cancelled() => return Ok(()),
                        admission = handler(frame) => admission?,
                    };
                    drop(turn);
                    tokio::select! {
                        _ = handler_cancel.cancelled() => {}
                        _ = completion => {}
                    }
                    drop(held);
                    Ok(())
                }));
            }
            result = handlers.next(), if !handlers.is_empty() => {
                result.expect("handler future present")?;
            }
        }
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut outgoing: BoundedReceiver<OutboundRequest>,
    cancel: CancellationToken,
    write_flush_deadline: Duration,
) -> Result<(), RpcFacadeError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let request = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                fail_pending_outbound(&mut outgoing, RpcFacadeError::Sealed).await;
                return Ok(());
            }
            request = outgoing.recv_held() => request,
        };
        let Some(mut held) = request else {
            return Ok(());
        };
        let request = held.take_value();
        let sequence = request.sequence;
        let encoded_bytes = request.encoded.len();
        let write_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(RpcFacadeError::Sealed),
            result = timeout(write_flush_deadline, write_and_flush(&mut writer, sequence, &request.encoded)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err(RpcFacadeError::Timeout {
                        phase: TimeoutPhase::WriteFlush,
                        sequence: Some(sequence),
                    }),
                }
            }
        };
        match write_result {
            Ok(()) => {
                let _ = request.acknowledgement.send(Ok(WriteAck {
                    sequence,
                    encoded_bytes,
                }));
                drop(held);
            }
            Err(error) => {
                let _ = request.acknowledgement.send(Err(error.clone()));
                drop(held);
                fail_pending_outbound(&mut outgoing, RpcFacadeError::Sealed).await;
                if error != RpcFacadeError::Sealed {
                    return Err(error);
                }
                return Ok(());
            }
        }
    }
}

async fn write_and_flush<W>(
    writer: &mut W,
    sequence: u64,
    encoded: &[u8],
) -> Result<(), RpcFacadeError>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(encoded)
        .await
        .map_err(|error| RpcFacadeError::Write {
            sequence,
            kind: error.kind(),
            message: error.to_string(),
        })?;
    writer.flush().await.map_err(|error| RpcFacadeError::Flush {
        sequence,
        kind: error.kind(),
        message: error.to_string(),
    })
}

async fn fail_pending_outbound(
    outgoing: &mut BoundedReceiver<OutboundRequest>,
    error: RpcFacadeError,
) {
    outgoing.close();
    while let Some(mut held) = outgoing.recv_held().await {
        let request = held.take_value();
        let _ = request.acknowledgement.send(Err(error.clone()));
        drop(held);
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    reader: JoinHandle<ReaderExit>,
    scheduler: JoinHandle<Result<(), RpcFacadeError>>,
    writer: JoinHandle<Result<(), RpcFacadeError>>,
    ingress: BoundedSender<InboundFrame>,
    outgoing: BoundedSender<OutboundRequest>,
    state: Arc<ConnectionState>,
    shutdown: CancellationToken,
    mut transport_failure: mpsc::Receiver<RpcFacadeError>,
    reader_cancel: CancellationToken,
    writer_cancel: CancellationToken,
    handler_cancel: CancellationToken,
    join_deadline: Duration,
) -> Result<ShutdownReport, RpcFacadeError> {
    enum Trigger {
        Reader,
        Scheduler,
        Writer,
        Shutdown,
        TransportFailure(RpcFacadeError),
    }

    let mut reader = Some(reader);
    let mut scheduler = Some(scheduler);
    let mut writer = Some(writer);
    let mut reader_observed = None;
    let mut scheduler_observed = None;
    let mut writer_observed = None;
    let trigger = tokio::select! {
        result = reader.as_mut().expect("reader task present") => {
            reader_observed = Some(result);
            Trigger::Reader
        },
        result = scheduler.as_mut().expect("scheduler task present") => {
            scheduler_observed = Some(result);
            Trigger::Scheduler
        },
        result = writer.as_mut().expect("writer task present") => {
            writer_observed = Some(result);
            Trigger::Writer
        },
        error = transport_failure.recv() => {
            Trigger::TransportFailure(error.expect("transport failure sender present"))
        },
        _ = shutdown.cancelled() => Trigger::Shutdown,
    };
    if reader_observed.is_some() {
        drop(reader.take());
    }
    if scheduler_observed.is_some() {
        drop(scheduler.take());
    }
    if writer_observed.is_some() {
        drop(writer.take());
    }
    state.sealed.store(true, Ordering::Release);
    ingress.seal();
    outgoing.seal();
    reader_cancel.cancel();
    handler_cancel.cancel();
    writer_cancel.cancel();
    drop(ingress);
    let deadline = Instant::now() + join_deadline;

    let (reader_result, scheduler_result, writer_result) = tokio::join!(
        settle_task(reader, reader_observed, "reader", deadline),
        settle_task(scheduler, scheduler_observed, "scheduler", deadline),
        settle_task(writer, writer_observed, "writer", deadline),
    );
    let reader_error = match &reader_result {
        Ok(exit) => exit.error.clone(),
        Err(error) => Some(error.clone()),
    };
    let scheduler_error = scheduler_result.as_ref().err().cloned().or_else(|| {
        scheduler_result
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().err().cloned())
    });
    let writer_error = writer_result.as_ref().err().cloned().or_else(|| {
        writer_result
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().err().cloned())
    });
    let transport_failure_error = state.transport_failure();
    let trigger_error = match trigger {
        Trigger::Reader => reader_error.clone(),
        Trigger::Scheduler => scheduler_error.clone(),
        Trigger::Writer => writer_error.clone(),
        Trigger::Shutdown => None,
        Trigger::TransportFailure(error) => Some(error),
    };
    if let Some(error) = trigger_error
        .or(transport_failure_error)
        .or(reader_error)
        .or(scheduler_error)
        .or(writer_error)
    {
        return Err(error);
    }
    Ok(ShutdownReport {
        eof: reader_result.as_ref().is_ok_and(|exit| exit.eof),
        reader_joined: true,
        scheduler_joined: true,
        writer_joined: true,
    })
}

async fn settle_task<T>(
    join: Option<JoinHandle<T>>,
    observed: Option<Result<T, tokio::task::JoinError>>,
    task: &'static str,
    deadline: Instant,
) -> Result<T, RpcFacadeError> {
    if let Some(result) = observed {
        return map_task_result(result, task);
    }
    let mut join = join.expect("unobserved task handle present");
    match timeout_at(deadline, &mut join).await {
        Ok(result) => map_task_result(result, task),
        Err(_) => {
            join.abort();
            let _ = join.await;
            Err(RpcFacadeError::Timeout {
                phase: TimeoutPhase::SupervisorJoin,
                sequence: None,
            })
        }
    }
}

fn map_task_result<T>(
    result: Result<T, tokio::task::JoinError>,
    task: &'static str,
) -> Result<T, RpcFacadeError> {
    result.map_err(|error| RpcFacadeError::Task {
        task,
        message: error.to_string(),
    })
}

async fn join_result<T>(join: JoinHandle<T>, task: &'static str) -> Result<T, RpcFacadeError> {
    join.await.map_err(|error| RpcFacadeError::Task {
        task,
        message: error.to_string(),
    })
}
