use crate::{
    operations::OPERATION_RESPONSE_BYTES,
    rpc::{JsonRpcBridge, JsonRpcError, JsonRpcResponse, serialize_response_with_limit},
    security::require_safe_bind,
    transport::{RemoteCommand, RemoteServerLimits, RemoteService, TransportHandle},
};
#[cfg(feature = "visual")]
use crate::{
    operations::OperationState,
    protocol::RpcCommand,
    rpc::{timeout_frames_to_duration, verify_visual_capture_file},
    security::RPC_INTERNAL_ERROR,
};
use anyhow::{Context, Result};
use bevy::prelude::*;
use bevy_agent_runner::AgentApp;
#[cfg(feature = "visual")]
use bevy_agent_runner::{
    AgentVisualCaptureRenderer, CaptureSource, ResetOptions, VisualCaptureResult,
    visual_capture_path,
};
use serde_json::Value;
#[cfg(feature = "visual")]
use std::time::Instant;
use std::{
    net::{SocketAddr, TcpListener},
    sync::{Arc, Mutex, mpsc},
};

#[derive(Resource)]
struct RemoteServerLifecycle {
    _transport: TransportHandle,
}
#[derive(Resource)]
struct MainThreadRemoteQueue {
    receiver: Mutex<mpsc::Receiver<RemoteCommand>>,
}
#[derive(Resource)]
struct MainThreadRemoteState {
    service: RemoteService,
    reset_once: bool,
    failure_reported: bool,
}
#[cfg(feature = "visual")]
#[derive(Resource, Default)]
struct PendingCaptures(Vec<(Entity, crate::operations::OperationHandle)>);

/// Concurrent bounded networking with all Bevy mutations executed in `Update`.
/// Health and authenticated operation-status requests run outside the simulation
/// pump, including while a callback runs or a primary-window capture is pending.
pub struct BevyRemoteControlPlugin {
    listener: Arc<TcpListener>,
    bridge: JsonRpcBridge,
    limits: RemoteServerLimits,
}
impl BevyRemoteControlPlugin {
    pub fn bind(bind_addr: impl Into<String>, bridge: JsonRpcBridge) -> Result<Self> {
        let bind_addr = bind_addr.into();
        let listener =
            TcpListener::bind(&bind_addr).with_context(|| format!("binding {bind_addr}"))?;
        require_safe_bind(listener.local_addr()?, &bridge.security)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: Arc::new(listener),
            bridge,
            limits: RemoteServerLimits::default(),
        })
    }
    pub fn with_limits(mut self, limits: RemoteServerLimits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}
