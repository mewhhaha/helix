use crate::{registry::DebugAdapterId, Error, Result};
use anyhow::Context;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{collections::HashMap, fmt::Debug};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        mpsc::{unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
        Mutex, Notify,
    },
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    #[serde(skip)]
    pub back_ch: Option<Sender<Result<Response>>>,
    pub seq: u64,
    pub command: String,
    pub arguments: Option<Value>,
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Response {
    // seq is omitted as unused and is not sent by some implementations
    pub request_seq: u64,
    pub success: bool,
    pub command: String,
    pub message: Option<String>,
    pub body: Option<Value>,
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Event {
    pub event: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Payload {
    // type = "event"
    Event(Event),
    // type = "response"
    Response(Response),
    // type = "request"
    Request(Request),
}

#[derive(Debug)]
pub struct Transport {
    #[allow(unused)]
    id: DebugAdapterId,
    pending_requests: Mutex<HashMap<u64, Sender<Result<Response>>>>,
    disconnected: AtomicBool,
    disconnect_notify: Notify,
}

impl Transport {
    pub fn start(
        server_stdout: Box<dyn AsyncBufRead + Unpin + Send>,
        server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        server_stderr: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
        id: DebugAdapterId,
    ) -> (UnboundedReceiver<Payload>, UnboundedSender<Payload>) {
        let (client_tx, rx) = unbounded_channel();
        let (tx, client_rx) = unbounded_channel();

        let transport = Self {
            id,
            pending_requests: Mutex::new(HashMap::default()),
            disconnected: AtomicBool::new(false),
            disconnect_notify: Notify::new(),
        };

        let transport = Arc::new(transport);

        tokio::spawn(Self::recv(id, transport.clone(), server_stdout, client_tx));
        if let Some(stderr) = server_stderr {
            tokio::spawn(Self::err(transport.clone(), stderr));
        }
        tokio::spawn(Self::send(transport, server_stdin, client_rx));

        (rx, tx)
    }

    async fn recv_server_message(
        id: DebugAdapterId,
        reader: &mut Box<dyn AsyncBufRead + Unpin + Send>,
        buffer: &mut String,
        content: &mut Vec<u8>,
    ) -> Result<Payload> {
        if !helix_stdx::protocol::read_frame(reader, buffer, content).await? {
            return Err(Error::StreamClosed);
        }
        let msg = std::str::from_utf8(content).context("invalid utf8 from server")?;

        info!("[{}] <- DAP {}", id, msg);

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
    ) -> Result<()> {
        buffer.truncate(0);
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        };
        error!("err <- {}", buffer);

        Ok(())
    }

    async fn send_payload_to_server(
        &self,
        server_stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
        mut payload: Payload,
    ) -> Result<()> {
        if self.disconnected.load(Ordering::Acquire) {
            return Err(Error::StreamClosed);
        }
        if let Payload::Request(request) = &mut payload {
            let mut pending = self.pending_requests.lock().await;
            // Serialize registration with disconnect's final drain.
            if self.disconnected.load(Ordering::Acquire) {
                return Err(Error::StreamClosed);
            }
            // Guarded background workflows drop their waiters on cancellation.
            // Prune these entries before the next request instead of retaining
            // them indefinitely when an adapter never sends a reply.
            pending.retain(|_, callback| !callback.is_closed());
            if let Some(back) = request.back_ch.take() {
                if back.is_closed() {
                    return Ok(());
                }
                pending.insert(request.seq, back);
            }
        }
        let json = serde_json::to_string(&payload)?;
        self.send_string_to_server(server_stdin, json).await
    }

    async fn send_string_to_server(
        &self,
        server_stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
        request: String,
    ) -> Result<()> {
        info!("[{}] -> DAP {}", self.id, request);

        // send the headers
        server_stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", request.len()).as_bytes())
            .await?;

        // send the body
        server_stdin.write_all(request.as_bytes()).await?;

        server_stdin.flush().await?;

        Ok(())
    }

    fn process_response(&self, res: Response) -> Result<Response> {
        if res.success {
            info!(
                "[{}] <- DAP success in response to {}",
                self.id, res.request_seq
            );

            Ok(res)
        } else {
            error!(
                "[{}] <- DAP error {:?} ({:?}) for command #{} {}",
                self.id, res.message, res.body, res.request_seq, res.command
            );

            Err(Error::Other(anyhow::format_err!("{:?}", res.body)))
        }
    }

    async fn process_server_message(
        &self,
        client_tx: &UnboundedSender<Payload>,
        msg: Payload,
    ) -> Result<()> {
        match msg {
            Payload::Response(res) => {
                let request_seq = res.request_seq;
                let tx = self.pending_requests.lock().await.remove(&request_seq);

                match tx {
                    Some(tx) => match tx.send(self.process_response(res)).await {
                        Ok(_) => (),
                        Err(_) => error!(
                            "Tried sending response into a closed channel (id={:?}), original request likely timed out",
                            request_seq
                        ),
                    }
                    None => {
                        // A canceled callback may already have been pruned.
                        // Responses belong to pending RPCs, not the event stream.
                        warn!("Response to nonexistent request #{}", res.request_seq);
                    }
                }

                Ok(())
            }
            Payload::Request(Request {
                ref command,
                ref seq,
                ..
            }) => {
                info!("[{}] <- DAP request {} #{}", self.id, command, seq);
                client_tx.send(msg).map_err(|_| Error::StreamClosed)
            }
            Payload::Event(ref event) => {
                info!("[{}] <- DAP event {:?}", self.id, event);
                client_tx.send(msg).map_err(|_| Error::StreamClosed)
            }
        }
    }

    /// All transport exits use this path, including a disappearing event consumer.
    async fn disconnect(&self) {
        let pending = {
            let mut pending = self.pending_requests.lock().await;
            if self.disconnected.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut *pending)
        };
        self.disconnect_notify.notify_waiters();
        for callback in pending.into_values() {
            // Each RPC has one reply. A closed/full callback must not hold up shutdown.
            let _ = callback.try_send(Err(Error::StreamClosed));
        }
    }

    async fn recv(
        id: DebugAdapterId,
        transport: Arc<Self>,
        mut server_stdout: Box<dyn AsyncBufRead + Unpin + Send>,
        client_tx: UnboundedSender<Payload>,
    ) {
        let mut recv_buffer = String::new();
        let mut content_buffer = Vec::new();
        let disconnected = transport.disconnect_notify.notified();
        tokio::pin!(disconnected);
        disconnected.as_mut().enable();
        while !transport.disconnected.load(Ordering::Acquire) {
            let message = tokio::select! {
                biased;
                _ = &mut disconnected => break,
                _ = client_tx.closed() => break,
                message = Self::recv_server_message(
                    id, &mut server_stdout, &mut recv_buffer, &mut content_buffer,
                ) => message,
            };
            let result = match message {
                Ok(message) => transport.process_server_message(&client_tx, message).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if !matches!(error, Error::StreamClosed) {
                    error!("[{id}] receive failed: {error:?}");
                }
                break;
            }
        }
        transport.disconnect().await;
    }

    async fn send_inner(
        transport: Arc<Self>,
        mut server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        mut client_rx: UnboundedReceiver<Payload>,
    ) -> Result<()> {
        while let Some(payload) = client_rx.recv().await {
            transport
                .send_payload_to_server(&mut server_stdin, payload)
                .await?;
        }
        Ok(())
    }

    async fn send(
        transport: Arc<Self>,
        server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        client_rx: UnboundedReceiver<Payload>,
    ) {
        let disconnected = transport.disconnect_notify.notified();
        tokio::pin!(disconnected);
        disconnected.as_mut().enable();
        if !transport.disconnected.load(Ordering::Acquire) {
            let result = tokio::select! {
                biased;
                _ = &mut disconnected => Ok(()),
                result = Self::send_inner(transport.clone(), server_stdin, client_rx) => result,
            };
            if let Err(error) = result {
                if !matches!(error, Error::StreamClosed) {
                    error!("[{}] send failed: {error:?}", transport.id);
                }
            }
        }
        transport.disconnect().await;
    }

    async fn err(transport: Arc<Self>, mut server_stderr: Box<dyn AsyncBufRead + Unpin + Send>) {
        let mut recv_buffer = String::new();
        let disconnected = transport.disconnect_notify.notified();
        tokio::pin!(disconnected);
        disconnected.as_mut().enable();
        while !transport.disconnected.load(Ordering::Acquire) {
            let result = tokio::select! {
                biased;
                _ = &mut disconnected => break,
                result = Self::recv_server_error(&mut server_stderr, &mut recv_buffer) => result,
            };
            if let Err(error) = result {
                if !matches!(error, Error::StreamClosed) {
                    error!("[{}] stderr failed: {error:?}", transport.id);
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests;
