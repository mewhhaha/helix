use crate::{
    jsonrpc,
    lsp::{self, notification::Notification as _},
    Error, LanguageServerId, Result,
};
use anyhow::Context;
use helix_core::Rope;
use log::{error, info};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
    },
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{
        mpsc::{
            channel, error::SendError, unbounded_channel, Receiver, Sender, UnboundedReceiver,
            UnboundedSender,
        },
        Notify,
    },
};

#[derive(Debug)]
pub enum Payload {
    Request {
        pending: Option<PendingRequest>,
        value: QueuedRequest,
    },
    Notification(jsonrpc::Notification),
    Response(jsonrpc::Output),
    FullDocumentChange(Arc<Mutex<Option<FullDocumentChange>>>),
    CancelRequest {
        id: jsonrpc::Id,
        was_sent: bool,
    },
    Disconnected,
}

#[derive(Debug)]
pub struct PendingRequest {
    response: Sender<Result<jsonrpc::ResponseValue>>,
    canceled: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct FullDocumentChange {
    document: lsp::VersionedTextDocumentIdentifier,
    text: Rope,
}

type PendingRequests = Arc<Mutex<HashMap<jsonrpc::Id, Sender<Result<jsonrpc::ResponseValue>>>>>;
type QueuedRequest = Arc<Mutex<Option<jsonrpc::MethodCall>>>;
type CoalescedChanges = HashMap<lsp::Url, std::sync::Weak<Mutex<Option<FullDocumentChange>>>>;

/// An ordered outbox that retains one unsent FULL-sync snapshot per document
/// within a burst of changes. Regular messages are ordering barriers: snapshots
/// before requests, saves and document open/close notifications remain frozen.
/// Thus typing does not retain every intermediate revision, while a queue with
/// arbitrarily many barriers still requires separate snapshots for each segment.
#[derive(Clone, Debug)]
pub struct OutboundSender {
    tx: UnboundedSender<Payload>,
    inject_tx: UnboundedSender<Payload>,
    changes: Arc<Mutex<CoalescedChanges>>,
    pending_requests: PendingRequests,
    disconnected: Arc<AtomicBool>,
}

impl OutboundSender {
    fn new(
        tx: UnboundedSender<Payload>,
        inject_tx: UnboundedSender<Payload>,
        pending_requests: PendingRequests,
        disconnected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            inject_tx,
            changes: Arc::default(),
            pending_requests,
            disconnected,
        }
    }

    pub fn send(&self, payload: Payload) -> std::result::Result<(), SendError<Payload>> {
        // Hold this lock through enqueueing so concurrent senders cannot merge a
        // change into the segment before this message after the barrier is sent.
        let mut changes = self.changes.lock();
        changes.clear();
        if self.disconnected.load(Ordering::Acquire) {
            return Err(SendError(payload));
        }
        self.tx.send(payload)
    }

    pub fn full_document_change(
        &self,
        document: lsp::VersionedTextDocumentIdentifier,
        text: Rope,
    ) -> Result<()> {
        let mut changes = self.changes.lock();
        if self.disconnected.load(Ordering::Acquire) || self.tx.is_closed() {
            return Err(Error::StreamClosed);
        }
        if let Some(change) = changes
            .get(&document.uri)
            .and_then(std::sync::Weak::upgrade)
        {
            let mut snapshot = change.lock();
            if let Some(snapshot) = &mut *snapshot {
                *snapshot = FullDocumentChange { document, text };
                return Ok(());
            }
        }
        let uri = document.uri.clone();
        let change = Arc::new(Mutex::new(Some(FullDocumentChange { document, text })));
        self.tx
            .send(Payload::FullDocumentChange(change.clone()))
            .map_err(|_| Error::StreamClosed)?;
        // Retain only weak references: dequeuing a packet releases its snapshot
        // and allows the next update to start a new unsent packet.
        changes.insert(uri, Arc::downgrade(&change));
        Ok(())
    }

