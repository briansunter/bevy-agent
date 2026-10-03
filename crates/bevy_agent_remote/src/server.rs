use crate::{
    operations::OPERATION_RESPONSE_BYTES,
    rpc::{JsonRpcBridge, serialize_response_with_limit},
    security::require_safe_bind,
    transport::{RemoteServerLimits, RemoteService, TransportHandle},
};
use anyhow::{Context, Result};
use bevy_agent_runner::AgentApp;
use std::{
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

/// Bounded concurrent networking with one serial simulation owner.
///
/// The calling thread owns the app and executes admitted commands. A fixed
/// worker pool serves health and operation-status requests even while a game
/// callback runs. Running callbacks cannot be preempted; callers can retrieve
/// their eventual outcome through `agent.operations.status` after a timeout.
#[derive(Clone, Debug)]
pub struct HttpRemoteServer {
    pub bind_addr: String,
    pub bridge: JsonRpcBridge,
    pub limits: RemoteServerLimits,
}
impl HttpRemoteServer {
    pub fn new(bind_addr: impl Into<String>, bridge: JsonRpcBridge) -> Self {
        Self {
            bind_addr: bind_addr.into(),
            bridge,
            limits: RemoteServerLimits::default(),
        }
    }
    pub fn with_limits(mut self, limits: RemoteServerLimits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn serve(&self, env: &mut AgentApp) -> Result<()> {
        self.serve_until(env, &AtomicBool::new(false))
    }

    /// Stop accepting work and join all network workers when `shutdown` is
    /// set. An already running simulation callback finishes before returning.
    pub fn serve_until(&self, env: &mut AgentApp, shutdown: &AtomicBool) -> Result<()> {
        let listener = TcpListener::bind(&self.bind_addr)
            .with_context(|| format!("binding {}", self.bind_addr))?;
        require_safe_bind(listener.local_addr()?, &self.bridge.security)?;
        let (service, receiver) = RemoteService::new(self.bridge.clone(), &self.limits)?;
        service.update_tick(env.current_tick());
        let _transport = TransportHandle::start(Arc::new(listener), service.clone(), &self.limits)?;
        self.serve_owner(env, shutdown, &service, receiver)
    }
    fn serve_owner(
        &self,
        env: &mut AgentApp,
        shutdown: &AtomicBool,
        service: &RemoteService,
        receiver: mpsc::Receiver<crate::transport::RemoteCommand>,
    ) -> Result<()> {
        while !service.stop.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
            let command = match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if !command.try_claim() {
                continue;
            }
            let response = self.bridge.handle_prepared(env, command.prepared);
            service.update_tick(env.current_tick());
            command.operation.complete(serialize_response_with_limit(
                &response,
                OPERATION_RESPONSE_BYTES,
            ));
        }
        service.status.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;
    use bevy_agent_core::{AgentControlAppExt, Observation, ObservationMode};
    use bevy_agent_runner::AgentControlPlugins;
    #[test]
    fn fatal_listener_cause_reaches_the_headless_owner_result() {
        let mut env = AgentApp::new(|| {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .add_plugins(AgentControlPlugins::default());
            app.set_environment_metadata("supervision-test", "1", None)
                .insert_observation_extractor(|world, _| {
                    Observation::default_for_tick(
                        world.resource::<bevy_agent_core::SimClock>().tick,
                    )
                })
                .insert_checksum_extractor(|world| bevy_agent_core::default_checksum(world))
                .set_supported_observation_modes([ObservationMode::Hybrid])
                .set_supported_actions([bevy_agent_core::AgentActionKind::Noop]);
            app
        })
        .unwrap();
        let server = HttpRemoteServer::new("127.0.0.1:0", JsonRpcBridge::default());
        let (service, receiver) =
            RemoteService::new(server.bridge.clone(), &server.limits).unwrap();
        service.fail("listener accept failed: test cause");
        let result = server.serve_owner(&mut env, &AtomicBool::new(false), &service, receiver);
        assert!(result.unwrap_err().to_string().contains("test cause"));
        assert!(!env.has_reset());
    }
}
