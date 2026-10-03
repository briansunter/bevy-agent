//! Plugin composition and snapshot policy configuration.

use super::*;

#[derive(Clone, Debug)]
pub struct AgentControlPlugins {
    control: AgentControlPlugin,
    snapshots: bool,
    replay: bool,
    snapshot_policy: Option<SnapshotPolicy>,
}

impl AgentControlPlugins {
    #[must_use]
    pub fn without_snapshots(mut self) -> Self {
        self.snapshots = false;
        self
    }

    #[must_use]
    pub fn without_replay(mut self) -> Self {
        self.replay = false;
        self
    }

    #[must_use]
    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = Some(policy);
        self
    }
}

impl Default for AgentControlPlugins {
    fn default() -> Self {
        Self {
            control: AgentControlPlugin::default(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }
}

impl PluginGroup for AgentControlPlugins {
    fn build(self) -> PluginGroupBuilder {
        let mut builder = PluginGroupBuilder::start::<Self>().add(self.control);
        if self.snapshots {
            builder = builder.add(AgentSnapshotPlugin);
            if let Some(policy) = self.snapshot_policy {
                builder = builder.add(AgentSnapshotPolicyPlugin(policy));
            }
        }
        if self.replay {
            builder = builder.add(AgentReplayPlugin);
        }
        builder
    }
}

struct AgentSnapshotPolicyPlugin(SnapshotPolicy);

impl Plugin for AgentSnapshotPolicyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.0.clone());
    }
}