impl Plugin for BevyRemoteControlPlugin {
    fn build(&self, app: &mut App) {
        let (service, receiver) =
            RemoteService::new(self.bridge.clone(), &self.limits).expect("validated remote limits");
        let transport =
            TransportHandle::start(self.listener.clone(), service.clone(), &self.limits)
                .expect("failed to start bounded remote workers");
        app.add_message::<AppExit>();
        app.insert_resource(service.status.clone());
        app.insert_resource(RemoteServerLifecycle {
            _transport: transport,
        })
        .insert_resource(MainThreadRemoteQueue {
            receiver: Mutex::new(receiver),
        })
        .insert_resource(MainThreadRemoteState {
            service,
            reset_once: false,
            failure_reported: false,
        })
        .add_systems(Update, pump_main_thread_remote);
    }
}
fn pump_main_thread_remote(world: &mut World) {
    if world
        .resource::<MainThreadRemoteState>()
        .service
        .status
        .failure()
        .is_some()
    {
        if !world.resource::<MainThreadRemoteState>().failure_reported {
            world
                .resource_mut::<MainThreadRemoteState>()
                .failure_reported = true;
            world.write_message(AppExit::error());
        }
        return;
    }
    #[cfg(feature = "visual")]
    if let Some(mut pending) = world.remove_resource::<PendingCaptures>() {
        pending.0.retain(|(entity, operation)| {
            if operation.state() == Some(OperationState::Running) {
                true
            } else {
                // An expired render request must release its observer and
                // captured response even if the GPU never sends a callback.
                let _ = world.despawn(*entity);
                false
            }
        });
        world.insert_resource(pending);
    }
    let requests = {
        let queue = world.resource::<MainThreadRemoteQueue>();
        let receiver = queue
            .receiver
            .lock()
            .expect("remote command queue poisoned");
        receiver.try_iter().take(64).collect::<Vec<_>>()
    };
    for request in requests {
        if !request.try_claim() {
            continue;
        }
        #[cfg(feature = "visual")]
        if try_schedule_primary_window_capture(world, &request) {
            continue;
        }
        let bridge = world
            .resource::<MainThreadRemoteState>()
            .service
            .bridge
            .clone();
        let id = request.prepared.id.clone();
        let response =
            with_running_agent(world, |env| bridge.handle_prepared(env, request.prepared));
        request.operation.complete(match response {
            Ok(response) => serialize_response_with_limit(&response, OPERATION_RESPONSE_BYTES),
            Err(error) => error_json_rpc_response(id, crate::RPC_INTERNAL_ERROR, error.to_string()),
        });
    }
    let tick = world
        .get_resource::<bevy_agent_core::SimClock>()
        .map_or(0, |clock| clock.tick);
    world
        .resource::<MainThreadRemoteState>()
        .service
        .update_tick(tick);
}
fn with_running_agent<R>(world: &mut World, run: impl FnOnce(&mut AgentApp) -> R) -> Result<R> {
    AgentApp::validate_world(world)?;
    let reset_once = world.resource::<MainThreadRemoteState>().reset_once;
    let owned_world = std::mem::replace(world, World::new());
    let mut app = App::empty();
    *app.world_mut() = owned_world;
    let mut env =
        AgentApp::from_running_app(app, reset_once).expect("unchanged world passed preflight");
    let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut env)));
    let reset_once = env.has_reset();
    let mut app = env.into_app();
    *world = std::mem::replace(app.world_mut(), World::new());
    world.resource_mut::<MainThreadRemoteState>().reset_once = reset_once;
    match response {
        Ok(response) => Ok(response),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(feature = "visual")]