    pub fn request(
        &self,
        value: jsonrpc::MethodCall,
    ) -> Result<(Receiver<Result<jsonrpc::ResponseValue>>, RequestGuard)> {
        let id = value.id.clone();
        let queued = Arc::new(Mutex::new(Some(value)));
        let (response, receiver) = channel(1);
        let canceled = Arc::new(AtomicBool::new(false));
        self.send(Payload::Request {
            pending: Some(PendingRequest {
                response,
                canceled: canceled.clone(),
            }),
            value: queued.clone(),
        })
        .map_err(|_| Error::StreamClosed)?;
        Ok((
            receiver,
            RequestGuard {
                id,
                sender: self.clone(),
                canceled,
                queued,
                completed: false,
            },
        ))
    }
}

/// Cancel even when the returned future was never polled. Cancellation is
/// recorded before taking the pending-request lock, so registration cannot miss
/// a cancellation that arrived just before a request was written.
pub struct RequestGuard {
    id: jsonrpc::Id,
    sender: OutboundSender,
    canceled: Arc<AtomicBool>,
    queued: QueuedRequest,
    completed: bool,
}

impl RequestGuard {
    pub fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.canceled.store(true, Ordering::Release);
        // The writer may be stalled on an earlier frame. Release this request's
        // parameters immediately, leaving only its lightweight ordered token.
        self.queued.lock().take();
        let was_sent = self
            .sender
            .pending_requests
            .lock()
            .remove(&self.id)
            .is_some();
        // This also wakes the writer to release canceled pre-initialization
        // requests. The wire cancellation is sent only for registered requests.
        let _ = self.sender.inject_tx.send(Payload::CancelRequest {
            id: self.id.clone(),
            was_sent,
        });
    }
}

impl Payload {
    fn is_canceled(&self) -> bool {
        match self {
            Self::Request { pending, value } => {
                pending.as_ref().is_some_and(|request| {
                    request.canceled.load(Ordering::Acquire) || request.response.is_closed()
                }) || value.lock().is_none()
            }
            _ => false,
        }
    }
}

fn full_document_change_json(change: FullDocumentChange) -> Result<String> {
    // collect_str streams the Rope's display output into the JSON serializer,
    // avoiding an intermediate full-document String before JSON escaping.
    struct RopeText<'a>(&'a Rope);
    impl Serialize for RopeText<'_> {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            serializer.collect_str(self.0)
        }
    }
    #[derive(Serialize)]
    struct Change<'a> {
        text: RopeText<'a>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        text_document: lsp::VersionedTextDocumentIdentifier,
        content_changes: [Change<'a>; 1],
    }
    #[derive(Serialize)]
    struct Notification<'a> {
        jsonrpc: jsonrpc::Version,
        method: &'static str,
        params: Params<'a>,
    }
    Ok(serde_json::to_string(&Notification {
        jsonrpc: jsonrpc::Version::V2,
        method: "textDocument/didChange",
        params: Params {
            text_document: change.document,
            content_changes: [Change {
                text: RopeText(&change.text),
            }],
        },
    })?)
}

/// A type representing all possible values sent from the server to the client.
#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum ServerMessage {
    /// A regular JSON-RPC request output (single response).
    Output(jsonrpc::Output),
    /// A JSON-RPC request or notification.
    Call(jsonrpc::Call),
}

/// Inspect the envelope once; untagged deserialization otherwise builds a DOM
/// and retries its payload for every response/request variant.
#[derive(Deserialize)]
struct Envelope<'a> {
    jsonrpc: Option<jsonrpc::Version>,
    method: Option<String>,
    #[serde(default, deserialize_with = "present")]
    id: Option<jsonrpc::Id>,
    #[serde(default, borrow, deserialize_with = "present")]
    params: Option<sonic_rs::LazyValue<'a>>,
    #[serde(default, borrow, deserialize_with = "present")]
    result: Option<sonic_rs::LazyValue<'a>>,
    error: Option<jsonrpc::Error>,
}

