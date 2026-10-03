use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc::channel;

fn request(seq: u64, callback: Sender<Result<Response>>) -> Payload {
    Payload::Request(Request {
        seq,
        command: "stackTrace".into(),
        arguments: Some(serde_json::json!({"threadId":7})),
        back_ch: Some(callback),
    })
}

fn transport() -> Transport {
    Transport {
        id: DebugAdapterId::default(),
        pending_requests: Mutex::new(HashMap::new()),
        disconnected: AtomicBool::new(false),
        disconnect_notify: Notify::new(),
    }
}

#[tokio::test]
async fn closed_event_consumer_is_a_normal_disconnect() {
    let transport = transport();
    let (events, receiver) = unbounded_channel();
    drop(receiver);
    for message in [
        Payload::Event(Event {
            event: "terminated".into(),
            body: None,
        }),
        Payload::Request(Request {
            seq: 1,
            command: "runInTerminal".into(),
            arguments: None,
            back_ch: None,
        }),
    ] {
        assert!(matches!(
            transport.process_server_message(&events, message).await,
            Err(Error::StreamClosed)
        ));
    }
}

#[tokio::test]
async fn disconnect_drains_requests_and_rejects_late_registration() {
    let transport = transport();
    let (reply, mut receiver) = channel(1);
    transport.pending_requests.lock().await.insert(1, reply);
    transport.disconnect().await;
    transport.disconnect().await;
    assert!(matches!(
        receiver.recv().await,
        Some(Err(Error::StreamClosed))
    ));
    assert!(transport.pending_requests.lock().await.is_empty());
    let (reply, mut receiver) = channel(1);
    let mut writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tokio::io::sink());
    assert!(matches!(
        transport
            .send_payload_to_server(&mut writer, request(2, reply))
            .await,
        Err(Error::StreamClosed)
    ));
    assert!(receiver.recv().await.is_none());
    assert!(transport.pending_requests.lock().await.is_empty());
}

#[tokio::test]
async fn losing_the_consumer_stops_a_stalled_reader_and_writer() {
    let (reader, _adapter_stdout) = tokio::io::duplex(8);
    let (writer, _adapter_stdin) = tokio::io::duplex(1);
    let (events, sender) = Transport::start(
        Box::new(BufReader::new(reader)),
        Box::new(writer),
        None,
        DebugAdapterId::default(),
    );
    let (reply, mut receiver) = channel(1);
    sender.send(request(1, reply)).unwrap();
    tokio::task::yield_now().await;
    drop(events);
    tokio::time::timeout(Duration::from_secs(1), sender.closed())
        .await
        .unwrap();
    // Depending on registration order, shutdown either replies or drops the queued RPC.
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap(),
        None | Some(Err(Error::StreamClosed))
    ));
}

struct BrokenWriter;

#[tokio::test]
async fn reader_eof_stops_a_stalled_writer_and_releases_registered_requests() {
    let transport = Arc::new(transport());
    let (reply, mut first) = channel(1);
    transport.pending_requests.lock().await.insert(1, reply);
    let (reply, mut second) = channel(1);
    let (sender, requests) = unbounded_channel();
    sender.send(request(2, reply)).unwrap();
    let (writer, _adapter_stdin) = tokio::io::duplex(1);
    let writer = tokio::spawn(Transport::send(
        transport.clone(),
        Box::new(writer),
        requests,
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !transport.pending_requests.lock().await.contains_key(&2) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    Transport::recv(
        DebugAdapterId::default(),
        transport.clone(),
        Box::new(BufReader::new(tokio::io::empty())),
        events,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), writer)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first.recv().await, Some(Err(Error::StreamClosed))));
    assert!(matches!(
        second.recv().await,
        Some(Err(Error::StreamClosed))
    ));
    assert!(transport.pending_requests.lock().await.is_empty());
    assert!(sender.is_closed());
}

impl AsyncWrite for BrokenWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn write_failure_releases_requests_and_a_reader_with_no_eof() {
    let (reader, _adapter) = tokio::io::duplex(8);
    let (mut events, sender) = Transport::start(
        Box::new(BufReader::new(reader)),
        Box::new(BrokenWriter),
        None,
        DebugAdapterId::default(),
    );
    let (reply, mut receiver) = channel(1);
    sender.send(request(1, reply)).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap(),
        Some(Err(Error::StreamClosed))
    ));
    assert!(tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .is_none());
    tokio::time::timeout(Duration::from_secs(1), sender.closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn new_requests_prune_abandoned_callbacks_and_keep_live_requests() {
    let transport = transport();
    let mut writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tokio::io::sink());
    let (abandoned_tx, abandoned_rx) = channel(1);
    let (live_tx, mut live_rx) = channel(1);
    transport
        .send_payload_to_server(&mut writer, request(1, abandoned_tx))
        .await
        .unwrap();
    transport
        .send_payload_to_server(&mut writer, request(2, live_tx))
        .await
        .unwrap();
    drop(abandoned_rx);
    let (next_tx, _next_rx) = channel(1);
    transport
        .send_payload_to_server(&mut writer, request(3, next_tx))
        .await
        .unwrap();
    {
        let pending = transport.pending_requests.lock().await;
        assert!(!pending.contains_key(&1));
        assert!(pending.contains_key(&2));
        assert!(pending.contains_key(&3));
    }
    let (events, mut events_rx) = unbounded_channel();
    transport
        .process_server_message(
            &events,
            Payload::Response(Response {
                request_seq: 1,
                command: "stackTrace".into(),
                success: true,
                message: None,
                body: None,
            }),
        )
        .await
        .unwrap();
    transport
        .process_server_message(
            &events,
            Payload::Response(Response {
                request_seq: 2,
                command: "stackTrace".into(),
                success: true,
                message: None,
                body: None,
            }),
        )
        .await
        .unwrap();
    assert!(live_rx.recv().await.unwrap().unwrap().success);
    assert!(
        matches!(
            events_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "late canceled responses must not enter the debugger event stream"
    );
    assert!(!transport.pending_requests.lock().await.contains_key(&2));
}

#[tokio::test]
async fn canceled_queued_request_is_not_written_or_registered() {
    let transport = transport();
    let (client, adapter) = tokio::io::duplex(4096);
    let mut writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(client);
    let mut reader: Box<dyn AsyncBufRead + Send + Unpin> = Box::new(BufReader::new(adapter));
    let (canceled_tx, canceled_rx) = channel(1);
    drop(canceled_rx);
    transport
        .send_payload_to_server(&mut writer, request(1, canceled_tx))
        .await
        .unwrap();
    assert!(transport.pending_requests.lock().await.is_empty());
    let (live_tx, _live_rx) = channel(1);
    transport
        .send_payload_to_server(&mut writer, request(2, live_tx))
        .await
        .unwrap();
    let message = tokio::time::timeout(
        Duration::from_secs(1),
        Transport::recv_server_message(
            DebugAdapterId::default(),
            &mut reader,
            &mut String::new(),
            &mut Vec::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let Payload::Request(request) = message else {
        panic!("expected live request");
    };
    assert_eq!(
        request.seq, 2,
        "canceled request must not reach the adapter"
    );
    let mut byte = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(25), reader.read(&mut byte))
            .await
            .is_err()
    );
}
