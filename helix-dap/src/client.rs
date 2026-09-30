use crate::{
    registry::DebugAdapterId,
    requests::{DisconnectArguments, TerminateArguments},
    transport::{Payload, Request, Response, Transport},
    Error, ProgressMap, ProgressState, Result,
};
use helix_core::syntax::config::{DebugAdapterConfig, DebuggerQuirks};
use helix_dap_types::*;

use futures_util::StreamExt;
use serde_json::Value;

use anyhow::anyhow;
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufRead, AsyncWrite, BufReader, BufWriter},
    net::TcpStream,
    process::{Child, Command},
    sync::mpsc::{channel, unbounded_channel, UnboundedReceiver, UnboundedSender},
    time,
};

#[derive(Debug, Default)]
struct RequestEpoch {
    generation: AtomicU64,
    changed: tokio::sync::Notify,
}

/// A result may update debugger UI only while its captured state is current.
#[derive(Clone, Debug)]
pub struct RequestGuard {
    epoch: Arc<RequestEpoch>,
    generation: u64,
}

impl RequestGuard {
    pub fn is_current(&self) -> bool {
        self.epoch.generation.load(Ordering::Relaxed) == self.generation
    }

    pub async fn canceled(&self) {
        loop {
            let changed = self.epoch.changed.notified();
            if !self.is_current() {
                return;
            }
            changed.await;
        }
    }
}

impl RequestEpoch {
    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }
    fn guard(self: &Arc<Self>) -> RequestGuard {
        RequestGuard {
            epoch: self.clone(),
            generation: self.generation.load(Ordering::Relaxed),
        }
    }
}

/// Owned request sender for background workflows without borrowing the editor.
#[derive(Clone, Debug)]
pub struct Requester {
    server_tx: UnboundedSender<Payload>,
    request_counter: Arc<AtomicU64>,
}

impl Requester {
    pub async fn request<R: helix_dap_types::Request>(
        &self,
        params: R::Arguments,
    ) -> Result<R::Result>
    where
        R::Arguments: serde::Serialize,
    {
        let id = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let json = call::<R>(self.server_tx.clone(), id, params).await?;
        Ok(serde_json::from_value(json)?)
    }
    pub async fn stack_trace(
        &self,
        thread: ThreadId,
        guard: &RequestGuard,
    ) -> Option<Result<requests::StackTraceResponse>> {
        tokio::select! {
            biased;
            _ = guard.canceled() => None,
            result = self.request::<requests::StackTrace>(requests::StackTraceArguments {
                thread_id: thread, start_frame: None, levels: None, format: None,
            }) => Some(result),
        }
    }

    pub async fn variables_for_frame(
        &self,
        frame: usize,
        stop: &RequestGuard,
        selection: &RequestGuard,
        variables: &RequestGuard,
    ) -> Option<Result<Vec<(Scope, Result<requests::VariablesResponse>)>>> {
        let load = async {
            let scopes = self
                .request::<requests::Scopes>(requests::ScopesArguments { frame_id: frame })
                .await?
                .scopes;
            Ok(
                futures_util::stream::iter(scopes.into_iter().map(|scope| async move {
                    let response = self
                        .request::<requests::Variables>(requests::VariablesArguments {
                            variables_reference: scope.variables_reference,
                            filter: None,
                            start: None,
                            count: None,
                            format: None,
                        })
                        .await;
                    (scope, response)
                }))
                .buffered(4)
                .collect()
                .await,
            )
        };
        tokio::select! {
            biased;
            _ = stop.canceled() => None,
            _ = selection.canceled() => None,
            _ = variables.canceled() => None,
            result = load => Some(result),
        }
    }
}