// JSON null is a present ID/result, unlike an omitted field.
fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn decode_server_message(content: &[u8]) -> Result<ServerMessage> {
    let envelope: Envelope<'_> = match sonic_rs::from_slice(content) {
        Ok(envelope) => envelope,
        // Preserve the permissive invalid-request/id salvage behavior for
        // malformed envelopes and nonconforming servers.
        Err(_) => return sonic_rs::from_slice(content).map_err(Into::into),
    };
    if let Some(id) = envelope.id.clone() {
        if let Some(error) = envelope.error {
            return Ok(ServerMessage::Output(jsonrpc::Output::Failure(
                jsonrpc::Failure {
                    jsonrpc: envelope.jsonrpc,
                    id,
                    error,
                },
            )));
        }
        if let Some(result) = envelope.result {
            return Ok(ServerMessage::Output(jsonrpc::Output::Success(
                jsonrpc::Success {
                    jsonrpc: envelope.jsonrpc,
                    id,
                    result: jsonrpc::ResponseValue::Raw(jsonrpc::RawJson::new(
                        result.as_raw_str(),
                    )?),
                },
            )));
        }
    }
    let Some(method) = envelope.method else {
        return sonic_rs::from_slice(content).map_err(Into::into);
    };
    let params = match envelope.params.as_ref().map(|params| params.as_raw_str()) {
        None | Some("null") => jsonrpc::Params::None,
        Some(raw) if matches!(raw.as_bytes().first(), Some(b'{' | b'[')) => {
            jsonrpc::Params::Raw(jsonrpc::RawJson::new(raw)?)
        }
        _ => return sonic_rs::from_slice(content).map_err(Into::into),
    };
    Ok(ServerMessage::Call(match envelope.id {
        Some(id) => jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
            jsonrpc: envelope.jsonrpc,
            id,
            method,
            params,
        }),
        None => jsonrpc::Call::Notification(jsonrpc::Notification {
            jsonrpc: envelope.jsonrpc,
            method,
            params,
        }),
    }))
}

#[derive(Debug)]
pub struct Transport {
    id: LanguageServerId,
    name: String,
    pending_requests: PendingRequests,
    disconnected: Arc<AtomicBool>,
    disconnect_notify: Notify,
    shutdown_requested: AtomicBool,
    inject_tx: UnboundedSender<Payload>,
    /// Notified once the `exit` notification has been flushed to the server's stdin
    shutdown_flushed: Arc<Notify>,
}

impl Transport {
    #[allow(clippy::type_complexity)]
    pub fn start(
        server_stdout: BufReader<ChildStdout>,
        server_stdin: BufWriter<ChildStdin>,
        server_stderr: BufReader<ChildStderr>,
        id: LanguageServerId,
        name: String,
    ) -> (
        UnboundedReceiver<(LanguageServerId, jsonrpc::Call)>,
        OutboundSender,
        Arc<Notify>,
        Arc<Notify>,
    ) {
        let (client_tx, rx) = unbounded_channel();
        let (tx, client_rx) = unbounded_channel();
        let (inject_tx, inject_rx) = unbounded_channel();
        let notify = Arc::new(Notify::new());
        let shutdown_flushed = Arc::new(Notify::new());

        let transport = Self {
            id,
            name,
            pending_requests: Arc::default(),
            disconnected: Arc::default(),
            disconnect_notify: Notify::new(),
            shutdown_requested: AtomicBool::new(false),
            inject_tx,
            shutdown_flushed: shutdown_flushed.clone(),
        };

        let transport = Arc::new(transport);
        let tx = OutboundSender::new(
            tx,
            transport.inject_tx.clone(),
            transport.pending_requests.clone(),
            transport.disconnected.clone(),
        );

        tokio::spawn(Self::recv(
            transport.clone(),
            server_stdout,
            client_tx.clone(),
        ));
        tokio::spawn(Self::err(transport.clone(), server_stderr));
        tokio::spawn(Self::send(
            transport,
            server_stdin,
            client_tx,
            client_rx,
            inject_rx,
            notify.clone(),
        ));

        (rx, tx, notify, shutdown_flushed)
    }

