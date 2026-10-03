use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

fn main() -> anyhow::Result<()> {
    let mut env = AgentApp::new(sample_platformer::build_headless_app)?;
    let initial = env.reset(ResetOptions::default())?;
    println!("{}", serde_json::to_string_pretty(&initial)?);

    let mut actions = Vec::new();
    actions.extend((0..80).map(|_| AgentAction::Move { x: 1.0, y: 0.0 }));
    actions.push(AgentAction::Jump);
    actions.extend((0..120).map(|_| AgentAction::Move { x: 1.0, y: 0.0 }));

    for response in env.step_many(actions)? {
        if response.done || response.truncated {
            println!("{}", serde_json::to_string_pretty(&response)?);
            break;
        }
    }

    Ok(())
}