#[derive(Debug)]
pub struct Client {
    id: DebugAdapterId,
    _process: Option<Child>,
    server_tx: UnboundedSender<Payload>,
    request_counter: Arc<AtomicU64>,
    stop_epoch: Arc<RequestEpoch>,
    selection_epoch: Arc<RequestEpoch>,
    variables_epoch: Arc<RequestEpoch>,
    resume_pending: bool,
    stack_request_counter: u64,
    pending_stack_traces: HashMap<ThreadId, u64>,
    connection_type: Option<ConnectionType>,
    starting_request_args: Option<Value>,
    /// The socket address of the debugger, if using TCP transport.
    pub socket: Option<SocketAddr>,
    pub caps: Option<DebuggerCapabilities>,
    // thread_id -> frames
    pub stack_frames: HashMap<ThreadId, Vec<StackFrame>>,
    pub thread_states: ThreadStates,
    pub progress: ProgressMap,
    pub thread_id: Option<ThreadId>,
    /// Currently active frame for the current thread.
    pub active_frame: Option<usize>,
    pub quirks: DebuggerQuirks,
    /// The config which was used to start this debugger.
    pub config: Option<DebugAdapterConfig>,
}

async fn call<R: helix_dap_types::Request>(
    server_tx: UnboundedSender<Payload>,
    id: u64,
    arguments: R::Arguments,
) -> Result<Value>
where
    R::Arguments: serde::Serialize,
{
    use std::time::Duration;
    use tokio::time::timeout;

    let arguments = Some(serde_json::to_value(arguments)?);

    let (callback_tx, mut callback_rx) = channel(1);

    let req = Request {
        back_ch: Some(callback_tx),
        seq: id,
        command: R::COMMAND.to_string(),
        arguments,
    };

    server_tx
        .send(Payload::Request(req))
        .map_err(|e| Error::Other(e.into()))?;

    // TODO: specifiable timeout, delay other calls until initialize success
    let response = timeout(Duration::from_secs(20), callback_rx.recv())
        .await
        .map_err(|_| Error::Timeout(id))? // return Timeout
        .ok_or(Error::StreamClosed)??;

    if !response.success {
        let message = response
            .message
            .clone()
            .unwrap_or_else(|| "DAP request failed".to_string());
        return Err(Error::Other(anyhow!(message)));
    }

    Ok(response.body.unwrap_or_default())
}

impl Drop for Client {
    fn drop(&mut self) {
        self.invalidate_stop_requests();
    }
}

impl Client {
    // Spawn a process and communicate with it by either TCP or stdio
    // The returned stream includes the Client ID so consumers can differentiate between multiple clients
    pub async fn process(
        transport: &str,
        command: &str,
        args: Vec<&str>,
        port_arg: Option<&str>,
        id: DebugAdapterId,
    ) -> Result<(Self, UnboundedReceiver<(DebugAdapterId, Payload)>)> {
        if command.is_empty() {
            return Result::Err(Error::Other(anyhow!("Command not provided")));
        }
        match (transport, port_arg) {
            ("tcp", Some(port_arg)) => Self::tcp_process(command, args, port_arg, id).await,
            ("stdio", _) => Self::stdio(command, args, id),
            _ => Result::Err(Error::Other(anyhow!("Incorrect transport {}", transport))),
        }
    }

    pub fn streams(
        rx: Box<dyn AsyncBufRead + Unpin + Send>,
        tx: Box<dyn AsyncWrite + Unpin + Send>,
        err: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
        id: DebugAdapterId,
        process: Option<Child>,
    ) -> Result<(Self, UnboundedReceiver<(DebugAdapterId, Payload)>)> {
        let (server_rx, server_tx) = Transport::start(rx, tx, err, id);
        let (client_tx, client_rx) = unbounded_channel();

        let client = Self {
            id,
            _process: process,
            server_tx,
            request_counter: Arc::new(AtomicU64::new(0)),
            stop_epoch: Arc::default(),
            selection_epoch: Arc::default(),
            variables_epoch: Arc::default(),
            resume_pending: false,
            stack_request_counter: 0,
            pending_stack_traces: HashMap::new(),
            caps: None,
            connection_type: None,
            starting_request_args: None,
            socket: None,
            stack_frames: HashMap::new(),
            thread_states: HashMap::new(),
            progress: HashMap::new(),
            thread_id: None,
            active_frame: None,
            quirks: DebuggerQuirks::default(),
            config: None,
        };

        tokio::spawn(Self::recv(id, server_rx, client_tx));

        Ok((client, client_rx))
    }