    async fn recv_server_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        content: &mut Vec<u8>,
        language_server_name: &str,
    ) -> Result<ServerMessage> {
        if !helix_stdx::protocol::read_frame(reader, buffer, content).await? {
            return Err(Error::StreamClosed);
        }
        let msg = std::str::from_utf8(content).context("invalid utf8 from server")?;

        info!("{language_server_name} <- {msg}");

        // NOTE: We avoid using `?` here, since it would return early on error
        // and skip clearing `content`. By returning the result directly instead,
        // we ensure `content.clear()` is always called.
        let output = decode_server_message(content);

        content.clear();

        output
    }

    async fn recv_server_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        language_server_name: &str,
    ) -> Result<()> {
        buffer.truncate(0);
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        };
        error!("{language_server_name} err <- {buffer:?}");

        Ok(())
    }

    async fn send_payload_to_server(
        &self,
        server_stdin: &mut BufWriter<impl AsyncWrite + Unpin + Send>,
        payload: Payload,
    ) -> Result<()> {
        // Only a confirmed disconnect may interrupt a frame. Request
        // cancellation must let the current frame finish before its notification
        // is written, otherwise the byte stream would become invalid.
        let disconnected = self.disconnect_notify.notified();
        tokio::pin!(disconnected);
        if self.disconnected.load(Ordering::Acquire) {
            return Err(Error::StreamClosed);
        }
        tokio::select! {
            biased;
            result = self.send_payload(server_stdin, payload) => result,
            _ = &mut disconnected => Err(Error::StreamClosed),
        }
    }

    async fn send_payload(
        &self,
        server_stdin: &mut BufWriter<impl AsyncWrite + Unpin + Send>,
        payload: Payload,
    ) -> Result<()> {
        //TODO: reuse string
        let json = match payload {
            Payload::Request { pending, value } => {
                let Some(value) = value.lock().take() else {
                    return Ok(());
                };
                if let Some(request) = pending {
                    let mut requests = self.pending_requests.lock();
                    if self.disconnected.load(Ordering::Acquire) {
                        let _ = request.response.try_send(Err(Error::StreamClosed));
                        return Ok(());
                    }
                    if request.canceled.load(Ordering::Acquire) || request.response.is_closed() {
                        return Ok(());
                    }
                    requests.insert(value.id.clone(), request.response);
                }
                serde_json::to_string(&value)?
            }
            Payload::Notification(value) => serde_json::to_string(&value)?,
            Payload::Response(error) => serde_json::to_string(&error)?,
            Payload::FullDocumentChange(change) => {
                let snapshot = change.lock().take();
                let Some(snapshot) = snapshot else {
                    return Ok(());
                };
                tokio::task::spawn_blocking(move || full_document_change_json(snapshot))
                    .await
                    .map_err(|err| Error::Other(err.into()))??
            }
            Payload::CancelRequest { id, was_sent } => {
                if !was_sent || self.disconnected.load(Ordering::Acquire) {
                    return Ok(());
                }
                serde_json::to_string(&jsonrpc::Notification {
                    jsonrpc: Some(jsonrpc::Version::V2),
                    method: "$/cancelRequest".into(),
                    params: jsonrpc::Params::Map(serde_json::Map::from_iter([(
                        "id".into(),
                        serde_json::to_value(id)?,
                    )])),
                })?
            }
            Payload::Disconnected => return Ok(()),
        };
        self.send_string_to_server(server_stdin, json, &self.name)
            .await
    }

    async fn send_string_to_server(
        &self,
        server_stdin: &mut BufWriter<impl AsyncWrite + Unpin + Send>,
        request: String,
        language_server_name: &str,
    ) -> Result<()> {
        info!("{language_server_name} -> {request}");

        // send the headers
        server_stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", request.len()).as_bytes())
            .await?;

        // send the body
        server_stdin.write_all(request.as_bytes()).await?;

        server_stdin.flush().await?;

        Ok(())
    }

    async fn process_server_message(
        &self,
        client_tx: &UnboundedSender<(LanguageServerId, jsonrpc::Call)>,
        msg: ServerMessage,
        language_server_name: &str,
    ) -> Result<()> {
        match msg {
            ServerMessage::Output(output) => {
                self.process_request_response(output, language_server_name)
                    .await?
            }
            ServerMessage::Call(jsonrpc::Call::MethodCall(ref method_call))
                if self.shutdown_requested.load(Ordering::Acquire) =>
            {
                // After helix sends shutdown the application event loop is no longer
                // consuming server-to-client requests. Respond with null success so the
                // server is not left waiting for a reply before it sends the shutdown
                // response. Sending an error is intentionally avoided: servers based on
                // vscode-languageserver-node (including gopls) treat an error response to
                // client/registerCapability as fatal and abort rather than completing the
                // handshake.
                let _ = self
                    .inject_tx
                    .send(Payload::Response(jsonrpc::Output::Success(
                        jsonrpc::Success {
                            jsonrpc: Some(jsonrpc::Version::V2),
                            id: method_call.id.clone(),
                            result: serde_json::Value::Null.into(),
                        },
                    )));
            }
            ServerMessage::Call(call) => {
                client_tx
                    .send((self.id, call))
                    .context("failed to send a message to server")?;
            }
        };
        Ok(())
    }

    async fn process_request_response(
        &self,
        output: jsonrpc::Output,
        language_server_name: &str,
    ) -> Result<()> {
        let (id, result) = match output {
            jsonrpc::Output::Success(jsonrpc::Success { id, result, .. }) => (id, Ok(result)),
            jsonrpc::Output::Failure(jsonrpc::Failure { id, error, .. }) => {
                error!("{language_server_name} <- {error}");
                (id, Err(error.into()))
            }
        };

        let tx = self.pending_requests.lock().remove(&id);
        if let Some(tx) = tx {
            match tx.send(result).await {
                Ok(_) => (),
                Err(_) => log::debug!(
                    "Tried sending response into a closed channel (id={:?}), likely a fire-and-forget shutdown",
                    id
                ),
            };
        } else {
            log::debug!(
                "Discarding late or untracked Language Server response (id={:?}) {:?}",
                id,
                result
            );
        }

        Ok(())
    }

    fn disconnect(&self) {
        if self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        let requests = std::mem::take(&mut *self.pending_requests.lock());
        for (_, response) in requests {
            let _ = response.try_send(Err(Error::StreamClosed));
        }
        self.disconnect_notify.notify_waiters();
        let _ = self.inject_tx.send(Payload::Disconnected);
    }

    async fn recv(
        transport: Arc<Self>,
        mut server_stdout: BufReader<impl AsyncRead + Unpin + Send>,
        client_tx: UnboundedSender<(LanguageServerId, jsonrpc::Call)>,
    ) {
        let mut recv_buffer = String::new();
        let mut content_buffer = Vec::new();
        loop {
            match Self::recv_server_message(
                &mut server_stdout,
                &mut recv_buffer,
                &mut content_buffer,
                &transport.name,
            )
            .await
            {
                Ok(msg) => {
                    match transport
                        .process_server_message(&client_tx, msg, &transport.name)
                        .await
                    {
                        Ok(_) => {}
                        Err(err) => {
                            error!("{} err: <- {err:?}", transport.name);
                            break;
                        }
                    };
                }
                Err(err) => {
                    if !matches!(err, Error::StreamClosed) {
                        error!(
                            "Exiting {} after unexpected error: {err:?}",
                            &transport.name
                        );
                    }

                    transport.disconnect();

                    // Hack: inject a terminated notification so we trigger code that needs to happen after exit
                    let notification =
                        ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                            jsonrpc: None,
                            method: lsp::notification::Exit::METHOD.to_string(),
                            params: jsonrpc::Params::None,
                        }));
                    match transport
                        .process_server_message(&client_tx, notification, &transport.name)
                        .await
                    {
                        Ok(_) => {}
                        Err(err) => {
                            error!("err: <- {:?}", err);
                        }
                    }
                    break;
                }
            }
        }
        transport.disconnect();
    }

    async fn err(transport: Arc<Self>, mut server_stderr: BufReader<ChildStderr>) {
        let mut recv_buffer = String::new();
        loop {
            match Self::recv_server_error(&mut server_stderr, &mut recv_buffer, &transport.name)
                .await
            {
                Ok(_) => {}
                Err(err) => {
                    error!("{} err: <- {err:?}", transport.name);
                    break;
                }
            }
        }
    }

    async fn send(
        transport: Arc<Self>,
        mut server_stdin: BufWriter<impl AsyncWrite + Unpin + Send>,
        client_tx: UnboundedSender<(LanguageServerId, jsonrpc::Call)>,
        mut client_rx: UnboundedReceiver<Payload>,
        mut inject_rx: UnboundedReceiver<Payload>,
        initialize_notify: Arc<Notify>,
    ) {
        let mut pending_messages: Vec<Payload> = Vec::new();
        let mut is_pending = true;

        // Pin outside the loop to avoid cancellation-safety issue:
        // recreating `notified()` inside `select!` can lose the permit.
        let notified = initialize_notify.notified();
        tokio::pin!(notified);

        // Servers may ask the client questions during initialization. Their
        // responses must be sent immediately so initialization can complete.
        fn allowed_before_initialize(payload: &Payload) -> bool {
            use lsp::{
                notification::Initialized,
                request::{Initialize, Request},
            };
            match payload {
                Payload::Request { value, .. } => value
                    .lock()
                    .as_ref()
                    .is_some_and(|value| value.method == Initialize::METHOD),
                Payload::Notification(jsonrpc::Notification { method, .. })
                    if method == Initialized::METHOD =>
                {
                    true
                }
                Payload::Response(_) => true,
                _ => false,
            }
        }

        fn is_shutdown(payload: &Payload) -> bool {
            use lsp::request::{Request, Shutdown};
            matches!(payload, Payload::Request { value, .. }
                if value.lock().as_ref().is_some_and(|value| value.method == Shutdown::METHOD))
        }

        fn is_exit(payload: &Payload) -> bool {
            use lsp::notification::{Exit, Notification};
            matches!(payload, Payload::Notification(jsonrpc::Notification { method, .. }) if method == Exit::METHOD)
        }

        // TODO: events that use capabilities need to do the right thing

        loop {
            pending_messages.retain(|message| !message.is_canceled());
            tokio::select! {
                biased;
                msg = inject_rx.recv() => {
                    if let Some(msg) = msg {
                        if matches!(msg, Payload::Disconnected) {
                            break;
                        }
                        match transport.send_payload_to_server(&mut server_stdin, msg).await {
                            Ok(_) => {}
                            Err(err) => {
                                error!("{} inject err: <- {err:?}", transport.name);
                                transport.disconnect();
                                break;
                            }
                        }
                    }
                }
                _ = &mut notified, if is_pending => {
                    // server successfully initialized
                    is_pending = false;

                    // Hack: inject an initialized notification so we trigger code that needs to happen after init
                    let notification = ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                        jsonrpc: None,

                        method: lsp::notification::Initialized::METHOD.to_string(),
                        params: jsonrpc::Params::None,
                    }));
                    let language_server_name = &transport.name;
                    match transport.process_server_message(&client_tx, notification, language_server_name).await {
                        Ok(_) => {}
                        Err(err) => {
                            error!("{language_server_name} err: <- {err:?}");
                        }
                    }

                    // drain the pending queue and send payloads to server
                    for msg in pending_messages.drain(..) {
                        log::info!("Draining pending message {:?}", msg);
                        match transport.send_payload_to_server(&mut server_stdin, msg).await {
                            Ok(_) => {}
                            Err(err) => {
                                error!("{language_server_name} err: <- {err:?}");
                                transport.disconnect();
                                return;
                            }
                        }
                    }
                }
                msg = client_rx.recv() => {
                    if let Some(msg) = msg {
                        if msg.is_canceled() {
                            continue;
                        } else if is_pending && is_shutdown(&msg) {
                            log::info!("Language server not initialized, shutting down");
                            break;
                        } else if is_pending && !allowed_before_initialize(&msg) {
                            // ignore notifications
                            if matches!(msg, Payload::Notification(_) | Payload::FullDocumentChange(_)) {
                                continue;
                            }

                            log::info!("Language server not initialized, delaying request");
                            pending_messages.push(msg);
                        } else {
                            let is_shutdown_msg = is_shutdown(&msg);
                            let is_exit_msg = is_exit(&msg);
                            // Set the flag *before* flushing to stdin so that the recv task
                            // cannot observe an unanswered server request in the window between
                            // the kernel delivering the bytes to the server and this store.
                            if is_shutdown_msg {
                                transport
                                    .shutdown_requested
                                    .store(true, Ordering::Release);
                            }
                            match transport.send_payload_to_server(&mut server_stdin, msg).await {
                                Ok(_) => {
                                    // `exit` is the last thing a shutting-down client sends;
                                    // signal that it has reached the server's stdin.
                                    if is_exit_msg {
                                        transport.shutdown_flushed.notify_one();
                                    }
                                }
                                Err(err) => {
                                    error!("{} err: <- {err:?}", transport.name);
                                    transport.disconnect();
                                    break;
                                }
                            }
                        }
                    } else {
                        // channel closed
                        break;
                    }
                }
            }
        }
        transport.disconnect();
    }
}

#[cfg(test)]
mod tests;