fn try_schedule_primary_window_capture(world: &mut World, request: &RemoteCommand) -> bool {
    use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
    use bevy::window::{PrimaryWindow, Window};
    let RpcCommand::VisualCapture(params) = &request.prepared.command else {
        return false;
    };
    let source = params.source.unwrap_or_default();
    if source == CaptureSource::Software
        || (source == CaptureSource::Auto
            && world.contains_resource::<AgentVisualCaptureRenderer>())
    {
        return false;
    }
    let id = request.prepared.id.clone();
    let operation = request.operation.clone();
    let bridge = world
        .resource::<MainThreadRemoteState>()
        .service
        .bridge
        .clone();
    let options = match bridge.prepare_visual_capture(params) {
        Ok(options) => options,
        Err(error) => {
            operation.complete(error_json_rpc_response(id, error.code, error.message));
            return true;
        }
    };
    if !world.resource::<MainThreadRemoteState>().reset_once
        && let Err(error) = with_running_agent(world, |env| {
            let observation_mode = env
                .world()
                .resource::<bevy_agent_core::ObservationConfig>()
                .mode
                .clone();
            env.reset_with_response(ResetOptions {
                observation_mode,
                ..Default::default()
            })
        })
        .and_then(|result| result)
    {
        operation.complete(error_json_rpc_response(
            id,
            RPC_INTERNAL_ERROR,
            error.to_string(),
        ));
        return true;
    }
    if let Err(error) = AgentApp::validate_world(world) {
        operation.complete(error_json_rpc_response(
            id,
            RPC_INTERNAL_ERROR,
            error.to_string(),
        ));
        return true;
    }
    let (tick, frame, width, height) = {
        let tick = world.resource::<bevy_agent_core::SimClock>().tick;
        let frame = world.resource::<bevy_agent_core::AgentControlState>().frame;
        let mut windows = world.query_filtered::<&Window, With<PrimaryWindow>>();
        let Some(window) = windows.iter(world).next() else {
            operation.complete(error_json_rpc_response(
                id,
                RPC_INTERNAL_ERROR,
                "visual capture requires a primary window".into(),
            ));
            return true;
        };
        (
            tick,
            frame,
            window.physical_width(),
            window.physical_height(),
        )
    };
    let path = match visual_capture_path(&options, tick, frame) {
        Ok(path) => path,
        Err(error) => {
            operation.complete(error_json_rpc_response(
                id,
                RPC_INTERNAL_ERROR,
                error.to_string(),
            ));
            return true;
        }
    };
    // One listener maintenance loop expires all pending captures, without one
    // watchdog thread per request. The completion gate preserves one outcome.
    operation.expire_at(
        Instant::now() + timeout_frames_to_duration(options.timeout_frames),
        error_json_rpc_response(
            id.clone(),
            RPC_INTERNAL_ERROR,
            format!(
                "primary-window capture timed out after {} frames",
                options.timeout_frames
            ),
        ),
    );
    let mut save = save_to_disk(path.clone());
    let pending_operation = operation.clone();
    let entity = world
        .spawn(Screenshot::primary_window())
        .observe(move |captured: On<ScreenshotCaptured>| {
            if operation.state() != Some(OperationState::Running) {
                return;
            }
            save(captured);
            if let Err(error) = verify_visual_capture_file(&path) {
                operation.complete(error_json_rpc_response(
                    id.clone(),
                    RPC_INTERNAL_ERROR,
                    format!("capture failed verification: {error}"),
                ));
                return;
            }
            let result = VisualCaptureResult {
                tick,
                frame,
                path: path.clone(),
                width,
                height,
                format: "png".into(),
            };
            let response = JsonRpcResponse::Result {
                jsonrpc: "2.0",
                id: id.clone(),
                result: serde_json::to_value(result).expect("capture result is serializable"),
            };
            operation.complete(serialize_response_with_limit(
                &response,
                OPERATION_RESPONSE_BYTES,
            ));
        })
        .id();
    world.init_resource::<PendingCaptures>();
    world
        .resource_mut::<PendingCaptures>()
        .0
        .push((entity, pending_operation));
    true
}
fn error_json_rpc_response(id: Value, code: i32, message: String) -> String {
    serialize_response_with_limit(
        &JsonRpcResponse::Error {
            jsonrpc: "2.0",
            id,
            error: JsonRpcError {
                code,
                message,
                data: None,
            },
        },
        OPERATION_RESPONSE_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::{Duration, Instant},
    };
    #[test]
    fn fatal_service_emits_one_error_exit_and_keeps_the_cause_observable() {
        let plugin =
            BevyRemoteControlPlugin::bind("127.0.0.1:0", JsonRpcBridge::default()).unwrap();
        let mut app = App::new();
        app.add_plugins(plugin);
        app.world()
            .resource::<MainThreadRemoteState>()
            .service
            .fail("injected listener failure");
        let mut cursor = bevy::ecs::message::MessageCursor::<AppExit>::default();
        app.update();
        let messages = app
            .world()
            .resource::<bevy::ecs::message::Messages<AppExit>>();
        assert!(matches!(
            cursor.read(messages).next(),
            Some(AppExit::Error(_))
        ));
        app.update();
        let messages = app
            .world()
            .resource::<bevy::ecs::message::Messages<AppExit>>();
        assert_eq!(cursor.read(messages).count(), 0);
        assert_eq!(
            app.world()
                .resource::<crate::RemoteServerStatus>()
                .failure()
                .as_deref(),
            Some("injected listener failure")
        );
    }
    #[test]
    fn app_drop_closes_every_active_socket_and_releases_listener() {
        let plugin =
            BevyRemoteControlPlugin::bind("127.0.0.1:0", JsonRpcBridge::default()).unwrap();
        let address = plugin.local_addr().unwrap();
        let mut app = App::new();
        app.add_plugins(plugin);
        let mut clients = Vec::new();
        for _ in 0..4 {
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(b"GET /health HTTP/1.1\r\n").unwrap();
            clients.push(client);
        }
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        drop(app);
        assert!(started.elapsed() < Duration::from_secs(1));
        let _listener = TcpListener::bind(address).expect("App drop releases listener");
    }
    #[test]
    fn shared_health_routing_applies_cors_without_pumping() {
        let plugin = BevyRemoteControlPlugin::bind(
            "127.0.0.1:0",
            JsonRpcBridge::new(crate::RemoteSecurity {
                session_token: Some("secret".into()),
                allowed_origin: Some("https://allowed.test".into()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let address = plugin.local_addr().unwrap();
        let mut app = App::new();
        app.add_plugins(plugin);
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .write_all(b"GET /health HTTP/1.1\r\nOrigin: https://allowed.test\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.contains("Access-Control-Allow-Origin: https://allowed.test\r\n"));
        assert!(response.contains("Vary: Origin\r\n"));
    }
    #[test]
    fn invalid_world_is_rejected_before_loan_and_preserves_resources() {
        #[derive(Resource)]
        struct Marker;
        let mut world = World::new();
        world.insert_resource(Marker);
        let response = with_running_agent(&mut world, |_| ());
        assert!(response.is_err());
        assert!(world.contains_resource::<Marker>());
    }
    #[cfg(feature = "visual")]
    #[test]
    fn capture_initialization_preserves_configured_mode_permissions_and_original_id() {
        use bevy_agent_core::{
            AgentActionKind, AgentControlAppExt, Observation, ObservationMode, SymbolicObservation,
        };
        use bevy_agent_runner::{AgentControlPlugins, ResetOptions};
        let mut env = AgentApp::new(|| {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .add_plugins(AgentControlPlugins::default());
            app.set_environment_metadata("remote-capture-test", "1", None)
                .set_supported_actions([AgentActionKind::Noop])
                .set_supported_observation_modes([ObservationMode::PlayerKnowledge]);
            app.insert_resource(bevy_agent_core::ObservationConfig {
                mode: ObservationMode::PlayerKnowledge,
            });
            app.insert_observation_extractor(|world, _| {
                Observation::Symbolic(SymbolicObservation {
                    tick: world.resource::<bevy_agent_core::SimClock>().tick,
                    ..Default::default()
                })
            })
            .insert_checksum_extractor(|world| bevy_agent_core::default_checksum(world));
            app
        })
        .unwrap();
        env.reset_with_response(ResetOptions {
            observation_mode: ObservationMode::PlayerKnowledge,
            ..Default::default()
        })
        .unwrap();
        let mut app = env.into_app();
        let root =
            std::env::temp_dir().join(format!("bevy-primary-capture-{}", std::process::id()));
        let bridge = JsonRpcBridge::new(crate::RemoteSecurity {
            capabilities: crate::AgentCapability::VISUAL_CAPTURE
                | crate::AgentCapability::FILESYSTEM,
            artifact_root: Some(root.clone()),
            ..Default::default()
        })
        .unwrap();
        let (service, _) =
            RemoteService::new(bridge.clone(), &RemoteServerLimits::default()).unwrap();
        app.world_mut().insert_resource(MainThreadRemoteState {
            service: service.clone(),
            reset_once: false,
            failure_reported: false,
        });
        let prepared=bridge.prepare_request(r#"{"jsonrpc":"2.0","id":"capture-id","method":"agent.visual.capture","params":{"source":"primary_window"}}"#).unwrap();
        let operation = service.ledger.admit("agent.visual.capture").unwrap();
        assert!(operation.try_claim());
        let request = RemoteCommand {
            prepared,
            operation: operation.clone(),
            deadline: Instant::now() + std::time::Duration::from_secs(1),
        };
        assert!(try_schedule_primary_window_capture(
            app.world_mut(),
            &request
        ));
        let status = service.ledger.status(operation.id()).unwrap();
        let response = status.response.unwrap();
        assert_eq!(response["id"], "capture-id");
        assert_eq!(response["error"]["code"], crate::RPC_INTERNAL_ERROR);
        assert_eq!(
            app.world()
                .resource::<bevy_agent_core::ObservationConfig>()
                .mode,
            ObservationMode::PlayerKnowledge
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("primary window")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
