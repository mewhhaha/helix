use crate::editor::{Action, Breakpoint, TerminalConfig};
use crate::{align_view, Align, Editor};
use anyhow::bail;
use dap::requests::DisconnectArguments;
use dap::requests::ThreadsArguments;
use helix_core::Selection;
use helix_dap::{
    self as dap, registry::DebugAdapterId, ConnectionType, Payload, Request, ThreadId,
};
use helix_lsp::block_on;
use log::{error, warn};
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::PathBuf;

#[macro_export]
macro_rules! debugger {
    ($editor:expr) => {{
        let Some(debugger) = $editor.debug_adapters.get_active_client_mut() else {
            return;
        };
        debugger
    }};
}

// general utils:
pub fn dap_pos_to_pos(doc: &helix_core::Rope, line: usize, column: usize) -> Option<usize> {
    // 1-indexing to 0 indexing
    let line = doc.try_line_to_char(line - 1).ok()?;
    let pos = line + column.saturating_sub(1);
    // TODO: this is probably utf-16 offsets
    Some(pos)
}

pub fn select_thread_id(editor: &mut Editor, thread_id: ThreadId, force: bool) {
    let debugger = debugger!(editor);
    if !force && debugger.thread_id.is_some() {
        return;
    }
    let id = debugger.id();
    debugger.select_thread(thread_id);
    if let Some(frame) = debugger
        .stack_frames
        .get(&thread_id)
        .and_then(|frames| frames.first())
        .cloned()
    {
        debugger.select_frame(0);
        jump_to_stack_frame(editor, &frame);
    } else {
        editor.request_stack_trace(id, thread_id);
    }
}

pub(crate) enum PreparedDap {
    Stack {
        id: DebugAdapterId,
        thread: ThreadId,
        request: u64,
        guard: dap::RequestGuard,
        frames: dap::Result<dap::requests::StackTraceResponse>,
    },
    Threads {
        id: DebugAdapterId,
        guard: dap::RequestGuard,
        reason: String,
        threads: dap::Result<dap::requests::ThreadsResponse>,
    },
}

pub fn jump_to_stack_frame(editor: &mut Editor, frame: &helix_dap::StackFrame) {
    let path = if let Some(helix_dap::Source {
        path: Some(ref path),
        ..
    }) = frame.source
    {
        path.clone()
    } else {
        return;
    };

    if let Err(e) = editor.open(&path, Action::Replace) {
        editor.set_error(format!("Unable to jump to stack frame: {}", e));
        return;
    }

    let (view, doc) = current!(editor);

    let text_end = doc.text().len_chars().saturating_sub(1);
    let start = dap_pos_to_pos(doc.text(), frame.line, frame.column).unwrap_or(0);
    let end = frame
        .end_line
        .and_then(|end_line| dap_pos_to_pos(doc.text(), end_line, frame.end_column.unwrap_or(0)))
        .unwrap_or(start);

    let selection = Selection::single(start.min(text_end), end.min(text_end));
    doc.set_selection(view.id, selection);
    align_view(doc, view, Align::Center);
}

pub fn breakpoints_changed(
    debugger: &mut dap::Client,
    path: PathBuf,
    breakpoints: &mut [Breakpoint],
) -> Result<(), anyhow::Error> {
    if let Some(caps) = debugger.caps.as_ref() {
        if breakpoints.iter().any(|b| b.condition.is_some())
            && !caps.supports_conditional_breakpoints.unwrap_or_default()
        {
            bail!("Can't edit breakpoint: debugger does not support conditional breakpoints")
        }
        if breakpoints.iter().any(|b| b.hit_condition.is_some())
            && !caps
                .supports_hit_conditional_breakpoints
                .unwrap_or_default()
        {
            bail!("Can't edit breakpoint: debugger does not support hit conditional breakpoints")
        }
        if breakpoints.iter().any(|b| b.log_message.is_some())
            && !caps.supports_log_points.unwrap_or_default()
        {
            bail!("Can't edit breakpoint: debugger does not support logpoints")
        }
    }
    let source_breakpoints = breakpoints
        .iter()
        .map(|breakpoint| helix_dap::SourceBreakpoint {
            line: breakpoint.line + 1, // convert from 0-indexing to 1-indexing (TODO: could set debugger to 0-indexing on init)
            column: breakpoint.column,
            condition: breakpoint.condition.clone(),
            hit_condition: breakpoint.hit_condition.clone(),
            log_message: breakpoint.log_message.clone(),
        })
        .collect::<Vec<_>>();

    let request = debugger.set_breakpoints(path, source_breakpoints);
    match block_on(request) {
        Ok(Some(dap_breakpoints)) => {
            for (breakpoint, dap_breakpoint) in breakpoints.iter_mut().zip(dap_breakpoints) {
                breakpoint.id = dap_breakpoint.id;
                breakpoint.verified = dap_breakpoint.verified;
                breakpoint.message = dap_breakpoint.message;
                // TODO: handle breakpoint.message
                // TODO: verify source matches
                breakpoint.line = dap_breakpoint.line.unwrap_or(0).saturating_sub(1); // convert to 0-indexing
                                                                                      // TODO: no unwrap
                breakpoint.column = dap_breakpoint.column;
                // TODO: verify end_linef/col instruction reference, offset
            }
        }
        Err(e) => anyhow::bail!("Failed to set breakpoints: {}", e),
        _ => {}
    };
    Ok(())
}

