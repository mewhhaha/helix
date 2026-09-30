use std::{sync::Arc, time::Duration};

use helix_core::syntax::config::{DebugAdapterConfig, DebuggerQuirks};
use helix_dap::Payload;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::tcp::OwnedWriteHalf,
    sync::Mutex,
};

use super::helpers::AppBuilder;

async fn read_request(reader: &mut (impl AsyncBufRead + Unpin)) -> anyhow::Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        anyhow::ensure!(
            reader.read_line(&mut line).await? != 0,
            "adapter disconnected"
        );
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; length.ok_or_else(|| anyhow::anyhow!("missing length"))?];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn reply(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request: &Value,
    body: Value,
) -> anyhow::Result<()> {
    let response = json!({"type":"response", "seq":1, "request_seq":request["seq"], "command":request["command"], "success":true, "body":body}).to_string();
    writer
        .lock()
        .await
        .write_all(format!("Content-Length: {}\r\n\r\n{response}", response.len()).as_bytes())
        .await?;
    Ok(())
}

fn thread_id(value: isize) -> helix_dap::ThreadId {
    serde_json::from_value(json!(value)).unwrap()
}

fn event(name: &str, body: Value) -> Payload {
    serde_json::from_value(json!({"type":"event", "event":name, "body":body})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_events_remain_responsive_and_pending_resume_cannot_replace_new_stop(
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (requests_tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let adapter = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, writer) = stream.into_split();
        let writer = Arc::new(Mutex::new(writer));
        let mut reader = BufReader::new(reader);
        loop {
            let request = read_request(&mut reader).await?;
            if request["command"] == "initialize" {
                reply(&writer, &request, json!({})).await?;
            } else if requests_tx.send((request, writer.clone())).is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let mut app = AppBuilder::new().build()?;
    let config = DebugAdapterConfig {
        name: "test".into(),
        transport: "tcp".into(),
        command: String::new(),
        args: Vec::new(),
        port_arg: None,
        templates: Vec::new(),
        quirks: DebuggerQuirks::default(),
    };
    let id = app
        .editor
        .debug_adapters
        .start_client(Some(address), &config, false)?;
    app.editor.debug_adapters.set_active_client(id);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            app.editor.handle_debugger_message(
                id,
                event(
                    "stopped",
                    json!({"reason":"breakpoint", "threadId":7, "allThreadsStopped":true})
                )
            )
        )
        .await?
    );
    let first = tokio::time::timeout(Duration::from_secs(1), requests.recv())
        .await?
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), requests.recv())
        .await?
        .unwrap();
    assert_eq!(
        [&first.0, &second.0]
            .iter()
            .filter(|request| request["command"] == "stackTrace")
            .count(),
        1
    );
    helix_view::handlers::dap::select_thread_id(&mut app.editor, thread_id(7), true);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), requests.recv())
            .await
            .is_err(),
        "thread selection duplicated pending stack request"
    );

    let old_guard = {
        let client = app.editor.debug_adapters.get_client_mut(id).unwrap();
        client.begin_resume();
        client.stop_guard()
    };
    for (request, writer) in [first, second] {
        let body = if request["command"] == "stackTrace" {
            json!({"stackFrames":[{"id":1,"name":"old","line":1,"column":1}]})
        } else {
            json!({"threads":[{"id":7,"name":"old thread"}]})
        };
        reply(&writer, &request, body).await?;
    }
    app.editor
        .handle_debugger_message(id, event("stopped", json!({"reason":"step", "threadId":9})))
        .await;
    assert!(
        !old_guard.is_current(),
        "new stop invalidates the outstanding resume response"
    );
    let (request, writer) = tokio::time::timeout(Duration::from_secs(1), requests.recv())
        .await?
        .unwrap();
    assert_eq!(request["command"], "stackTrace");
    assert_eq!(request["arguments"]["threadId"], 9);
    reply(
        &writer,
        &request,
        json!({"stackFrames":[{"id":2,"name":"current","line":1,"column":1}]}),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !app
            .editor
            .debug_adapters
            .get_client(id)
            .unwrap()
            .stack_frames
            .contains_key(&thread_id(9))
        {
            let event = app.editor.wait_event().await;
            app.handle_editor_event(event).await;
        }
    })
    .await?;
    let client = app.editor.debug_adapters.get_client(id).unwrap();
    assert_eq!(client.thread_id, Some(thread_id(9)));
    assert_eq!(client.active_frame, Some(0));
    assert!(!client.stack_frames.contains_key(&thread_id(7)));
    assert_eq!(client.stack_frames[&thread_id(9)][0].name, "current");
    adapter.abort();
    Ok(())
}
