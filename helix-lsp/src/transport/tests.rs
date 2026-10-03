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
            result: result.into(),
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
                result: Value::Null.into(),
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

#[test]
fn raw_envelopes_preserve_jsonrpc_compatibility_and_error_precedence() {
    for input in [
        r#"{"jsonrpc":"2.0","id":1,"result":null,"extra":true}"#,
        r#"{"id":4.0,"result":[{"label":"résumé","data":[1,2]}]}"#,
        r#"{"id":null,"method":"request","params":{"value":3}}"#,
        r#"{"method":"notify","params":[1,2],"traceparent":"ignored"}"#,
        r#"{"method":"notify","params":null}"#,
        r#"{"id":3,"result":42,"error":{"code":-32603,"message":"failure"}}"#,
        r#"{"id":3,"result":42,"error":null}"#,
        r#"{"id":"salvage","method":4}"#,
        r#"{"id":3,"method":"invalid","params":42}"#,
        r#"{"jsonrpc":"bad","id":3,"method":"invalid"}"#,
        r#"{}"#,
    ] {
        match sonic_rs::from_str::<ServerMessage>(input) {
            Ok(expected) => {
                let actual = decode_server_message(input.as_bytes()).unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{input}"
                );
            }
            Err(_) => assert!(decode_server_message(input.as_bytes()).is_err(), "{input}"),
        }
    }
    for input in [r#"{"result": [1,]}"#, r#"{"method": "broken}"#] {
        assert!(decode_server_message(input.as_bytes()).is_err());
    }
}

#[tokio::test]
async fn large_raw_responses_decode_directly_into_typed_results() {
    let values: Vec<_> = (0..10_000)
        .map(|i| format!("completion_{i}_résumé"))
        .collect();
    let wire = serde_json::json!({"id": 7, "result": values}).to_string();
    let ServerMessage::Output(jsonrpc::Output::Success(response)) =
        decode_server_message(wire.as_bytes()).unwrap()
    else {
        panic!()
    };
    assert!(
        matches!(response.result, jsonrpc::ResponseValue::Raw(ref raw) if raw.len() > 64 * 1024)
    );
    assert_eq!(
        response.result.parse::<Vec<String>>().await.unwrap(),
        values
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
        result: serde_json::json!({ "title": "Continue" }).into(),
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