fn run_in_terminal(
    terminal: Option<&TerminalConfig>,
    arguments: &dap::requests::RunInTerminalArguments,
) -> dap::Result<dap::requests::RunInTerminalResponse> {
    let terminal = terminal
        .ok_or_else(|| dap::Error::Other(anyhow::anyhow!("No external terminal defined")))?;
    let process = std::process::Command::new(&terminal.command)
        .args(&terminal.args)
        .args(&arguments.args)
        .spawn()
        .map_err(|err| {
            dap::Error::Other(anyhow::anyhow!("Error starting external terminal: {err}"))
        })?;

    Ok(dap::requests::RunInTerminalResponse {
        process_id: Some(process.id()),
        shell_process_id: None,
    })
}

impl Editor {
    fn request_stack_trace(&mut self, id: DebugAdapterId, thread: ThreadId) {
        let Some(debugger) = self.debug_adapters.get_client_mut(id) else {
            return;
        };
        let Some((requester, guard, request)) = debugger.begin_stack_trace(thread) else {
            return;
        };
        self.dap_tasks.push(tokio::spawn(async move {
            let frames = requester.stack_trace(thread, &guard).await?;
            Some(PreparedDap::Stack {
                id,
                thread,
                request,
                guard,
                frames,
            })
        }));
    }

    pub(crate) fn apply_prepared_dap(&mut self, prepared: PreparedDap) -> bool {
        match prepared {
            PreparedDap::Stack {
                id,
                thread,
                request,
                guard,
                frames,
            } => {
                if !guard.is_current() {
                    return false;
                }
                let active = self
                    .debug_adapters
                    .get_active_client()
                    .is_some_and(|client| client.id() == id);
                let Some(debugger) = self.debug_adapters.get_client_mut(id) else {
                    return false;
                };
                if !debugger.finish_stack_trace(thread, request) {
                    return false;
                }
                let frames = match frames {
                    Ok(response) => response.stack_frames,
                    Err(err) => {
                        self.set_error(format!("Failed to get stack frames: {err}"));
                        return true;
                    }
                };
                let selected =
                    debugger.thread_id == Some(thread) && debugger.active_frame.is_none();
                let frame = selected.then(|| frames.first().cloned()).flatten();
                debugger.stack_frames.insert(thread, frames);
                if selected {
                    debugger.select_frame(0);
                }
                if let Some(frame) = frame.filter(|_| active) {
                    jump_to_stack_frame(self, &frame);
                }
            }
            PreparedDap::Threads {
                id,
                guard,
                reason,
                threads,
            } => {
                if !guard.is_current() {
                    return false;
                }
                let active = self
                    .debug_adapters
                    .get_active_client()
                    .is_some_and(|client| client.id() == id);
                let Some(debugger) = self.debug_adapters.get_client_mut(id) else {
                    return false;
                };
                let threads = match threads {
                    Ok(response) => response.threads,
                    Err(err) => {
                        self.set_error(format!("Failed to get threads: {err}"));
                        return true;
                    }
                };
                for thread in &threads {
                    debugger.thread_states.insert(thread.id, reason.clone());
                }
                if active && debugger.thread_id.is_none() {
                    if let Some(thread) = threads.first() {
                        select_thread_id(self, thread.id, false);
                    }
                }
            }
        }
        true
    }

