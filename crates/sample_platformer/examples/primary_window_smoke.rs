//! Bevy primary-window render/capture smoke test. Run under a display.
//!
//! Linux CI: Xvfb + Mesa lavapipe, WGPU_BACKEND=vulkan and VK_ICD_FILENAMES
//! pointing to lvp_icd.json. This exercises the renderer on a software Vulkan
//! adapter; it does not establish physical-GPU compatibility. The report
//! includes the selected adapter. Optional argument: directory for retained PNGs.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::renderer::RenderAdapterInfo;
use bevy::window::PrimaryWindow;
use bevy_agent_remote::{AgentCapability, BevyRemoteControlPlugin, JsonRpcBridge, RemoteSecurity};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
struct RendererInfo {
    name: String,
    backend: String,
    device_type: String,
    driver: String,
    driver_info: String,
}

#[derive(Debug, Serialize)]
struct SmokeReport {
    tick: u64,
    frame: u64,
    width: u32,
    height: u32,
    path: PathBuf,
    distinct_colors: usize,
    listener_closed: bool,
    renderer: RendererInfo,
}

type SharedResult = Arc<Mutex<Option<Result<SmokeReport>>>>;

#[derive(Resource)]
struct SmokeState {
    address: SocketAddr,
    result: SharedResult,
    frames: u32,
    deadline: Instant,
    started: bool,
    exiting: bool,
}

fn main() -> Result<()> {
    let artifacts = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "artifacts/primary-window-smoke".to_owned()),
    );
    std::fs::create_dir_all(&artifacts)?;
    let artifacts = artifacts.canonicalize()?;
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        artifact_root: Some(artifacts),
        ..Default::default()
    })?;
    let remote = BevyRemoteControlPlugin::bind("127.0.0.1:0", bridge)?;
    let address = remote.local_addr()?;
    let result: SharedResult = Arc::new(Mutex::new(None));
    let mut app = sample_platformer::build_visual_app();
    app.insert_resource(bevy::winit::WinitSettings::continuous())
        .insert_resource(SmokeState {
            address,
            result: Arc::clone(&result),
            frames: 0,
            deadline: Instant::now() + Duration::from_secs(120),
            started: false,
            exiting: false,
        })
        .add_plugins(remote)
        .add_systems(Update, run_smoke);
    app.run();
    drop(app);

    let mut report = result
        .lock()
        .map_err(|_| anyhow!("smoke result lock poisoned"))?
        .take()
        .ok_or_else(|| anyhow!("window exited before smoke completed"))??;
    ensure!(
        TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_err(),
        "remote listener remains alive after the Bevy app was dropped",
    );
    report.listener_closed = true;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_smoke(
    mut state: ResMut<SmokeState>,
    window: Query<&Window, With<PrimaryWindow>>,
    adapter: Res<RenderAdapterInfo>,
    mut exit: MessageWriter<AppExit>,
) {
    if state.exiting {
        return;
    }
    state.frames += 1;
    if !state.started && state.frames >= 30 {
        let Ok(window) = window.single() else {
            return;
        };
        let dimensions = (window.physical_width(), window.physical_height());
        if dimensions.0 == 0 || dimensions.1 == 0 {
            return;
        }
        state.started = true;
        let address = state.address;
        let shared = Arc::clone(&state.result);
        let renderer = RendererInfo {
            name: adapter.name.clone(),
            backend: format!("{:?}", adapter.backend),
            device_type: format!("{:?}", adapter.device_type),
            driver: adapter.driver.clone(),
            driver_info: adapter.driver_info.clone(),
        };
        std::thread::spawn(move || {
            let outcome = capture_through_remote_callback(address, dimensions, renderer);
            if let Ok(mut result) = shared.lock() {
                *result = Some(outcome);
            }
        });
    }
    if Instant::now() >= state.deadline
        && let Ok(mut result) = state.result.lock()
        && result.is_none()
    {
        *result = Some(Err(anyhow!(
            "graphical smoke exceeded its 120 second deadline"
        )));
    }
    let finished = state
        .result
        .lock()
        .map(|result| result.is_some())
        .unwrap_or(true);
    if finished {
        state.exiting = true;
        exit.write(AppExit::Success);
    }
}

fn capture_through_remote_callback(
    address: SocketAddr,
    window: (u32, u32),
    renderer: RendererInfo,
) -> Result<SmokeReport> {
    let reset = rpc(
        address,
        json!(1),
        "agent.reset",
        json!({
            "options": {"seed": 42, "observation_mode": "Hybrid", "create_initial_snapshot": true}
        }),
    )?;
    ensure!(reset["tick"] == 0, "reset did not establish tick zero");
    let step = rpc(
        address,
        json!(2),
        "agent.step",
        json!({"action": {"type": "Noop"}}),
    )?;
    ensure!(step["tick"] == 1, "step did not establish tick one");
    // A string correlation ID must survive scheduling and the render callback.
    let capture = rpc(
        address,
        json!("primary-window-callback"),
        "agent.visual.capture",
        json!({
            "source": "primary_window", "output_dir": "rendered", "label": "primary_window",
            "timeout_frames": 300,
        }),
    )?;
    let tick = capture["tick"].as_u64().context("capture tick missing")?;
    let frame = capture["frame"].as_u64().context("capture frame missing")?;
    let width = u32::try_from(capture["width"].as_u64().context("capture width missing")?)?;
    let height = u32::try_from(
        capture["height"]
            .as_u64()
            .context("capture height missing")?,
    )?;
    ensure!(
        tick == 1,
        "capture changed or misreported the controlled tick"
    );
    ensure!(capture["format"] == "png", "capture format is not PNG");
    ensure!(
        step["info"]["frame"] == frame,
        "capture changed or misreported the controlled frame"
    );
    ensure!(
        (width, height) == window,
        "capture metadata differs from actual primary window dimensions"
    );
    let path = PathBuf::from(capture["path"].as_str().context("capture path missing")?);
    let image = image::open(&path)
        .context("callback did not produce a decodable PNG")?
        .into_rgba8();
    ensure!(
        image.dimensions() == window,
        "PNG dimensions differ from the primary window"
    );
    let colors: BTreeSet<_> = image
        .pixels()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    ensure!(
        colors.len() >= 3,
        "primary-window PNG contains no rendered platformer geometry"
    );
    let observed = rpc(
        address,
        json!(4),
        "agent.observe",
        json!({"observation_mode": "Hybrid"}),
    )?;
    ensure!(
        observed["symbolic"]["tick"] == 1,
        "render capture advanced the simulation"
    );
    Ok(SmokeReport {
        tick,
        frame,
        width,
        height,
        path,
        distinct_colors: colors.len(),
        listener_closed: false,
        renderer,
    })
}

fn rpc(address: SocketAddr, id: Value, method: &str, params: Value) -> Result<Value> {
    let request = serde_json::to_vec(
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    )?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /rpc HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        request.len(),
    )?;
    stream.write_all(&request)?;
    let mut bytes = Vec::new();
    stream.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "smoke response exceeded its bound"
    );
    let boundary = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("response did not contain an HTTP header boundary")?;
    let header = std::str::from_utf8(&bytes[..boundary])?;
    ensure!(
        header.starts_with("HTTP/1.1 200 "),
        "RPC HTTP error: {header}"
    );
    let response: Value = serde_json::from_slice(&bytes[boundary + 4..])?;
    ensure!(
        response["jsonrpc"] == "2.0" && response["id"] == id,
        "response correlation failed: {response}"
    );
    ensure!(
        response.get("error").is_none(),
        "{method} failed: {response}"
    );
    response
        .get("result")
        .cloned()
        .context("response result missing")
}
