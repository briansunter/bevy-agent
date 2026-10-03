//! Shared network admission and routing; simulation is owned by one adapter.
use crate::{
    http::{
        HTTP_REQUEST_DEADLINE, HttpRequest, read_http_request_until, validate_http_rpc,
        write_http_response_until, write_preflight_response_until,
    },
    operations::{OPERATION_RESPONSE_BYTES, OperationHandle, OperationLedger},
    protocol::RpcCommand,
    rpc::{
        JsonRpcBridge, JsonRpcError, JsonRpcResponse, PreparedRequest, serialize_response,
        serialize_response_with_limit,
    },
    security::{RPC_INVALID_PARAMS, RPC_REQUEST_TIMEOUT, RPC_SERVER_BUSY},
    websocket::serve_websocket,
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Observable supervision state, also installed in Bevy apps.
#[derive(bevy::prelude::Resource, Clone, Default)]
pub struct RemoteServerStatus {
    failure: Arc<Mutex<Option<String>>>,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
    io_errors: Arc<AtomicU64>,
}
impl RemoteServerStatus {
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().expect("supervisor poisoned").clone()
    }
    pub fn check(&self) -> Result<()> {
        match self.failure() {
            Some(error) => Err(anyhow!("remote server failed: {error}")),
            None => Ok(()),
        }
    }
}

/// Fixed resource bounds for both headless and Bevy remote adapters.
#[derive(Clone, Debug)]
pub struct RemoteServerLimits {
    pub connection_workers: usize,
    pub queued_connections: usize,
    pub queued_commands: usize,
    pub retained_operations: usize,
    pub retained_operation_bytes: usize,
    pub operation_retention: Duration,
}
impl Default for RemoteServerLimits {
    fn default() -> Self {
        Self {
            connection_workers: 4,
            queued_connections: 8,
            queued_commands: 16,
            retained_operations: 256,
            retained_operation_bytes: 64 * 1024 * 1024,
            operation_retention: Duration::from_secs(300),
        }
    }
}
impl RemoteServerLimits {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(2..=64).contains(&self.connection_workers)
            || !(1..=1024).contains(&self.queued_connections)
            || !(1..=1024).contains(&self.queued_commands)
            || !(1..=65536).contains(&self.retained_operations)
            || self.retained_operation_bytes > 1024 * 1024 * 1024
            || self.operation_retention > Duration::from_secs(86400)
        {
            return Err(anyhow!(
                "remote limits require 2..64 workers, 1..1024 queued connections/commands, 1..65536 retained operations, at most1GiB of outcomes and at most24hours retention"
            ));
        }
        OperationLedger::new(
            self.retained_operations,
            self.retained_operation_bytes,
            self.operation_retention,
        )?;
        Ok(())
    }
}