    pub async fn handle_debugger_message(
        &mut self,
        id: DebugAdapterId,
        payload: helix_dap::Payload,
    ) -> bool {
        use helix_dap::{events, Event};

        match payload {
            Payload::Event(event) => {
                let event = match Event::parse(&event.event, event.body) {
                    Ok(event) => event,
                    Err(dap::Error::Unhandled) => {
                        log::info!("Discarding unknown DAP event '{}'", event.event);
                        return false;
                    }
                    Err(err) => {
                        log::warn!("Discarding invalid DAP event '{}': {err}", event.event);
                        return false;
                    }
                };
                match event {
                    Event::Stopped(events::StoppedBody {
                        thread_id,
                        description,
                        text,
                        reason,
                        all_threads_stopped,
                        ..
                    }) => {
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => return false,
                        };

                        let all_threads_stopped = all_threads_stopped.unwrap_or_default();

                        let selected = if debugger.is_resuming() {
                            thread_id.or(debugger.thread_id)
                        } else {
                            debugger.thread_id.or(thread_id)
                        };
                        debugger.begin_stop();
                        if let Some(thread) = thread_id {
                            debugger.thread_states.insert(thread, reason.clone());
                        }
                        if let Some(thread) = selected {
                            debugger.select_thread(thread);
                        }
                        // Fetch the selected stack first; other stacks are loaded on demand
                        // when switching threads rather than delaying the stop event.
                        let threads_request = all_threads_stopped
                            .then(|| (debugger.requester(), debugger.stop_guard()));
                        if let Some(thread) = selected {
                            self.request_stack_trace(id, thread);
                        }
                        if let Some((requester, guard)) = threads_request {
                            let reason = reason.clone();
                            self.dap_tasks.push(tokio::spawn(async move {
                                let threads = tokio::select! {
                                    biased;
                                    _ = guard.canceled() => return None,
                                    result = requester.request::<dap::requests::Threads>(Some(ThreadsArguments {})) => result,
                                };
                                Some(PreparedDap::Threads { id, guard, reason, threads })
                            }));
                        }

                        let scope = match thread_id {
                            Some(id) => format!("Thread {}", id),
                            None => "Target".to_owned(),
                        };

                        let mut status = format!("{} stopped because of {}", scope, reason);
                        if let Some(desc) = description {
                            write!(status, " {}", desc).unwrap();
                        }
                        if let Some(text) = text {
                            write!(status, " {}", text).unwrap();
                        }
                        if all_threads_stopped {
                            status.push_str(" (all threads stopped)");
                        }

                        self.set_status(status);
                    }
                    Event::Continued(events::ContinuedBody {
                        thread_id,
                        all_threads_continued,
                    }) => {
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => return false,
                        };

                        debugger
                            .thread_states
                            .insert(thread_id, "running".to_owned());
                        debugger.invalidate_thread_stack(thread_id);
                        if all_threads_continued.unwrap_or(true)
                            || debugger.thread_id == Some(thread_id)
                        {
                            debugger.resume_application();
                        }
                    }
                    Event::Thread(thread) => {
                        self.set_status(format!("Thread {}: {}", thread.thread_id, thread.reason));
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => return false,
                        };

                        if thread.reason == "exited" {
                            debugger.invalidate_thread_stack(thread.thread_id);
                            if debugger.thread_id == Some(thread.thread_id) {
                                debugger.resume_application();
                            }
                        }
                    }
                    Event::Breakpoint(events::BreakpointBody { reason, breakpoint }) => {
                        match &reason[..] {
                            "new" => {
                                if let Some(source) = breakpoint.source {
                                    let Some(path) = source.path else {
                                        warn!("DAP breakpoint event missing source path");
                                        return false;
                                    };
                                    let Some(line) = breakpoint.line else {
                                        warn!("DAP breakpoint event missing line");
                                        return false;
                                    };
                                    self.breakpoints.entry(path).or_default().push(Breakpoint {
                                        id: breakpoint.id,
                                        verified: breakpoint.verified,
                                        message: breakpoint.message.clone(),
                                        line: line.saturating_sub(1),
                                        column: breakpoint.column,
                                        ..Default::default()
                                    });
                                    if let Some(message) = &breakpoint.message {
                                        self.set_status(format!("Breakpoint: {}", message));
                                    }
                                }
                            }
                            "changed" => {
                                let Some(id) = breakpoint.id else {
                                    warn!("DAP breakpoint event missing id");
                                    return false;
                                };
                                if let Some(source) = &breakpoint.source {
                                    if source.path.is_none() {
                                        warn!("DAP breakpoint event missing source path");
                                    }
                                }
                                if breakpoint.line.is_none() {
                                    warn!("DAP breakpoint event missing line");
                                }
                                for breakpoints in self.breakpoints.values_mut() {
                                    if let Some(i) =
                                        breakpoints.iter().position(|b| b.id == Some(id))
                                    {
                                        breakpoints[i].verified = breakpoint.verified;
                                        breakpoints[i].message = breakpoint
                                            .message
                                            .clone()
                                            .or_else(|| breakpoints[i].message.take());
                                        breakpoints[i].line =
                                            breakpoint.line.map_or(breakpoints[i].line, |line| {
                                                line.saturating_sub(1)
                                            });
                                        breakpoints[i].column =
                                            breakpoint.column.or(breakpoints[i].column);
                                    }
                                }
                                if let Some(message) = &breakpoint.message {
                                    self.set_status(format!("Breakpoint: {}", message));
                                }
                            }
                            "removed" => {
                                let Some(id) = breakpoint.id else {
                                    warn!("DAP breakpoint event missing id");
                                    return false;
                                };
                                for breakpoints in self.breakpoints.values_mut() {
                                    if let Some(i) =
                                        breakpoints.iter().position(|b| b.id == Some(id))
                                    {
                                        breakpoints.remove(i);
                                    }
                                }
                            }
                            reason => {
                                warn!("Unknown breakpoint event: {}", reason);
                            }
                        }
                    }
                    Event::Output(events::OutputBody {
                        category, output, ..
                    }) => {
                        let prefix = match category {
                            Some(category) => {
                                if &category == "telemetry" {
                                    return false;
                                }
                                format!("Debug ({}):", category)
                            }
                            None => "Debug:".to_owned(),
                        };

                        log::info!("{}", output);
                        self.set_status(format!("{} {}", prefix, output));
                    }
                    Event::ProgressStart(body) => {
                        let status = {
                            let debugger = match self.debug_adapters.get_client_mut(id) {
                                Some(debugger) => debugger,
                                None => return false,
                            };

                            debugger.progress_start(body)
                        };

                        self.set_status(status);
                    }
                    Event::ProgressUpdate(body) => {
                        let status = {
                            let debugger = match self.debug_adapters.get_client_mut(id) {
                                Some(debugger) => debugger,
                                None => return false,
                            };

                            debugger.progress_update(body)
                        };

                        if let Some(status) = status {
                            self.set_status(status);
                        }
                    }
                    Event::ProgressEnd(body) => {
                        let status = {
                            let debugger = match self.debug_adapters.get_client_mut(id) {
                                Some(debugger) => debugger,
                                None => return false,
                            };

                            debugger.progress_end(body)
                        };

                        if let Some(status) = status {
                            self.set_status(status);
                        }
                    }
                    Event::Initialized(_) => {
                        self.set_status("Debugger initialized...");
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => return false,
                        };

                        // send existing breakpoints
                        for (path, breakpoints) in &mut self.breakpoints {
                            // TODO: call futures in parallel, await all
                            let _ = breakpoints_changed(debugger, path.clone(), breakpoints);
                        }
                        // TODO: fetch breakpoints (in case we're attaching)

                        if let Err(err) = debugger.configuration_done().await {
                            self.set_error(format!("Debugger configuration failed: {}", err));
                        } else {
                            self.set_status("Debugged application started");
                        }

                        self.debug_adapters.set_active_client(id);
                    }
                    Event::Terminated(terminated) => {
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => return false,
                        };

