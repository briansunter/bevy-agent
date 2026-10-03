//! Software and primary-window visual capture.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VisualCaptureOptions {
    pub output_dir: PathBuf,
    pub label: Option<String>,
    pub timeout_frames: u32,
    #[serde(default)]
    pub source: CaptureSource,
}

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    #[default]
    Auto,
    Software,
    PrimaryWindow,
}

impl Default for VisualCaptureOptions {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("screenshots"),
            label: None,
            timeout_frames: 8,
            source: CaptureSource::Auto,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct VisualCaptureResult {
    pub tick: u64,
    pub frame: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: String,
}

type VisualCaptureFn =
    dyn Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult> + Send + Sync;

#[derive(Resource)]
pub struct AgentVisualCaptureRenderer {
    capture: Box<VisualCaptureFn>,
}

impl AgentVisualCaptureRenderer {
    pub fn new<F>(capture: F) -> Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        Self {
            capture: Box::new(capture),
        }
    }

    pub fn capture(
        &self,
        world: &mut World,
        options: &VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        (self.capture)(world, options)
    }
}

pub trait VisualCaptureAppExt {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static;
}

impl VisualCaptureAppExt for App {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        self.insert_resource(AgentVisualCaptureRenderer::new(capture))
    }
}

#[must_use]
pub fn sanitized_capture_label(label: Option<&str>) -> String {
    let label = label.unwrap_or("capture");
    let mut sanitized = String::new();
    let mut previous_dash = false;
    for character in label.chars() {
        let next = if character.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(character.to_ascii_lowercase())
        } else if character == '-' || character == '_' || character.is_ascii_whitespace() {
            if previous_dash {
                None
            } else {
                previous_dash = true;
                Some('-')
            }
        } else {
            None
        };
        if let Some(next) = next {
            sanitized.push(next);
        }
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "capture".to_string()
    } else {
        sanitized.to_string()
    }
}

pub fn visual_capture_path(
    options: &VisualCaptureOptions,
    tick: u64,
    frame: u64,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&options.output_dir)?;
    let label = sanitized_capture_label(options.label.as_deref());
    let stem = format!("tick-{tick:06}-frame-{frame:06}-{label}");
    unique_path(&options.output_dir, &stem, "png")
}

fn unique_path(directory: &Path, stem: &str, extension: &str) -> Result<PathBuf> {
    let first = directory.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return Ok(first);
    }

    for index in 1..10_000 {
        let candidate = directory.join(format!("{stem}-{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "could not find an unused capture path for {}",
        directory.display()
    ))
}

#[cfg(feature = "visual")]
fn file_is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

impl AgentApp {
    pub fn capture_visual(&mut self, options: VisualCaptureOptions) -> Result<VisualCaptureResult> {
        self.ensure_started();
        self.ensure_reset()?;

        match options.source {
            CaptureSource::PrimaryWindow => self.capture_primary_window(options),
            CaptureSource::Software => {
                if !self
                    .app
                    .world()
                    .contains_resource::<AgentVisualCaptureRenderer>()
                {
                    return Err(anyhow!(
                        "software visual capture requested, but no AgentVisualCaptureRenderer is registered"
                    ));
                }
                self.app.world_mut().resource_scope(
                    |world, renderer: Mut<AgentVisualCaptureRenderer>| {
                        renderer.capture(world, &options)
                    },
                )
            }
            CaptureSource::Auto => {
                if self
                    .app
                    .world()
                    .contains_resource::<AgentVisualCaptureRenderer>()
                {
                    return self.app.world_mut().resource_scope(
                        |world, renderer: Mut<AgentVisualCaptureRenderer>| {
                            renderer.capture(world, &options)
                        },
                    );
                }
                self.capture_primary_window(options)
            }
        }
    }

    pub fn capture_primary_window(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        #[cfg(feature = "visual")]
        {
            self.capture_primary_window_impl(options)
        }

        #[cfg(not(feature = "visual"))]
        {
            let _ = options;
            Err(anyhow!(
                "visual capture requires a registered AgentVisualCaptureRenderer or the bevy_agent_runner visual feature"
            ))
        }
    }

    #[cfg(feature = "visual")]
    pub(super) fn capture_primary_window_impl(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        use bevy::render::view::screenshot::{Screenshot, save_to_disk};
        use bevy::window::{PrimaryWindow, Window};

        let (tick, frame, width, height) = {
            let world = self.app.world_mut();
            let tick = world.resource::<SimClock>().tick;
            let frame = world.resource::<AgentControlState>().frame;
            let mut windows = world.query_filtered::<&Window, With<PrimaryWindow>>();
            let window = windows
                .iter(world)
                .next()
                .ok_or_else(|| anyhow!("visual capture requires a primary window"))?;
            (
                tick,
                frame,
                window.physical_width(),
                window.physical_height(),
            )
        };
        let path = visual_capture_path(&options, tick, frame)?;
        let timeout_frames = options.timeout_frames.max(1);

        self.app
            .world_mut()
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));

        for _ in 0..=timeout_frames {
            self.app.update();
            if file_is_nonempty(&path) {
                return Ok(VisualCaptureResult {
                    tick,
                    frame,
                    path,
                    width,
                    height,
                    format: "png".to_string(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }

        Err(anyhow!(
            "visual capture timed out after {timeout_frames} frames: {}",
            path.display()
        ))
    }
}