    pub async fn tcp(
        addr: std::net::SocketAddr,
        id: DebugAdapterId,
    ) -> Result<(Self, UnboundedReceiver<(DebugAdapterId, Payload)>)> {
        let stream = TcpStream::connect(addr).await?;
        let (rx, tx) = stream.into_split();
        Self::streams(Box::new(BufReader::new(rx)), Box::new(tx), None, id, None)
    }

    pub fn stdio(
        cmd: &str,
        args: Vec<&str>,
        id: DebugAdapterId,
    ) -> Result<(Self, UnboundedReceiver<(DebugAdapterId, Payload)>)> {
        // Resolve path to the binary
        let cmd = helix_stdx::env::which(cmd)?;

        let process = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // make sure the process is reaped on drop
            .kill_on_drop(true)
            .spawn();

        let mut process = process?;

        // TODO: do we need bufreader/writer here? or do we use async wrappers on unblock?
        let writer = BufWriter::new(process.stdin.take().expect("Failed to open stdin"));
        let reader = BufReader::new(process.stdout.take().expect("Failed to open stdout"));
        let stderr = BufReader::new(process.stderr.take().expect("Failed to open stderr"));

        Self::streams(
            Box::new(reader),
            Box::new(writer),
            Some(Box::new(stderr)),
            id,
            Some(process),
        )
    }

