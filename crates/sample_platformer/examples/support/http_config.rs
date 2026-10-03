//! Shared configuration for headless and visual HTTP examples.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use bevy_agent_remote::{AgentCapability, RemoteSecurity};

pub fn configuration() -> Result<(String, RemoteSecurity)> {
    let (bind_addr, artifact_root) = parse_args(std::env::args().skip(1))?;
    std::fs::create_dir_all(&artifact_root)?;
    let security = RemoteSecurity {
        session_token: std::env::var("AGENT_TOKEN").ok(),
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        artifact_root: Some(artifact_root),
        ..Default::default()
    };
    Ok((bind_addr, security))
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(String, PathBuf)> {
    let mut bind_addr = None;
    let mut artifact_root = PathBuf::from("./artifacts");
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let path = if arg == "--artifact-dir" {
            Some(
                args.next()
                    .ok_or_else(|| anyhow!("--artifact-dir requires a directory"))?,
            )
        } else {
            arg.strip_prefix("--artifact-dir=").map(str::to_owned)
        };
        if let Some(path) = path {
            if path.is_empty() || path.starts_with("--") {
                return Err(anyhow!("--artifact-dir requires a directory"));
            }
            artifact_root = PathBuf::from(path);
        } else if arg.starts_with('-') {
            return Err(anyhow!("unknown option {arg:?}"));
        } else if bind_addr.replace(arg).is_some() {
            return Err(anyhow!("only one bind address is accepted"));
        }
    }
    Ok((
        bind_addr.unwrap_or_else(|| "127.0.0.1:4000".to_owned()),
        artifact_root,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_can_precede_the_bind_address() {
        let (bind, root) =
            parse_args(["--artifact-dir", "images", "127.0.0.1:4444"].map(str::to_owned)).unwrap();
        assert_eq!(bind, "127.0.0.1:4444");
        assert_eq!(root, PathBuf::from("images"));
        assert_eq!(
            parse_args(["--artifact-dir=images".to_owned()]).unwrap().1,
            root
        );
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        for args in [
            vec!["--artifact-dir"],
            vec!["--artifact-dir="],
            vec!["--artifact-dir", "--unknown"],
            vec!["--unknown"],
            vec!["127.0.0.1:1", "127.0.0.1:2"],
        ] {
            assert!(parse_args(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}