pub(crate) struct RemoteCommand {
    pub(crate) prepared: PreparedRequest,
    pub(crate) operation: OperationHandle,
    pub(crate) deadline: Instant,
}
impl RemoteCommand {
    pub(crate) fn try_claim(&self) -> bool {
        if Instant::now() >= self.deadline {
            let response = serialize_response_with_limit(
                &error_response(
                    self.prepared.id.clone(),
                    RPC_REQUEST_TIMEOUT,
                    "request expired before execution".into(),
                    Some(
                        json!({"operation_id":self.operation.id(), "execution_state":"cancelled"}),
                    ),
                ),
                OPERATION_RESPONSE_BYTES,
            );
            self.operation.cancel(response);
            return false;
        }
        self.operation.try_claim()
    }
}
#[derive(Clone)]
pub(crate) struct RemoteService {
    pub(crate) bridge: JsonRpcBridge,
    sender: SyncSender<RemoteCommand>,
    pub(crate) ledger: OperationLedger,
    pub(crate) stop: Arc<AtomicBool>,
    tick: Arc<AtomicU64>,
    pub(crate) status: RemoteServerStatus,
}
impl RemoteService {
    pub(crate) fn new(
        bridge: JsonRpcBridge,
        limits: &RemoteServerLimits,
    ) -> Result<(Self, Receiver<RemoteCommand>)> {
        limits.validate()?;
        let (sender, receiver) = mpsc::sync_channel(limits.queued_commands);
        Ok((
            Self {
                bridge,
                sender,
                ledger: OperationLedger::new(
                    limits.retained_operations,
                    limits.retained_operation_bytes,
                    limits.operation_retention,
                )?,
                stop: Arc::new(AtomicBool::new(false)),
                tick: Arc::new(AtomicU64::new(0)),
                status: RemoteServerStatus::default(),
            },
            receiver,
        ))
    }
    pub(crate) fn fail(&self, message: impl Into<String>) {
        let mut failure = self.status.failure.lock().expect("supervisor poisoned");
        if failure.is_none() {
            *failure = Some(message.into());
        }
        self.stop.store(true, Ordering::Release);
        self.ledger.shutdown();
    }
    pub(crate) fn update_tick(&self, tick: u64) {
        self.tick.store(tick, Ordering::Release);
    }
    pub(crate) fn dispatch(&self, body: &str, deadline: Instant) -> String {
        let prepared = match self.bridge.prepare_request(body) {
            Ok(prepared) => prepared,
            Err(response) => return serialize_response(&response),
        };
        self.dispatch_prepared(prepared, deadline)
    }
    fn dispatch_prepared(&self, mut prepared: PreparedRequest, deadline: Instant) -> String {
        let id = prepared.id.clone();
        if let RpcCommand::OperationStatus(params) = &prepared.command {
            let response = match match (&params.operation_id, &params.retry_key) {
                (Some(id), None) => self.ledger.status(id),
                (None, Some(key)) => self.ledger.status_by_key(key),
                _ => Err(anyhow!("supply exactly one operation_id or retry_key")),
            } {
                Ok(status) => JsonRpcResponse::Result {
                    jsonrpc: "2.0",
                    id,
                    result: serde_json::to_value(status).expect("operation status is serializable"),
                },
                Err(error) => error_response(id, RPC_INVALID_PARAMS, error.to_string(), None),
            };
            return serialize_response(&response);
        }
        if self.stop.load(Ordering::Acquire) {
            return serialize_response(&error_response(
                id,
                RPC_SERVER_BUSY,
                "remote server is stopping".into(),
                None,
            ));
        }
        let identity = prepared.retry_identity();
        let (operation, is_new) = match self.ledger.admit_request(
            prepared.command.method().as_str(),
            prepared.retry_key.clone(),
            identity,
        ) {
            Ok(admitted) => admitted,
            Err(error) => {
                let code = if error.is::<crate::operations::RetryConflict>() {
                    RPC_INVALID_PARAMS
                } else {
                    RPC_SERVER_BUSY
                };
                return serialize_response(&error_response(id, code, error.to_string(), None));
            }
        };
        prepared.admitted = true;
        let operation_id = operation.id().to_owned();
        let execution_deadline = execution_deadline(deadline);
        if is_new {
            let command = RemoteCommand {
                prepared,
                operation: operation.clone(),
                deadline: execution_deadline,
            };
            if let Err(error) = self.sender.try_send(command) {
                let message = match error {
                    TrySendError::Full(_) => "simulation command queue is full",
                    TrySendError::Disconnected(_) => "simulation owner stopped",
                };
                let response = serialize_response_with_limit(
                    &error_response(
                        id,
                        RPC_SERVER_BUSY,
                        message.into(),
                        Some(json!({"operation_id":operation_id,"execution_state":"rejected"})),
                    ),
                    OPERATION_RESPONSE_BYTES,
                );
                operation.cancel(response.clone());
                return response;
            }
        }
        if let Some(response) = operation.wait_until(execution_deadline, &self.stop) {
            return if is_new {
                response
            } else {
                correlate_retry(response, id)
            };
        }
        let cancelled = serialize_response_with_limit(
            &error_response(
                id.clone(),
                RPC_REQUEST_TIMEOUT,
                "request expired before execution".into(),
                Some(json!({"operation_id":operation_id,"execution_state":"cancelled"})),
            ),
            OPERATION_RESPONSE_BYTES,
        );
        if is_new && operation.cancel(cancelled.clone()) {
            return cancelled;
        }
        // Complete may have raced the wait deadline; retrieve it before uncertainty.
        if let Some(response) = operation.wait_until(Instant::now(), &self.stop) {
            return if is_new {
                response
            } else {
                correlate_retry(response, id)
            };
        }
        serialize_response_with_limit(
            &error_response(id, RPC_REQUEST_TIMEOUT,
                "request deadline expired while execution continues; query agent.operations.status for its final outcome".into(),
                Some(json!({ "operation_id": operation_id, "execution_state": "unknown" }))),
            OPERATION_RESPONSE_BYTES,
        )
    }
    #[cfg(test)]
    fn handle_connection(&self, stream: &mut TcpStream, deadline: Instant) -> Result<()> {
        let request = read_http_request_until(stream, deadline)?;
        self.handle_request(stream, &request, deadline, None)
    }
    fn handle_request(
        &self,
        stream: &mut TcpStream,
        request: &HttpRequest,
        deadline: Instant,
        prepared: Option<std::result::Result<PreparedRequest, JsonRpcResponse>>,
    ) -> Result<()> {
        let security = &self.bridge.security;
        if request.method == "OPTIONS" {
            return write_preflight_response_until(stream, request, security, deadline);
        }
        if request.method == "GET" && request.path == "/health" {
            return write_http_response_until(
                stream,
                200,
                "OK",
                "application/json",
                &json!({"ok":self.status.failure().is_none(),"tick":self.tick.load(Ordering::Acquire),"accepted_connections":self.status.accepted.load(Ordering::Relaxed),"rejected_connections":self.status.rejected.load(Ordering::Relaxed),"io_errors":self.status.io_errors.load(Ordering::Relaxed)}).to_string(),
                Some(request),
                Some(security),
                deadline,
            );
        }
        if request.method == "GET" && request.path == "/ws" {
            return serve_websocket(stream, request, security, deadline, |body, deadline| {
                Ok(self.dispatch(&body, deadline))
            });
        }
        if request.method == "POST" && request.path == "/rpc" {
            if let Err(error) = validate_http_rpc(request, security) {
                return write_http_response_until(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    &json!({"error":error.to_string()}).to_string(),
                    Some(request),
                    Some(security),
                    deadline,
                );
            }
            let response = match prepared {
                Some(Ok(prepared)) => self.dispatch_prepared(prepared, deadline),
                Some(Err(response)) => serialize_response(&response),
                None => self.dispatch(&request.body, deadline),
            };
            return write_http_response_until(
                stream,
                200,
                "OK",
                "application/json",
                &response,
                Some(request),
                Some(security),
                deadline,
            );
        }
        write_http_response_until(
            stream,
            404,
            "Not Found",
            "application/json",
            "{\"error\":\"not found\"}",
            Some(request),
            Some(security),
            deadline,
        )
    }
}
fn correlate_retry(body: String, id: Value) -> String {
    let mut response: Value = serde_json::from_str(&body).expect("stored operation response");
    response["id"] = id.clone();
    match crate::rpc::serialize_json_bounded(&response, false) {
        Ok(bytes) => String::from_utf8(bytes).expect("JSON is UTF-8"),
        Err(error) => serialize_response(&error_response(
            id,
            crate::RPC_INTERNAL_ERROR,
            error.to_string(),
            None,
        )),
    }
}
fn execution_deadline(deadline: Instant) -> Instant {
    let reserve = deadline
        .saturating_duration_since(Instant::now())
        .div_f32(4.0)
        .min(Duration::from_millis(250));
    deadline - reserve
}
fn error_response(id: Value, code: i32, message: String, data: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::Error {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message,
            data,
        },
    }
}