    async fn get_port() -> Option<u16> {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                0,
            ))
            .await
            .ok()?
            .local_addr()
            .ok()?
            .port(),
        )
    }

    pub fn starting_request_args(&self) -> Option<&Value> {
        self.starting_request_args.as_ref()
    }

    pub async fn tcp_process(
        cmd: &str,
        args: Vec<&str>,
        port_format: &str,
        id: DebugAdapterId,
    ) -> Result<(Self, UnboundedReceiver<(DebugAdapterId, Payload)>)> {
        let port = Self::get_port().await.unwrap();

        let process = Command::new(cmd)
            .args(args)
            .args(port_format.replace("{}", &port.to_string()).split(' '))
            // silence messages
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Do not kill debug adapter when leaving, it should exit automatically
            .spawn()?;

        // Wait for adapter to become ready for connection
        time::sleep(time::Duration::from_millis(500)).await;
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
        let stream = TcpStream::connect(socket).await?;

        let (rx, tx) = stream.into_split();
        let mut result = Self::streams(
            Box::new(BufReader::new(rx)),
            Box::new(tx),
            None,
            id,
            Some(process),
        );

        // Set the socket address for the client
        if let Ok((client, _)) = &mut result {
            client.socket = Some(socket);
        }

        result
    }

    async fn recv(
        id: DebugAdapterId,
        mut server_rx: UnboundedReceiver<Payload>,
        client_tx: UnboundedSender<(DebugAdapterId, Payload)>,
    ) {
        while let Some(msg) = server_rx.recv().await {
            match msg {
                Payload::Event(ev) => {
                    client_tx
                        .send((id, Payload::Event(ev)))
                        .expect("Failed to send");
                }
                Payload::Response(_) => unreachable!(),
                Payload::Request(req) => {
                    client_tx
                        .send((id, Payload::Request(req)))
                        .expect("Failed to send");
                }
            }
        }
    }

    pub fn id(&self) -> DebugAdapterId {
        self.id
    }

    pub fn connection_type(&self) -> Option<ConnectionType> {
        self.connection_type
    }

    fn next_request_id(&self) -> u64 {
        // > The `seq` for the first message sent by a client or debug adapter
        // > is 1, and for each subsequent message is 1 greater than the
        // > previous message sent by that actor
        // <https://microsoft.github.io/debug-adapter-protocol/specification#Base_Protocol_ProtocolMessage>
        self.request_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn requester(&self) -> Requester {
        Requester {
            server_tx: self.server_tx.clone(),
            request_counter: self.request_counter.clone(),
        }
    }
    pub fn begin_stack_trace(
        &mut self,
        thread: ThreadId,
    ) -> Option<(Requester, RequestGuard, u64)> {
        if self.stack_frames.contains_key(&thread)
            || self.pending_stack_traces.contains_key(&thread)
        {
            return None;
        }
        self.stack_request_counter += 1;
        let request = self.stack_request_counter;
        self.pending_stack_traces.insert(thread, request);
        Some((self.requester(), self.stop_guard(), request))
    }
    pub fn finish_stack_trace(&mut self, thread: ThreadId, request: u64) -> bool {
        if self.pending_stack_traces.get(&thread) != Some(&request) {
            return false;
        }
        self.pending_stack_traces.remove(&thread);
        true
    }
    pub fn invalidate_thread_stack(&mut self, thread: ThreadId) {
        self.stack_frames.remove(&thread);
        self.pending_stack_traces.remove(&thread);
    }
    pub fn stop_guard(&self) -> RequestGuard {
        self.stop_epoch.guard()
    }
    pub fn selection_guard(&self) -> RequestGuard {
        self.selection_epoch.guard()
    }
    pub fn variables_guard(&mut self) -> RequestGuard {
        self.variables_epoch.invalidate();
        self.variables_epoch.guard()
    }
    pub fn is_resuming(&self) -> bool {
        self.resume_pending
    }
    pub fn begin_resume(&mut self) {
        self.invalidate_stop_requests();
        self.resume_pending = true;
    }
    pub fn cancel_resume(&mut self) {
        self.resume_pending = false;
    }
    pub fn invalidate_stop_requests(&mut self) {
        self.resume_pending = false;
        self.stop_epoch.invalidate();
        self.selection_epoch.invalidate();
        self.variables_epoch.invalidate();
        self.pending_stack_traces.clear();
    }
    pub fn begin_stop(&mut self) {
        self.invalidate_stop_requests();
        self.stack_frames.clear();
        self.active_frame = None;
    }
    pub fn invalidate_selection_requests(&mut self) {
        self.selection_epoch.invalidate();
        self.variables_epoch.invalidate();
    }
    pub fn select_thread(&mut self, id: ThreadId) {
        self.invalidate_selection_requests();
        self.thread_id = Some(id);
        self.active_frame = None;
    }
    pub fn select_frame(&mut self, frame: usize) {
        self.invalidate_selection_requests();
        self.active_frame = Some(frame);
    }

    // Internal, called by specific DAP commands when resuming
    pub fn resume_application(&mut self) {
        self.invalidate_stop_requests();
        // A continue may resume every thread. Results from the previous stop
        // must not be reused when selecting another thread afterward.
        self.stack_frames.clear();
        if let Some(thread_id) = self.thread_id {
            self.thread_states.insert(thread_id, "running".to_string());
        }
        self.active_frame = None;
        self.thread_id = None;
    }

    /// Execute a RPC request on the debugger.
    pub fn call<R: helix_dap_types::Request>(
        &self,
        arguments: R::Arguments,
    ) -> impl Future<Output = Result<Value>>
    where
        R::Arguments: serde::Serialize,
    {
        call::<R>(self.server_tx.clone(), self.next_request_id(), arguments)
    }

    pub async fn request<R: helix_dap_types::Request>(
        &self,
        params: R::Arguments,
    ) -> Result<R::Result>
    where
        R::Arguments: serde::Serialize,
    {
        // a future that resolves into the response
        let json = self.call::<R>(params).await?;
        let response = serde_json::from_value(json)?;
        Ok(response)
    }

    pub fn reply(
        &self,
        request_seq: u64,
        command: &str,
        result: core::result::Result<Value, Error>,
    ) -> impl Future<Output = Result<()>> {
        let server_tx = self.server_tx.clone();
        let command = command.to_string();

        async move {
            let response = match result {
                Ok(result) => Response {
                    request_seq,
                    command,
                    success: true,
                    message: None,
                    body: Some(result),
                },
                Err(error) => Response {
                    request_seq,
                    command,
                    success: false,
                    message: Some(error.to_string()),
                    body: None,
                },
            };

            server_tx
                .send(Payload::Response(response))
                .map_err(|e| Error::Other(e.into()))?;

            Ok(())
        }
    }

    pub fn capabilities(&self) -> &DebuggerCapabilities {
        self.caps.as_ref().expect("debugger not yet initialized!")
    }

    pub fn progress_start(&mut self, event: events::ProgressStartBody) -> String {
        let status = ProgressState::new(event.title, event.message, event.percentage);
        let status_line = status.status_line();
        self.progress.insert(event.progress_id, status);
        status_line
    }

    pub fn progress_update(&mut self, event: events::ProgressUpdateBody) -> Option<String> {
        let status = self.progress.get_mut(&event.progress_id)?;
        status.update(event.message, event.percentage);
        Some(status.status_line())
    }

    pub fn progress_end(&mut self, event: events::ProgressEndBody) -> Option<String> {
        let status = self.progress.remove(&event.progress_id)?;
        Some(status.end_status_line(event.message.as_deref()))
    }

    pub async fn initialize(
        &mut self,
        adapter_id: String,
        supports_run_in_terminal: bool,
    ) -> Result<()> {
        let args = requests::InitializeArguments {
            client_id: Some("hx".to_owned()),
            client_name: Some("helix".to_owned()),
            adapter_id,
            locale: Some("en-us".to_owned()),
            lines_start_at_one: Some(true),
            columns_start_at_one: Some(true),
            path_format: Some("path".to_owned()),
            supports_variable_type: Some(true),
            supports_variable_paging: Some(false),
            supports_run_in_terminal_request: Some(supports_run_in_terminal),
            supports_memory_references: Some(false),
            supports_progress_reporting: Some(true),
            supports_invalidated_event: Some(false),
        };

        let response = self.request::<requests::Initialize>(args).await?;
        self.caps = Some(response);

        Ok(())
    }

    pub fn disconnect(
        &mut self,
        args: Option<DisconnectArguments>,
    ) -> impl Future<Output = Result<Value>> {
        self.invalidate_stop_requests();
        self.connection_type = None;
        self.call::<requests::Disconnect>(args)
    }

    pub fn terminate(
        &mut self,
        args: Option<TerminateArguments>,
    ) -> impl Future<Output = Result<Value>> {
        self.invalidate_stop_requests();
        self.connection_type = None;
        self.call::<requests::Terminate>(args)
    }

    pub fn launch(&mut self, args: serde_json::Value) -> impl Future<Output = Result<Value>> {
        self.invalidate_stop_requests();
        self.connection_type = Some(ConnectionType::Launch);
        self.starting_request_args = Some(args.clone());
        self.call::<requests::Launch>(args)
    }

    pub fn attach(&mut self, args: serde_json::Value) -> impl Future<Output = Result<Value>> {
        self.invalidate_stop_requests();
        self.connection_type = Some(ConnectionType::Attach);
        self.starting_request_args = Some(args.clone());
        self.call::<requests::Attach>(args)
    }

    pub fn restart(&mut self) -> impl Future<Output = Result<Value>> {
        self.invalidate_stop_requests();
        let args = if let Some(args) = &self.starting_request_args {
            args.clone()
        } else {
            Value::Null
        };
        self.call::<requests::Restart>(args)
    }

    pub async fn set_breakpoints(
        &self,
        file: PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
    ) -> Result<Option<Vec<Breakpoint>>> {
        let args = requests::SetBreakpointsArguments {
            source: Source {
                path: Some(file),
                name: None,
                source_reference: None,
                presentation_hint: None,
                origin: None,
                sources: None,
                adapter_data: None,
                checksums: None,
            },
            breakpoints: Some(breakpoints),
            source_modified: Some(false),
        };

        let response = self.request::<requests::SetBreakpoints>(args).await?;

        Ok(response.breakpoints)
    }

    pub async fn configuration_done(&self) -> Result<()> {
        if !self
            .caps
            .as_ref()
            .and_then(|caps| caps.supports_configuration_done_request)
            .unwrap_or(false)
        {
            return Ok(());
        }

        self.call::<requests::ConfigurationDone>(Some(requests::ConfigurationDoneArguments {}))
            .await?;

        Ok(())
    }

    pub fn continue_thread(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::ContinueArguments { thread_id };

        self.call::<requests::Continue>(args)
    }

    pub async fn stack_trace(
        &self,
        thread_id: ThreadId,
    ) -> Result<(Vec<StackFrame>, Option<usize>)> {
        let args = requests::StackTraceArguments {
            thread_id,
            start_frame: None,
            levels: None,
            format: None,
        };

        let response = self.request::<requests::StackTrace>(args).await?;
        Ok((response.stack_frames, response.total_frames))
    }

    pub fn threads(&self) -> impl Future<Output = Result<Value>> {
        self.call::<requests::Threads>(Some(requests::ThreadsArguments {}))
    }

    pub async fn scopes(&self, frame_id: usize) -> Result<Vec<Scope>> {
        let args = requests::ScopesArguments { frame_id };

        let response = self.request::<requests::Scopes>(args).await?;
        Ok(response.scopes)
    }

    pub async fn variables(&self, variables_reference: usize) -> Result<Vec<Variable>> {
        let args = requests::VariablesArguments {
            variables_reference,
            filter: None,
            start: None,
            count: None,
            format: None,
        };

        let response = self.request::<requests::Variables>(args).await?;
        Ok(response.variables)
    }

    pub fn step_in(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::StepInArguments {
            thread_id,
            target_id: None,
            granularity: None,
        };

        self.call::<requests::StepIn>(args)
    }

    pub fn step_out(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::StepOutArguments {
            thread_id,
            granularity: None,
        };

        self.call::<requests::StepOut>(args)
    }

    pub fn next(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::NextArguments {
            thread_id,
            granularity: None,
        };

        self.call::<requests::Next>(args)
    }

    pub fn pause(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::PauseArguments { thread_id };

        self.call::<requests::Pause>(args)
    }

    pub async fn eval(
        &self,
        expression: String,
        frame_id: Option<usize>,
    ) -> Result<requests::EvaluateResponse> {
        let args = requests::EvaluateArguments {
            expression,
            frame_id,
            context: None,
            format: None,
        };

        self.request::<requests::Evaluate>(args).await
    }

    pub fn set_exception_breakpoints(
        &self,
        filters: Vec<String>,
    ) -> impl Future<Output = Result<Value>> {
        let args = requests::SetExceptionBreakpointsArguments { filters };

        self.call::<requests::SetExceptionBreakpoints>(args)
    }

    pub fn current_stack_frame(&self) -> Option<&StackFrame> {
        self.stack_frames
            .get(&self.thread_id?)?
            .get(self.active_frame?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn thread_id(value: isize) -> ThreadId {
        serde_json::from_value(serde_json::json!(value)).unwrap()
    }

    fn client_streams() -> (
        Client,
        UnboundedReceiver<(DebugAdapterId, Payload)>,
        BufReader<DuplexStream>,
    ) {
        let (client, adapter) = tokio::io::duplex(4096);
        let (rx, tx) = tokio::io::split(client);
        let (client, incoming) = Client::streams(
            Box::new(BufReader::new(rx)),
            Box::new(tx),
            None,
            DebugAdapterId::default(),
            None,
        )
        .unwrap();
        (client, incoming, BufReader::new(adapter))
    }

    async fn read_message(reader: &mut BufReader<DuplexStream>) -> Value {
        let mut line = String::new();
        let mut content_length = None;
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(length) = line.strip_prefix("Content-Length: ") {
                content_length = Some(length.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; content_length.unwrap()];
        reader.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn reply_message(adapter: &mut BufReader<DuplexStream>, request: &Value, body: Value) {
        use tokio::io::AsyncWriteExt;
        let response = serde_json::json!({
            "type": "response", "seq": 1, "request_seq": request["seq"],
            "success": true, "command": request["command"], "body": body,
        })
        .to_string();
        adapter
            .get_mut()
            .write_all(format!("Content-Length: {}\r\n\r\n{response}", response.len()).as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn slow_stack_request_deduplicates_and_new_stop_discards_late_response() {
        let (mut client, _incoming, mut adapter) = client_streams();
        client.begin_stop();
        client.select_thread(thread_id(7));
        let (requester, guard, _) = client.begin_stack_trace(thread_id(7)).unwrap();
        assert!(client.begin_stack_trace(thread_id(7)).is_none());
        let old = tokio::spawn(async move { requester.stack_trace(thread_id(7), &guard).await });
        let request = tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
            .await
            .unwrap();
        assert_eq!(request["command"], "stackTrace");
        client.begin_stop();
        assert!(tokio::time::timeout(Duration::from_secs(1), old)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        reply_message(
            &mut adapter,
            &request,
            serde_json::json!({"stackFrames": []}),
        )
        .await;
        let (requester, guard, _) = client.begin_stack_trace(thread_id(7)).unwrap();
        let current =
            tokio::spawn(async move { requester.stack_trace(thread_id(7), &guard).await });
        let request = tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
            .await
            .unwrap();
        reply_message(
            &mut adapter,
            &request,
            serde_json::json!({"stackFrames": []}),
        )
        .await;
        assert!(tokio::time::timeout(Duration::from_secs(1), current)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn changed_frame_cancels_scopes_before_variable_requests() {
        let (mut client, _incoming, mut adapter) = client_streams();
        client.begin_stop();
        client.select_thread(thread_id(7));
        client.select_frame(0);
        let requester = client.requester();
        let stop = client.stop_guard();
        let selection = client.selection_guard();
        let variables = client.variables_guard();
        let load = tokio::spawn(async move {
            requester
                .variables_for_frame(3, &stop, &selection, &variables)
                .await
        });
        let request = tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
            .await
            .unwrap();
        assert_eq!(request["command"], "scopes");
        client.select_frame(1);
        assert!(tokio::time::timeout(Duration::from_secs(1), load)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        reply_message(&mut adapter, &request, serde_json::json!({"scopes": [{"name":"locals", "variablesReference":9, "expensive":false}]})).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(25), read_message(&mut adapter))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn variables_requests_are_bounded_and_results_preserve_scope_order() {
        let (mut client, _incoming, mut adapter) = client_streams();
        client.begin_stop();
        client.select_thread(thread_id(7));
        client.select_frame(0);
        let requester = client.requester();
        let stop = client.stop_guard();
        let selection = client.selection_guard();
        let variables = client.variables_guard();
        let load = tokio::spawn(async move {
            requester
                .variables_for_frame(3, &stop, &selection, &variables)
                .await
        });
        let request = tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
            .await
            .unwrap();
        let scopes: Vec<_> = (1..=9).map(|i| serde_json::json!({"name":format!("scope{i}"), "variablesReference":i, "expensive":false})).collect();
        reply_message(&mut adapter, &request, serde_json::json!({"scopes":scopes})).await;
        for batch_size in [4, 4, 1] {
            let mut batch = Vec::new();
            for _ in 0..batch_size {
                let request =
                    tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
                        .await
                        .unwrap();
                assert_eq!(request["command"], "variables");
                batch.push(request);
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(25), read_message(&mut adapter))
                    .await
                    .is_err(),
                "more than four requests were outstanding"
            );
            for request in batch.iter().rev() {
                reply_message(&mut adapter, request, serde_json::json!({"variables":[]})).await;
            }
        }
        let loaded = tokio::time::timeout(Duration::from_secs(1), load)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|(scope, _)| scope.name.as_str())
                .collect::<Vec<_>>(),
            (1..=9).map(|i| format!("scope{i}")).collect::<Vec<_>>()
        );
        assert!(loaded.into_iter().all(|(_, variables)| variables.is_ok()));
    }

    #[tokio::test]
    async fn continued_thread_old_stack_response_cannot_consume_a_new_pending_request() {
        let (mut client, _incoming, _adapter) = client_streams();
        client.begin_stop();
        let (_, stopped, old) = client.begin_stack_trace(thread_id(7)).unwrap();
        client.invalidate_thread_stack(thread_id(7));
        let (_, still_stopped, new) = client.begin_stack_trace(thread_id(7)).unwrap();
        assert!(stopped.is_current() && still_stopped.is_current());
        assert!(
            !client.finish_stack_trace(thread_id(7), old),
            "old response must not remove the replacement request"
        );
        assert!(
            client.begin_stack_trace(thread_id(7)).is_none(),
            "replacement remains pending"
        );
        assert!(client.finish_stack_trace(thread_id(7), new));
    }

    #[tokio::test]
    async fn stop_selection_resume_and_session_changes_invalidate_guards() {
        let (mut client, _incoming, _adapter) = client_streams();
        let stop = client.stop_guard();
        client.select_thread(thread_id(3));
        assert!(stop.is_current());
        let selected = client.selection_guard();
        client.select_frame(0);
        assert!(!selected.is_current());
        let selected = client.selection_guard();
        let variables = client.variables_guard();
        client.select_thread(thread_id(4));
        assert!(!selected.is_current());
        assert!(!variables.is_current());
        let stop = client.stop_guard();
        client.stack_frames.insert(thread_id(3), Vec::new());
        client.stack_frames.insert(thread_id(4), Vec::new());
        client.resume_application();
        assert!(
            client.stack_frames.is_empty(),
            "other resumed threads must not retain cached frames"
        );
        assert!(!stop.is_current());
        let stop = client.stop_guard();
        client.begin_stop();
        assert!(!stop.is_current());
        let stop = client.stop_guard();
        drop(client);
        assert!(!stop.is_current());
    }

    #[tokio::test]
    async fn initialize_advertises_terminal_support_only_when_available() {
        for supported in [false, true] {
            let (mut client, _incoming, mut adapter) = client_streams();
            let exchange = async {
                let adapter_reply = async {
                    let request = read_message(&mut adapter).await;
                    assert_eq!(request["command"], "initialize");
                    assert_eq!(
                        request["arguments"]["supportsRunInTerminalRequest"],
                        supported
                    );

                    let response = serde_json::json!({
                        "type": "response",
                        "seq": 1,
                        "request_seq": request["seq"],
                        "success": true,
                        "command": "initialize",
                        "body": {},
                    })
                    .to_string();
                    adapter
                        .get_mut()
                        .write_all(
                            format!("Content-Length: {}\r\n\r\n{response}", response.len())
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                };
                let (initialized, ()) = tokio::join!(
                    client.initialize("test-adapter".into(), supported),
                    adapter_reply,
                );
                initialized.unwrap();
            };
            tokio::time::timeout(Duration::from_secs(1), exchange)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn failed_reverse_requests_send_error_responses() {
        let (client, _incoming, mut adapter) = client_streams();
        client
            .reply(
                7,
                "runInTerminal",
                Err(Error::Other(anyhow!("No external terminal defined"))),
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), read_message(&mut adapter))
            .await
            .unwrap();

        assert_eq!(response["type"], "response");
        assert_eq!(response["request_seq"], 7);
        assert_eq!(response["command"], "runInTerminal");
        assert_eq!(response["success"], false);
        assert_eq!(response["message"], "No external terminal defined");
    }
}
