use crate::{Client, Payload, Result, StackFrame};
use futures_executor::block_on;
use futures_util::stream::SelectAll;
use helix_core::syntax::config::DebugAdapterConfig;
use slotmap::SlotMap;
use std::fmt;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// The resgistry is a struct that manages and owns multiple debugger clients
/// This holds the responsibility of managing the lifecycle of each client
/// plus showing the heirarcihical nature betweeen them
pub struct Registry {
    inner: SlotMap<DebugAdapterId, Client>,
    /// The active debugger client
    ///
    /// TODO: You can have multiple active debuggers, so the concept of a single active debugger
    /// may need to be changed
    current_client_id: Option<DebugAdapterId>,
    /// A stream of incoming messages from all debuggers
    pub incoming: SelectAll<UnboundedReceiverStream<(DebugAdapterId, Payload)>>,
}

impl Registry {
    /// Creates a new DebuggerService instance
    pub fn new() -> Self {
        Self {
            inner: SlotMap::with_key(),
            current_client_id: None,
            incoming: SelectAll::new(),
        }
    }

    pub fn start_client(
        &mut self,
        socket: Option<std::net::SocketAddr>,
        config: &DebugAdapterConfig,
        supports_run_in_terminal: bool,
    ) -> Result<DebugAdapterId> {
        self.inner.try_insert_with_key(|id| {
            let result = match socket {
                Some(socket) => block_on(Client::tcp(socket, id)),
                None => block_on(Client::process(
                    &config.transport,
                    &config.command,
                    config.args.iter().map(|arg| arg.as_str()).collect(),
                    config.port_arg.as_deref(),
                    id,
                )),
            };

            let (mut client, receiver) = result?;
            self.incoming.push(UnboundedReceiverStream::new(receiver));

            client.config = Some(config.clone());
            block_on(client.initialize(config.name.clone(), supports_run_in_terminal))?;
            client.quirks = config.quirks.clone();

            Ok(client)
        })
    }

    pub fn remove_client(&mut self, id: DebugAdapterId) {
        self.inner.remove(id);
    }

    pub fn get_client(&self, id: DebugAdapterId) -> Option<&Client> {
        self.inner.get(id)
    }

    pub fn get_client_mut(&mut self, id: DebugAdapterId) -> Option<&mut Client> {
        self.inner.get_mut(id)
    }

    pub fn get_active_client(&self) -> Option<&Client> {
        self.current_client_id.and_then(|id| self.get_client(id))
    }

    pub fn get_active_client_mut(&mut self) -> Option<&mut Client> {
        self.current_client_id
            .and_then(|id| self.get_client_mut(id))
    }

    pub fn set_active_client(&mut self, id: DebugAdapterId) {
        if self.current_client_id != Some(id) {
            if let Some(client) = self.get_active_client_mut() {
                client.invalidate_selection_requests();
            }
        }
        if self.get_client(id).is_some() {
            self.current_client_id = Some(id);
        } else {
            self.current_client_id = None;
        }
    }

    pub fn unset_active_client(&mut self) {
        if let Some(client) = self.get_active_client_mut() {
            client.invalidate_selection_requests();
        }
        self.current_client_id = None;
    }

    pub fn current_stack_frame(&self) -> Option<&StackFrame> {
        self.get_active_client()
            .and_then(|debugger| debugger.current_stack_frame())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

slotmap::new_key_type! {
    pub struct DebugAdapterId;
}

impl fmt::Display for DebugAdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn add_client(registry: &mut Registry) -> DebugAdapterId {
        registry.inner.insert_with_key(|id| {
            let (client, _incoming) = Client::streams(
                Box::new(BufReader::new(tokio::io::empty())),
                Box::new(tokio::io::sink()),
                None,
                id,
                None,
            )
            .unwrap();
            client
        })
    }

    #[tokio::test]
    async fn switching_adapters_away_and_back_invalidates_old_variables_selection() {
        let mut registry = Registry::new();
        let first = add_client(&mut registry);
        let second = add_client(&mut registry);
        registry.set_active_client(first);
        let selected = registry.get_active_client().unwrap().selection_guard();
        let stopped = registry.get_active_client().unwrap().stop_guard();
        registry.set_active_client(first);
        assert!(
            selected.is_current(),
            "reselecting the same adapter preserves valid work"
        );
        registry.set_active_client(second);
        registry.set_active_client(first);
        assert!(!selected.is_current());
        assert!(
            stopped.is_current(),
            "thread stack caches still belong to the same stop"
        );
        let selected = registry.get_active_client().unwrap().selection_guard();
        registry.unset_active_client();
        assert!(!selected.is_current());
    }
}