struct AcceptedConnection {
    stream: TcpStream,
    deadline: Instant,
}
struct RoutedConnection {
    accepted: AcceptedConnection,
    request: HttpRequest,
    prepared: Option<std::result::Result<PreparedRequest, JsonRpcResponse>>,
}
trait ConnectionSocket {
    fn socket(&self) -> &TcpStream;
}
impl ConnectionSocket for AcceptedConnection {
    fn socket(&self) -> &TcpStream {
        &self.stream
    }
}
impl ConnectionSocket for RoutedConnection {
    fn socket(&self) -> &TcpStream {
        &self.accepted.stream
    }
}
fn reject_connection(stream: &mut TcpStream, deadline: Instant) -> Result<()> {
    write_http_response_until(
        stream,
        503,
        "Service Unavailable",
        "application/json",
        "{\"error\":\"connection queue is full\"}",
        None,
        None,
        deadline.min(Instant::now() + Duration::from_millis(100)),
    )
}
/// Owns a fixed listener/worker pool and closes every active socket on drop.
pub(crate) struct TransportHandle {
    service: RemoteService,
    sockets: Vec<Arc<Mutex<Option<TcpStream>>>>,
    listener: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}
impl TransportHandle {
    pub(crate) fn start(
        listener: Arc<TcpListener>,
        service: RemoteService,
        limits: &RemoteServerLimits,
    ) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let (sender, receiver) =
            mpsc::sync_channel::<AcceptedConnection>(limits.queued_connections);
        let (normal_sender, normal_receiver) =
            mpsc::sync_channel::<RoutedConnection>(limits.queued_connections);
        let (control_sender, control_receiver) =
            mpsc::sync_channel::<RoutedConnection>(limits.queued_connections);
        let mut handle = Self {
            service: service.clone(),
            sockets: Vec::new(),
            listener: None,
            workers: Vec::new(),
        };
        // Long WebSocket sessions and simulation waits occupy only normal workers.
        handle.spawn_pool(
            "connection",
            limits.connection_workers,
            normal_receiver,
            |service, routed| {
                service.handle_request(
                    &mut routed.accepted.stream,
                    &routed.request,
                    routed.accepted.deadline,
                    routed.prepared.take(),
                )
            },
        )?;
        handle.spawn_pool("control", 1, control_receiver, |service, routed| {
            service.handle_request(
                &mut routed.accepted.stream,
                &routed.request,
                routed.accepted.deadline,
                routed.prepared.take(),
            )
        })?;
        handle.spawn_pool("admission", 2, receiver, move |service, accepted| {
            let mut request = match read_http_request_until(&mut accepted.stream, accepted.deadline)
            {
                Ok(request) => request,
                Err(error) => {
                    return write_http_response_until(
                        &mut accepted.stream,
                        400,
                        "Bad Request",
                        "application/json",
                        &json!({"error":error.to_string()}).to_string(),
                        None,
                        None,
                        accepted.deadline,
                    );
                }
            };
            let prepared = if request.method == "POST"
                && request.path == "/rpc"
                && validate_http_rpc(&request, &service.bridge.security).is_ok()
            {
                let prepared = service.bridge.prepare_request(&request.body);
                request.body = String::new();
                Some(prepared)
            } else {
                None
            };
            let control = request.method == "OPTIONS"
                || (request.method == "GET" && request.path == "/health")
                || prepared.as_ref().is_some_and(|p| {
                    p.as_ref()
                        .is_ok_and(|p| p.method() == crate::RpcMethod::OperationStatus)
                });
            let target = if control {
                &control_sender
            } else {
                &normal_sender
            };
            let routed = RoutedConnection {
                accepted: AcceptedConnection {
                    stream: accepted.stream.try_clone()?,
                    deadline: accepted.deadline,
                },
                request,
                prepared,
            };
            if let Err(error) = target.try_send(routed) {
                match error {
                    TrySendError::Full(mut routed) => {
                        service.status.rejected.fetch_add(1, Ordering::Relaxed);
                        reject_connection(&mut routed.accepted.stream, routed.accepted.deadline)?;
                    }
                    TrySendError::Disconnected(_) => {
                        service.fail("connection worker queue disconnected")
                    }
                }
            }
            Ok(())
        })?;
        let service = service.clone();
        handle.listener = Some(
            std::thread::Builder::new()
                .name("bevy-agent-listener".into())
                .spawn(move || {
                    while !service.stop.load(Ordering::Acquire) {
                        service.ledger.maintain();
                        match listener.accept() {
                            Ok((stream, _)) => {
                                service.status.accepted.fetch_add(1, Ordering::Relaxed);
                                let accepted = AcceptedConnection {
                                    stream,
                                    deadline: Instant::now() + HTTP_REQUEST_DEADLINE,
                                };
                                if let Err(TrySendError::Full(mut connection)) =
                                    sender.try_send(accepted)
                                {
                                    service.status.rejected.fetch_add(1, Ordering::Relaxed);
                                    let response = serialize_response(&error_response(
                                        Value::Null,
                                        RPC_SERVER_BUSY,
                                        "connection queue is full".into(),
                                        None,
                                    ));
                                    let _ = write_http_response_until(
                                        &mut connection.stream,
                                        503,
                                        "Service Unavailable",
                                        "application/json",
                                        &response,
                                        None,
                                        None,
                                        Instant::now() + Duration::from_millis(100),
                                    );
                                    // Publish EOF after the complete response. Discard a
                                    // small immediately available request so normal clients
                                    // do not lose the 503 to TCP's unread-data reset on close.
                                    let _ = connection.stream.shutdown(Shutdown::Write);
                                    let _ = connection
                                        .stream
                                        .set_read_timeout(Some(Duration::from_millis(1)));
                                    let mut buffer = [0u8; 1024];
                                    let mut discarded = 0;
                                    let drain_deadline = Instant::now() + Duration::from_millis(1);
                                    while discarded < 32 * 1024 && Instant::now() < drain_deadline {
                                        match std::io::Read::read(
                                            &mut connection.stream,
                                            &mut buffer,
                                        ) {
                                            Ok(0) | Err(_) => break,
                                            Ok(count) => discarded += count,
                                        }
                                    }
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::park_timeout(Duration::from_millis(10))
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(error) => {
                                service.fail(format!("listener accept failed: {error}"));
                                break;
                            }
                        }
                    }
                })?,
        );
        Ok(handle)
    }
    fn spawn_pool<T>(
        &mut self,
        name: &str,
        count: usize,
        receiver: Receiver<T>,
        execute: impl Fn(&RemoteService, &mut T) -> Result<()> + Send + Sync + 'static,
    ) -> Result<()>
    where
        T: ConnectionSocket + Send + 'static,
    {
        let receiver = Arc::new(Mutex::new(receiver));
        let execute = Arc::new(execute);
        for index in 0..count {
            let service = self.service.clone();
            let receiver = receiver.clone();
            let execute = execute.clone();
            let socket = Arc::new(Mutex::new(None));
            self.sockets.push(socket.clone());
            self.workers.push(
                std::thread::Builder::new()
                    .name(format!("bevy-agent-{name}-{index}"))
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            while !service.stop.load(Ordering::Acquire) {
                                let received = receiver
                                    .lock()
                                    .expect("connection queue poisoned")
                                    .recv_timeout(Duration::from_millis(10));
                                let mut connection = match received {
                                    Ok(c) => c,
                                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                };
                                let tracked = match connection.socket().try_clone() {
                                    Ok(s) => s,
                                    Err(_) => {
                                        service.status.io_errors.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                };
                                *socket.lock().expect("active socket poisoned") = Some(tracked);
                                if !service.stop.load(Ordering::Acquire)
                                    && execute(&service, &mut connection).is_err()
                                {
                                    service.status.io_errors.fetch_add(1, Ordering::Relaxed);
                                }
                                *socket.lock().expect("active socket poisoned") = None;
                            }
                        }));
                        if result.is_err() {
                            service.fail("network worker panicked");
                        }
                    })?,
            );
        }
        Ok(())
    }
}
impl Drop for TransportHandle {
    fn drop(&mut self) {
        self.service.stop.store(true, Ordering::Release);
        self.service.ledger.shutdown();
        for socket in &self.sockets {
            if let Some(socket) = socket.lock().expect("active socket poisoned").as_ref() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        if let Some(listener) = self.listener.take() {
            listener.thread().unpark();
            let _ = listener.join();
        }
        for worker in self.workers.drain(..) {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    fn call(id: &str, method: &str, params: Value) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
    }
    fn setup() -> (RemoteService, Receiver<RemoteCommand>) {
        RemoteService::new(
            JsonRpcBridge::new(crate::RemoteSecurity {
                session_token: Some("secret".into()),
                ..Default::default()
            })
            .unwrap(),
            &RemoteServerLimits::default(),
        )
        .unwrap()
    }
    #[test]
    fn invalid_and_unauthenticated_calls_never_enter_queue_or_ledger() {
        let (service, receiver) = setup();
        for body in [
            call(
                "bad",
                "agent.step",
                json!({"session_token":"wrong","action":{"type":"Noop"}}),
            ),
            call(
                "invalid",
                "agent.fast_forward",
                json!({"session_token":"secret","ticks":10001}),
            ),
        ] {
            let response: Value = serde_json::from_str(
                &service.dispatch(&body, Instant::now() + Duration::from_millis(10)),
            )
            .unwrap();
            assert!(response.get("error").is_some());
        }
        assert!(receiver.try_recv().is_err());
        assert!(service.ledger.is_empty());
    }
    #[test]
    fn queued_timeout_is_correlated_cancelled_and_cannot_execute() {
        let (service, receiver) = setup();
        let response: Value = serde_json::from_str(&service.dispatch(
            &call("original", "agent.info", json!({"session_token":"secret"})),
            Instant::now() + Duration::from_millis(20),
        ))
        .unwrap();
        assert_eq!(response["id"], "original");
        assert_eq!(response["error"]["data"]["execution_state"], "cancelled");
        let command = receiver.try_recv().unwrap();
        assert!(!command.operation.try_claim());
        let status = service.ledger.status(command.operation.id()).unwrap();
        assert_eq!(status.state, crate::OperationState::Cancelled);
        assert_eq!(status.response.unwrap(), response);
    }
    #[test]
    fn running_timeout_remains_queryable_and_finishes_once_without_pump() {
        let (service, receiver) = setup();
        let request_service = service.clone();
        let caller = std::thread::spawn(move || {
            request_service.dispatch(
                &call("mutation", "agent.info", json!({"session_token":"secret"})),
                Instant::now() + Duration::from_millis(50),
            )
        });
        let command = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(command.operation.try_claim());
        let response: Value = serde_json::from_str(&caller.join().unwrap()).unwrap();
        assert_eq!(response["id"], "mutation");
        assert_eq!(response["error"]["data"]["execution_state"], "unknown");
        let id = response["error"]["data"]["operation_id"].as_str().unwrap();
        let query = |token: &str| {
            call(
                "lookup",
                "agent.operations.status",
                json!({"operation_id":id,"session_token":token}),
            )
        };
        let denied: Value = serde_json::from_str(
            &service.dispatch(&query("wrong"), Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(denied["error"]["code"], crate::RPC_AUTH_ERROR);
        let running: Value = serde_json::from_str(
            &service.dispatch(&query("secret"), Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(running["id"], "lookup");
        assert_eq!(running["result"]["state"], "running");
        assert!(receiver.try_recv().is_err());
        let final_response =
            json!({"jsonrpc":"2.0","id":"mutation","result":{"tick":7}}).to_string();
        assert!(command.operation.complete(final_response));
        assert!(!command.operation.complete("{\"result\":8}".into()));
        let final_status: Value = serde_json::from_str(
            &service.dispatch(&query("secret"), Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(final_status["result"]["state"], "completed");
        assert_eq!(final_status["result"]["response"]["id"], "mutation");
        assert_eq!(final_status["result"]["response"]["result"]["tick"], 7);
    }
    #[test]
    fn full_command_queue_rejects_promptly_with_correlated_busy_error() {
        let limits = RemoteServerLimits {
            queued_commands: 1,
            ..Default::default()
        };
        let (service, receiver) = RemoteService::new(JsonRpcBridge::default(), &limits).unwrap();
        service.dispatch(&call("one", "agent.info", json!({})), Instant::now());
        let started = Instant::now();
        let busy: Value = serde_json::from_str(&service.dispatch(
            &call("two", "agent.info", json!({})),
            Instant::now() + Duration::from_secs(1),
        ))
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(busy["id"], "two");
        assert_eq!(busy["error"]["code"], RPC_SERVER_BUSY);
        assert_eq!(busy["error"]["data"]["execution_state"], "rejected");
        assert!(!receiver.try_recv().unwrap().operation.try_claim());
        assert!(receiver.try_recv().is_err());
    }
    #[test]
    fn owner_checks_expiry_even_before_waiting_worker_cancels() {
        let (service, _) = setup();
        let prepared = service
            .bridge
            .prepare_request(&call(
                "late",
                "agent.info",
                json!({"session_token":"secret"}),
            ))
            .unwrap();
        let operation = service.ledger.admit("agent.info").unwrap();
        let command = RemoteCommand {
            prepared,
            operation: operation.clone(),
            deadline: Instant::now(),
        };
        assert!(!command.try_claim());
        let status = service.ledger.status(operation.id()).unwrap();
        assert_eq!(status.state, crate::OperationState::Cancelled);
        assert_eq!(status.response.unwrap()["id"], "late");
    }
    #[test]
    fn wire_timeouts_preserve_request_id_and_execution_uncertainty() {
        for claimed in [false, true] {
            let (service, receiver) = setup();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker_service = service.clone();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                worker_service
                    .handle_connection(&mut stream, Instant::now() + Duration::from_millis(100))
                    .unwrap();
            });
            let owner = std::thread::spawn(move || {
                let command = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                if claimed {
                    assert!(command.try_claim());
                }
                std::thread::sleep(Duration::from_millis(120));
                command.operation
            });
            let body = call("original", "agent.info", json!({"session_token":"secret"}));
            let request = format!(
                "POST /rpc HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let response = http(address, request.as_bytes());
            worker.join().unwrap();
            let operation = owner.join().unwrap();
            let value: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(value["id"], "original");
            assert_eq!(value["error"]["code"], RPC_REQUEST_TIMEOUT);
            assert_eq!(
                value["error"]["data"]["execution_state"],
                if claimed { "unknown" } else { "cancelled" }
            );
            assert_eq!(value["error"]["data"]["operation_id"], operation.id());
            if claimed {
                assert!(operation.complete(
                    json!({"jsonrpc":"2.0","id":"original","result":{"tick":3}}).to_string()
                ));
            }
            assert!(
                service
                    .ledger
                    .status(operation.id())
                    .unwrap()
                    .response
                    .is_some()
            );
        }
    }
    fn http(address: std::net::SocketAddr, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
    fn websocket(address: std::net::SocketAddr) -> TcpStream {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        assert!(
            String::from_utf8(header)
                .unwrap()
                .starts_with("HTTP/1.1 101")
        );
        stream
    }
    #[test]
    fn idle_websocket_does_not_block_health_or_authenticated_status() {
        let (service, _receiver) = setup();
        let ledger = service.ledger.clone();
        let op = ledger.admit("agent.step").unwrap();
        op.try_claim();
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        let address = listener.local_addr().unwrap();
        let transport =
            TransportHandle::start(listener, service, &RemoteServerLimits::default()).unwrap();
        let _idle = websocket(address);
        let started = Instant::now();
        assert!(http(address, b"GET /health HTTP/1.1\r\n\r\n").contains("\"ok\":true"));
        let body = call(
            "status",
            "agent.operations.status",
            json!({"session_token":"secret","operation_id":op.id()}),
        );
        let request = format!(
            "POST /rpc HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = http(address, request.as_bytes());
        let value: Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(value["result"]["state"], "running");
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(transport);
    }
    #[test]
    fn connection_saturation_rejects_promptly_and_drop_closes_all_workers() {
        let limits = RemoteServerLimits {
            connection_workers: 2,
            queued_connections: 1,
            ..Default::default()
        };
        let (service, _) = RemoteService::new(JsonRpcBridge::default(), &limits).unwrap();
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        let address = listener.local_addr().unwrap();
        let transport = TransportHandle::start(listener, service, &limits).unwrap();
        let a = websocket(address);
        let b = websocket(address);
        let queued = TcpStream::connect(address).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        let response = http(address, b"GET /health HTTP/1.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"ok\":true"));
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        drop(transport);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop((a, b, queued));
        let _listener = TcpListener::bind(address).expect("all listener clones released");
    }
    #[test]
    fn concurrent_retry_keys_execute_once_and_status_recovers_without_operation_id() {
        let (service, receiver) = setup();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let callers: Vec<_> = ["lost-response", "retry"].into_iter().map(|id| {
            let service = service.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || {
                let body = json!({"jsonrpc":"2.0","id":id,"method":"agent.step","retry_key":"episode1.tick1","params":{"session_token":"secret","action":{"type":"Noop"}}}).to_string();
                barrier.wait();
                service.dispatch(&body, Instant::now() + Duration::from_secs(2))
            })
        }).collect();
        barrier.wait();
        let command = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(command.try_claim());
        let lookup: Value = serde_json::from_str(&service.dispatch(
            &call(
                "lookup",
                "agent.operations.status",
                json!({"session_token":"secret","retry_key":"episode1.tick1"}),
            ),
            Instant::now() + Duration::from_secs(1),
        ))
        .unwrap();
        assert_eq!(lookup["result"]["state"], "running");
        assert_eq!(lookup["result"]["operation_id"], command.operation.id());
        command.operation.complete(
            json!({"jsonrpc":"2.0","id":command.prepared.id,"result":{"tick":1}}).to_string(),
        );
        for (worker, id) in callers.into_iter().zip(["lost-response", "retry"]) {
            let response: Value = serde_json::from_str(&worker.join().unwrap()).unwrap();
            assert_eq!(response["id"], id);
            assert_eq!(response["result"]["tick"], 1);
        }
        assert!(receiver.try_recv().is_err());
        let changed = json!({"jsonrpc":"2.0","id":"conflict","method":"agent.step","retry_key":"episode1.tick1","params":{"session_token":"secret","action":{"type":"Jump"}}}).to_string();
        let conflict: Value = serde_json::from_str(
            &service.dispatch(&changed, Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(conflict["error"]["code"], RPC_INVALID_PARAMS);
        assert!(receiver.try_recv().is_err());
        let unchanged = json!({"jsonrpc":"2.0","id":"after-disconnect","method":"agent.step","retry_key":"episode1.tick1","params":{"session_token":"secret","action":{"type":"Noop"}}}).to_string();
        let repeated: Value = serde_json::from_str(
            &service.dispatch(&unchanged, Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(repeated["id"], "after-disconnect");
        assert_eq!(repeated["result"]["tick"], 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn supervisor_preserves_fatal_cause_and_wakes_waiters() {
        let (service, _) = setup();
        service.fail("listener accept failed: injected failure");
        assert!(service.stop.load(Ordering::Acquire));
        assert!(
            service
                .status
                .check()
                .unwrap_err()
                .to_string()
                .contains("injected failure")
        );
        service.fail("later failure");
        assert!(
            service
                .status
                .failure()
                .unwrap()
                .contains("injected failure")
        );
        let response: Value = serde_json::from_str(&service.dispatch(
            &call(
                "after-fatal",
                "agent.info",
                json!({"session_token":"secret"}),
            ),
            Instant::now(),
        ))
        .unwrap();
        assert_eq!(response["error"]["code"], RPC_SERVER_BUSY);
    }

    #[test]
    fn health_and_status_keep_reserved_capacity_with_every_normal_worker_in_websocket() {
        let (service, receiver) = setup();
        let operation = service.ledger.admit("agent.step").unwrap();
        operation.try_claim();
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        let address = listener.local_addr().unwrap();
        let limits = RemoteServerLimits::default();
        let transport = TransportHandle::start(listener, service, &limits).unwrap();
        let sockets: Vec<_> = (0..limits.connection_workers)
            .map(|_| websocket(address))
            .collect();
        let health = http(address, b"GET /health HTTP/1.1\r\n\r\n");
        assert!(health.starts_with("HTTP/1.1 200"));
        let body = call(
            "status",
            "agent.operations.status",
            json!({"session_token":"secret","operation_id":operation.id()}),
        );
        let response = http(address, format!("POST /rpc HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
        let result: Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(result["result"]["state"], "running");
        assert!(receiver.try_recv().is_err());
        drop(transport);
        drop(sockets);
    }
}