                        let restart_arg = if let Some(terminated) = terminated {
                            terminated.restart
                        } else {
                            None
                        };

                        let restart_bool = restart_arg
                            .as_ref()
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let disconnect_args = Some(DisconnectArguments {
                            restart: Some(restart_bool),
                            terminate_debuggee: None,
                            suspend_debuggee: None,
                        });

                        if let Err(err) = debugger.disconnect(disconnect_args).await {
                            self.set_error(format!(
                                "Cannot disconnect debugger upon terminated event receival {:?}",
                                err
                            ));
                            return false;
                        }

                        match restart_arg {
                            Some(Value::Bool(false)) | None => {
                                self.debug_adapters.remove_client(id);
                                self.debug_adapters.unset_active_client();
                                self.set_status(
                                    "Terminated debugging session and disconnected debugger.",
                                );

                                // Go through all breakpoints and set verfified to false
                                // this should update the UI to show the breakpoints are no longer connected
                                for breakpoints in self.breakpoints.values_mut() {
                                    for breakpoint in breakpoints.iter_mut() {
                                        breakpoint.verified = false;
                                    }
                                }
                            }
                            Some(val) => {
                                log::info!("Attempting to restart debug session.");
                                let connection_type = match debugger.connection_type() {
                                    Some(connection_type) => connection_type,
                                    None => {
                                        self.set_error("No starting request found, to be used in restarting the debugging session.");
                                        return false;
                                    }
                                };

                                let relaunch_resp = if let ConnectionType::Launch = connection_type
                                {
                                    debugger.launch(val).await
                                } else {
                                    debugger.attach(val).await
                                };

                                if let Err(err) = relaunch_resp {
                                    self.set_error(format!(
                                        "Failed to restart debugging session: {:?}",
                                        err
                                    ));
                                }
                            }
                        }
                    }
                    Event::Exited(resp) => {
                        if let Some(debugger) = self.debug_adapters.get_client_mut(id) {
                            debugger.resume_application();
                        }
                        let exit_code = resp.exit_code;
                        if exit_code != 0 {
                            self.set_error(format!(
                                "Debuggee failed to exit successfully (exit code: {exit_code})."
                            ));
                        }
                    }
                    ev => {
                        log::warn!("Unhandled event {:?}", ev);
                        return false; // return early to skip render
                    }
                }
            }
            Payload::Response(_) => unreachable!(),
            Payload::Request(request) => {
                let reply = match Request::parse(&request.command, request.arguments) {
                    Ok(Request::RunInTerminal(arguments)) => {
                        let reply = run_in_terminal(self.config().terminal.as_ref(), &arguments);
                        if let Err(err) = &reply {
                            self.set_error(err.to_string());
                        }
                        reply.map(|response| json!(response))
                    }
                    Ok(Request::StartDebugging(arguments)) => {
                        let debugger = match self.debug_adapters.get_client_mut(id) {
                            Some(debugger) => debugger,
                            None => {
                                self.set_error("No active debugger found.");
                                return true;
                            }
                        };
                        // Currently we only support starting a child debugger if the parent is using the TCP transport
                        let socket = match debugger.socket {
                            Some(socket) => socket,
                            None => {
                                self.set_error("Child debugger can only be started if the parent debugger is using TCP transport.");
                                return true;
                            }
                        };

                        let config = match debugger.config.clone() {
                            Some(config) => config,
                            None => {
                                error!("No configuration found for the debugger.");
                                return true;
                            }
                        };

                        let supports_run_in_terminal = self.config().terminal.is_some();
                        let result = self.debug_adapters.start_client(
                            Some(socket),
                            &config,
                            supports_run_in_terminal,
                        );

                        let client_id = match result {
                            Ok(child) => child,
                            Err(err) => {
                                self.set_error(format!(
                                    "Failed to create child debugger: {:?}",
                                    err
                                ));
                                return true;
                            }
                        };

                        let client = match self.debug_adapters.get_client_mut(client_id) {
                            Some(child) => child,
                            None => {
                                self.set_error("Failed to get child debugger.");
                                return true;
                            }
                        };

                        let relaunch_resp = if let ConnectionType::Launch = arguments.request {
                            client.launch(arguments.configuration).await
                        } else {
                            client.attach(arguments.configuration).await
                        };
                        if let Err(err) = relaunch_resp {
                            self.set_error(format!("Failed to start debugging session: {:?}", err));
                            return true;
                        }

                        Ok(json!({
                            "success": true,
                        }))
                    }
                    Err(err) => Err(err),
                };

                if let Some(debugger) = self.debug_adapters.get_client_mut(id) {
                    debugger
                        .reply(request.seq, &request.command, reply)
                        .await
                        .ok();
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_arguments() -> dap::requests::RunInTerminalArguments {
        dap::requests::RunInTerminalArguments {
            kind: Some("external".into()),
            title: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            args: vec!["debuggee".into()],
            env: None,
        }
    }

    #[test]
    fn missing_terminal_returns_an_error_for_the_adapter() {
        let result = run_in_terminal(None, &terminal_arguments());

        assert_eq!(
            result.unwrap_err().to_string(),
            "No external terminal defined"
        );
    }

    #[test]
    fn terminal_spawn_failure_returns_an_error_for_the_adapter() {
        let directory = tempfile::tempdir().unwrap();
        let terminal = TerminalConfig {
            command: directory
                .path()
                .join("missing-terminal")
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
        };
        let result = run_in_terminal(Some(&terminal), &terminal_arguments());

        assert!(result
            .unwrap_err()
            .to_string()
            .starts_with("Error starting external terminal:"));
    }
}
