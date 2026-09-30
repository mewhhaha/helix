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
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader, BufWriter,
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
    response: Sender<Result<Value>>,
    canceled: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct FullDocumentChange {
    document: lsp::VersionedTextDocumentIdentifier,
    text: Rope,
}

type PendingRequests = Arc<Mutex<HashMap<jsonrpc::Id, Sender<Result<Value>>>>>;
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
    ) -> Result<(Receiver<Result<Value>>, RequestGuard)> {
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
        let mut content_length = None;
        loop {
            buffer.clear();
            if reader.read_line(buffer).await? == 0 {
                return Err(Error::StreamClosed);
            }

            // debug!("<- header {:?}", buffer);

            if buffer == "\r\n" {
                // look for an empty CRLF line
                break;
            }

            let header = buffer.trim();

            let parts = header.split_once(": ");

            match parts {
                Some(("Content-Length", value)) => {
                    content_length = Some(value.parse().context("invalid content length")?);
                }
                Some((_, _)) => {}
                None => {
                    // Workaround: Some non-conformant language servers will output logging and other garbage
                    // into the same stream as JSON-RPC messages. This can also happen from shell scripts that spawn
                    // the server. Skip such lines and log a warning.

                    // warn!("Failed to parse header: {:?}", header);
                }
            }
        }

        let content_length = content_length.context("missing content length")?;
        content.resize(content_length, 0);
        reader.read_exact(content).await?;
        let msg = std::str::from_utf8(content).context("invalid utf8 from server")?;

        info!("{language_server_name} <- {msg}");

        // NOTE: We avoid using `?` here, since it would return early on error
        // and skip clearing `content`. By returning the result directly instead,
        // we ensure `content.clear()` is always called.
        let output = sonic_rs::from_slice(content).map_err(Into::into);

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
                            result: serde_json::Value::Null,
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
mod tests {
    use super::*;
    use std::time::Duration;

    fn channels() -> (
        Arc<Transport>,
        OutboundSender,
        UnboundedReceiver<Payload>,
        UnboundedReceiver<Payload>,
    ) {
        let (tx, rx) = unbounded_channel();
        let (inject_tx, inject_rx) = unbounded_channel();
        let transport = Arc::new(Transport {
            id: LanguageServerId::default(),
            name: "test-server".into(),
            pending_requests: Arc::default(),
            disconnected: Arc::default(),
            disconnect_notify: Notify::new(),
            shutdown_requested: AtomicBool::new(false),
            inject_tx,
            shutdown_flushed: Arc::default(),
        });
        let sender = OutboundSender::new(
            tx,
            transport.inject_tx.clone(),
            transport.pending_requests.clone(),
            transport.disconnected.clone(),
        );
        (transport, sender, rx, inject_rx)
    }

    struct MockServer {
        transport: Arc<Transport>,
        sender: OutboundSender,
        reader: BufReader<tokio::io::DuplexStream>,
        initialize: Arc<Notify>,
        writer: tokio::task::JoinHandle<()>,
        receiver: tokio::task::JoinHandle<()>,
        _messages: UnboundedReceiver<(LanguageServerId, jsonrpc::Call)>,
    }

    impl MockServer {
        fn new(capacity: usize, initialized: bool) -> Self {
            let (transport, sender, rx, inject_rx) = channels();
            let (client, server) = tokio::io::duplex(capacity);
            let (reader, writer) = tokio::io::split(client);
            let (client_tx, messages) = unbounded_channel();
            let initialize = Arc::new(Notify::new());
            if initialized {
                initialize.notify_one();
            }
            let receiver = tokio::spawn(Transport::recv(
                transport.clone(),
                BufReader::new(reader),
                client_tx.clone(),
            ));
            let writer = tokio::spawn(Transport::send(
                transport.clone(),
                BufWriter::new(writer),
                client_tx,
                rx,
                inject_rx,
                initialize.clone(),
            ));
            Self {
                transport,
                sender,
                reader: BufReader::new(server),
                initialize,
                writer,
                receiver,
                _messages: messages,
            }
        }

        async fn next(&mut self) -> ServerMessage {
            tokio::time::timeout(
                Duration::from_secs(3),
                Transport::recv_server_message(
                    &mut self.reader,
                    &mut String::new(),
                    &mut Vec::new(),
                    "test-server",
                ),
            )
            .await
            .expect("outgoing message timed out")
            .unwrap()
        }

        async fn respond(&mut self, id: u64, result: Value) {
            let response = serde_json::to_vec(&jsonrpc::Output::Success(jsonrpc::Success {
                jsonrpc: Some(jsonrpc::Version::V2),
                id: jsonrpc::Id::Num(id),
                result,
            }))
            .unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", response.len());
            self.reader
                .get_mut()
                .write_all(header.as_bytes())
                .await
                .unwrap();
            self.reader.get_mut().write_all(&response).await.unwrap();
        }

        async fn stop(self) {
            drop(self.reader);
            drop(self.sender);
            tokio::time::timeout(Duration::from_secs(3), self.writer)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), self.receiver)
                .await
                .unwrap()
                .unwrap();
        }
    }

    fn document(uri: &str, version: i32) -> lsp::VersionedTextDocumentIdentifier {
        lsp::VersionedTextDocumentIdentifier::new(lsp::Url::parse(uri).unwrap(), version)
    }

    fn notification(method: &str, params: Value) -> Payload {
        let Value::Object(params) = params else {
            panic!("notification params must be an object");
        };
        Payload::Notification(jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.into(),
            params: jsonrpc::Params::Map(params),
        })
    }

    fn marker(sender: &OutboundSender, id: u64) {
        sender
            .send(Payload::Response(jsonrpc::Output::Success(
                jsonrpc::Success {
                    jsonrpc: Some(jsonrpc::Version::V2),
                    id: jsonrpc::Id::Num(id),
                    result: Value::Null,
                },
            )))
            .unwrap();
    }

    fn symbol_request(
        sender: &OutboundSender,
        id: u64,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<Option<lsp::WorkspaceSymbolResponse>>> {
        crate::client::request_with_timeout::<lsp::request::WorkspaceSymbolRequest>(
            sender,
            jsonrpc::Id::Num(id),
            &lsp::WorkspaceSymbolParams::default(),
            timeout,
        )
    }

    fn assert_marker(message: ServerMessage, expected: u64) {
        assert!(
            matches!(message, ServerMessage::Output(jsonrpc::Output::Success(response))
            if response.id == jsonrpc::Id::Num(expected))
        );
    }

    fn assert_request(message: ServerMessage, expected: u64) {
        assert!(
            matches!(message, ServerMessage::Call(jsonrpc::Call::MethodCall(request))
            if request.id == jsonrpc::Id::Num(expected))
        );
    }

    fn assert_cancellation(message: ServerMessage, expected: u64) {
        let ServerMessage::Call(jsonrpc::Call::Notification(notification)) = message else {
            panic!("expected cancellation notification");
        };
        assert_eq!(notification.method, "$/cancelRequest");
        assert_eq!(
            Value::from(notification.params),
            serde_json::json!({ "id": expected })
        );
    }

    fn assert_change(message: ServerMessage, uri: &str, version: i32, text: &str) {
        let ServerMessage::Call(jsonrpc::Call::Notification(notification)) = message else {
            panic!("expected full document change");
        };
        assert_eq!(notification.method, "textDocument/didChange");
        let params: lsp::DidChangeTextDocumentParams = notification.params.parse().unwrap();
        assert_eq!(params.text_document, document(uri, version));
        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(params.content_changes[0].text, text);
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn typing_retains_only_the_latest_unsent_snapshot() {
        let (_, sender, mut queue, _inject) = channels();
        for version in 0..1000 {
            sender
                .full_document_change(
                    document("file:///a.rs", version),
                    Rope::from_str(&version.to_string()),
                )
                .unwrap();
        }
        assert_eq!(queue.len(), 1);
        let Payload::FullDocumentChange(snapshot) = queue.recv().await.unwrap() else {
            panic!("expected a deferred Rope snapshot");
        };
        let snapshot = snapshot.lock().take().unwrap();
        assert_eq!(snapshot.document.version, 999);
        assert_eq!(snapshot.text.to_string(), "999");
    }

    #[tokio::test]
    async fn full_changes_preserve_request_open_save_and_close_barriers() {
        let mut server = MockServer::new(256, true);
        server.sender.send(notification("textDocument/didOpen", serde_json::json!({
            "textDocument": {"uri": "file:///a.rs", "languageId": "rust", "version": 0, "text": "initial"}
        }))).unwrap();
        for (uri, version, text) in [
            ("file:///a.rs", 1, "a1"),
            ("file:///b.rs", 1, "b1"),
            ("file:///a.rs", 2, "a2"),
        ] {
            server
                .sender
                .full_document_change(document(uri, version), Rope::from_str(text))
                .unwrap();
        }
        let request = symbol_request(&server.sender, 42, Duration::from_secs(3));
        for version in [3, 4] {
            server
                .sender
                .full_document_change(
                    document("file:///a.rs", version),
                    Rope::from_str(&format!("a{version}")),
                )
                .unwrap();
        }
        server
            .sender
            .send(notification(
                "textDocument/didSave",
                serde_json::json!({"textDocument": {"uri": "file:///a.rs"}}),
            ))
            .unwrap();
        server
            .sender
            .full_document_change(document("file:///a.rs", 5), Rope::from_str("a5"))
            .unwrap();
        server
            .sender
            .send(notification(
                "textDocument/didClose",
                serde_json::json!({"textDocument": {"uri": "file:///a.rs"}}),
            ))
            .unwrap();
        assert!(
            matches!(server.next().await, ServerMessage::Call(jsonrpc::Call::Notification(message)) if message.method == "textDocument/didOpen")
        );
        assert_change(server.next().await, "file:///a.rs", 2, "a2");
        assert_change(server.next().await, "file:///b.rs", 1, "b1");
        assert_request(server.next().await, 42);
        assert_change(server.next().await, "file:///a.rs", 4, "a4");
        assert!(
            matches!(server.next().await, ServerMessage::Call(jsonrpc::Call::Notification(message)) if message.method == "textDocument/didSave")
        );
        assert_change(server.next().await, "file:///a.rs", 5, "a5");
        assert!(
            matches!(server.next().await, ServerMessage::Call(jsonrpc::Call::Notification(message)) if message.method == "textDocument/didClose")
        );
        server.respond(42, Value::Null).await;
        assert!(request.await.unwrap().is_none());
        server.stop().await;
    }

    #[tokio::test]
    async fn slow_server_does_not_retain_every_typing_revision() {
        let mut server = MockServer::new(256, true);
        let initial = "long initial text\n".repeat(8192);
        server
            .sender
            .full_document_change(document("file:///a.rs", 0), Rope::from_str(&initial))
            .unwrap();
        wait_until(|| {
            server
                .sender
                .changes
                .lock()
                .get(&document("file:///a.rs", 0).uri)
                .is_some_and(|snapshot| {
                    snapshot
                        .upgrade()
                        .is_none_or(|snapshot| snapshot.lock().is_none())
                })
        })
        .await;
        // The first write is stalled on the tiny pipe. Producer-side coalescing
        // must still happen while the transport cannot dequeue more messages.
        for version in 1..1000 {
            server
                .sender
                .full_document_change(
                    document("file:///a.rs", version),
                    Rope::from_str(&format!("a{version}")),
                )
                .unwrap();
        }
        server
            .sender
            .full_document_change(document("file:///b.rs", 1), Rope::from_str("b1"))
            .unwrap();
        {
            let snapshots = server.sender.changes.lock();
            assert_eq!(snapshots.len(), 2);
            let current = snapshots[&document("file:///a.rs", 0).uri]
                .upgrade()
                .unwrap();
            assert_eq!(current.lock().as_ref().unwrap().document.version, 999);
        }
        assert_change(server.next().await, "file:///a.rs", 0, &initial);
        assert_change(server.next().await, "file:///a.rs", 999, "a999");
        assert_change(server.next().await, "file:///b.rs", 1, "b1");
        server.stop().await;
    }

    #[tokio::test]
    async fn deferred_full_change_serialization_preserves_unicode_and_json_escapes() {
        let mut server = MockServer::new(1024, true);
        let text = "😀 \"quoted\" \\ \t\n\r Ω".repeat(1000);
        server
            .sender
            .full_document_change(document("file:///a.rs", 7), Rope::from_str(&text))
            .unwrap();
        assert_change(server.next().await, "file:///a.rs", 7, &text);
        server.stop().await;
    }

    #[tokio::test]
    async fn unpolled_request_is_canceled_before_send() {
        let mut server = MockServer::new(1024, true);
        let request = symbol_request(&server.sender, 1, Duration::from_secs(3));
        drop(request);
        marker(&server.sender, 99);
        assert_marker(server.next().await, 99);
        assert!(server.transport.pending_requests.lock().is_empty());
        server.stop().await;
    }

    #[tokio::test]
    async fn cancellation_releases_requests_waiting_for_initialization() {
        let mut server = MockServer::new(1024, false);
        let request = symbol_request(&server.sender, 1, Duration::from_secs(3));
        marker(&server.sender, 98);
        assert_marker(server.next().await, 98);
        drop(request);
        server.initialize.notify_one();
        marker(&server.sender, 99);
        assert_marker(server.next().await, 99);
        assert!(server.transport.pending_requests.lock().is_empty());
        server.stop().await;
    }

    #[tokio::test]
    async fn cancellation_after_send_cleans_pending_state_and_ignores_late_reply() {
        let mut server = MockServer::new(1024, true);
        let request = symbol_request(&server.sender, 1, Duration::from_secs(3));
        assert_request(server.next().await, 1);
        assert_eq!(server.transport.pending_requests.lock().len(), 1);
        drop(request);
        assert!(server.transport.pending_requests.lock().is_empty());
        assert_cancellation(server.next().await, 1);
        server.respond(1, Value::Null).await;
        let next = symbol_request(&server.sender, 2, Duration::from_secs(3));
        assert_request(server.next().await, 2);
        server.respond(2, Value::Null).await;
        assert!(next.await.unwrap().is_none());
        server.stop().await;
    }

    #[tokio::test]
    async fn timeout_cancels_sent_request_and_removes_pending_state() {
        let mut server = MockServer::new(1024, true);
        let request = symbol_request(&server.sender, 3, Duration::from_millis(20));
        assert_request(server.next().await, 3);
        assert!(matches!(
            request.await,
            Err(Error::Timeout(jsonrpc::Id::Num(3)))
        ));
        assert!(server.transport.pending_requests.lock().is_empty());
        assert_cancellation(server.next().await, 3);
        server.stop().await;
    }

    #[tokio::test]
    async fn successful_request_does_not_emit_cancellation() {
        let mut server = MockServer::new(1024, true);
        let request = symbol_request(&server.sender, 4, Duration::from_secs(3));
        assert_request(server.next().await, 4);
        server.respond(4, Value::Null).await;
        assert!(request.await.unwrap().is_none());
        marker(&server.sender, 99);
        assert_marker(server.next().await, 99);
        assert!(server.transport.pending_requests.lock().is_empty());
        server.stop().await;
    }

    #[tokio::test]
    async fn cancel_during_a_blocked_request_write_is_sent_after_the_request() {
        let mut server = MockServer::new(64, true);
        let request = crate::client::request_with_timeout::<lsp::request::WorkspaceSymbolRequest>(
            &server.sender,
            jsonrpc::Id::Num(5),
            &lsp::WorkspaceSymbolParams {
                query: "x".repeat(32768),
                ..Default::default()
            },
            Duration::from_secs(3),
        );
        wait_until(|| {
            server
                .transport
                .pending_requests
                .lock()
                .contains_key(&jsonrpc::Id::Num(5))
        })
        .await;
        drop(request);
        assert!(server.transport.pending_requests.lock().is_empty());
        assert_request(server.next().await, 5);
        assert_cancellation(server.next().await, 5);
        server.stop().await;
    }

    #[tokio::test]
    async fn canceled_queued_request_releases_parameters_while_an_earlier_write_is_blocked() {
        let mut server = MockServer::new(64, true);
        let blocker = crate::client::request_with_timeout::<lsp::request::WorkspaceSymbolRequest>(
            &server.sender,
            jsonrpc::Id::Num(10),
            &lsp::WorkspaceSymbolParams {
                query: "x".repeat(32768),
                ..Default::default()
            },
            Duration::from_secs(3),
        );
        wait_until(|| {
            server
                .transport
                .pending_requests
                .lock()
                .contains_key(&jsonrpc::Id::Num(10))
        })
        .await;
        let (_response, guard) = server
            .sender
            .request(jsonrpc::MethodCall {
                jsonrpc: Some(jsonrpc::Version::V2),
                id: jsonrpc::Id::Num(11),
                method: "workspace/symbol".into(),
                params: jsonrpc::Params::Map(serde_json::Map::from_iter([(
                    "query".into(),
                    Value::String("y".repeat(32768)),
                )])),
            })
            .unwrap();
        let queued = guard.queued.clone();
        assert!(queued.lock().is_some());
        drop(guard);
        // Nothing has read from the pipe yet, so the writer cannot have reached
        // request 11. Its large JSON parameters must already have been dropped.
        assert!(queued.lock().is_none());
        assert!(!server
            .transport
            .pending_requests
            .lock()
            .contains_key(&jsonrpc::Id::Num(11)));
        marker(&server.sender, 99);
        assert_request(server.next().await, 10);
        server.respond(10, Value::Null).await;
        assert!(blocker.await.unwrap().is_none());
        assert_marker(server.next().await, 99);
        server.stop().await;
    }

    #[tokio::test]
    async fn disconnect_completes_requests_and_rejects_new_ones() {
        let mut server = MockServer::new(1024, true);
        let request = symbol_request(&server.sender, 6, Duration::from_secs(3));
        assert_request(server.next().await, 6);
        server.reader.get_mut().shutdown().await.unwrap();
        assert!(matches!(request.await, Err(Error::StreamClosed)));
        assert!(server.transport.pending_requests.lock().is_empty());
        let next = symbol_request(&server.sender, 7, Duration::from_secs(3));
        assert!(matches!(next.await, Err(Error::StreamClosed)));
        server.stop().await;
    }

    #[tokio::test]
    async fn confirmed_disconnect_interrupts_a_blocked_write_without_waiting_for_peer_reads() {
        let mut server = MockServer::new(64, true);
        let request = crate::client::request_with_timeout::<lsp::request::WorkspaceSymbolRequest>(
            &server.sender,
            jsonrpc::Id::Num(9),
            &lsp::WorkspaceSymbolParams {
                query: "x".repeat(32768),
                ..Default::default()
            },
            Duration::from_secs(3),
        );
        wait_until(|| {
            server
                .transport
                .pending_requests
                .lock()
                .contains_key(&jsonrpc::Id::Num(9))
        })
        .await;
        // Close only the server-to-client stream: its read side remains open and
        // does not drain the blocked outgoing frame.
        server.reader.get_mut().shutdown().await.unwrap();
        assert!(matches!(request.await, Err(Error::StreamClosed)));
        wait_until(|| server.writer.is_finished()).await;
        assert!(server.transport.pending_requests.lock().is_empty());
        server.stop().await;
    }

    #[tokio::test]
    async fn fire_and_forget_shutdown_is_not_pruned_as_a_canceled_request() {
        let mut server = MockServer::new(1024, true);
        server
            .sender
            .send(Payload::Request {
                pending: None,
                value: Arc::new(Mutex::new(Some(jsonrpc::MethodCall {
                    jsonrpc: Some(jsonrpc::Version::V2),
                    id: jsonrpc::Id::Num(8),
                    method: "shutdown".into(),
                    params: jsonrpc::Params::None,
                }))),
            })
            .unwrap();
        server
            .sender
            .send(notification("exit", serde_json::json!({})))
            .unwrap();
        assert_request(server.next().await, 8);
        assert!(
            matches!(server.next().await, ServerMessage::Call(jsonrpc::Call::Notification(message)) if message.method == "exit")
        );
        tokio::time::timeout(
            Duration::from_secs(3),
            server.transport.shutdown_flushed.notified(),
        )
        .await
        .unwrap();
        assert!(server.transport.pending_requests.lock().is_empty());
        server.stop().await;
    }

    #[tokio::test]
    async fn replies_are_sent_while_client_requests_wait_for_initialization() {
        let (stdin, stdout) = tokio::io::duplex(4096);
        let (client_tx, _client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let (inject_tx, inject_rx) = unbounded_channel();
        let initialize_notify = Arc::new(Notify::new());
        let transport = Arc::new(Transport {
            id: LanguageServerId::default(),
            name: "test-server".into(),
            pending_requests: Arc::default(),
            disconnected: Arc::default(),
            disconnect_notify: Notify::new(),
            shutdown_requested: AtomicBool::new(false),
            inject_tx,
            shutdown_flushed: Arc::new(Notify::new()),
        });
        let send = tokio::spawn(Transport::send(
            transport,
            BufWriter::new(stdin),
            client_tx,
            server_rx,
            inject_rx,
            initialize_notify.clone(),
        ));

        for (id, method) in [(0, "initialize"), (1, "textDocument/hover")] {
            server_tx
                .send(Payload::Request {
                    pending: None,
                    value: Arc::new(Mutex::new(Some(jsonrpc::MethodCall {
                        jsonrpc: Some(jsonrpc::Version::V2),
                        id: jsonrpc::Id::Num(id),
                        method: method.into(),
                        params: jsonrpc::Params::None,
                    }))),
                })
                .unwrap();
        }
        let response = jsonrpc::Output::Success(jsonrpc::Success {
            jsonrpc: Some(jsonrpc::Version::V2),
            id: jsonrpc::Id::Num(42),
            result: serde_json::json!({ "title": "Continue" }),
        });
        server_tx.send(Payload::Response(response.clone())).unwrap();

        let mut reader = BufReader::new(stdout);
        let mut buffer = String::new();
        let mut content = Vec::new();
        let first = tokio::time::timeout(
            Duration::from_secs(1),
            Transport::recv_server_message(&mut reader, &mut buffer, &mut content, "test-server"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            first,
            ServerMessage::Call(jsonrpc::Call::MethodCall(call)) if call.method == "initialize"
        ));

        let second = tokio::time::timeout(
            Duration::from_secs(1),
            Transport::recv_server_message(&mut reader, &mut buffer, &mut content, "test-server"),
        )
        .await
        .expect("the server must receive its reply before finishing initialization")
        .unwrap();
        assert_eq!(second, ServerMessage::Output(response));

        initialize_notify.notify_one();
        let third = tokio::time::timeout(
            Duration::from_secs(1),
            Transport::recv_server_message(&mut reader, &mut buffer, &mut content, "test-server"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            third,
            ServerMessage::Call(jsonrpc::Call::MethodCall(call)) if call.method == "textDocument/hover"
        ));

        drop(server_tx);
        send.await.unwrap();
    }
}
