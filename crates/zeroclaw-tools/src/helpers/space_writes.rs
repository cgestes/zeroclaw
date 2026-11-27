//! Tell the spaces committer which agent wrote a workspace file, so the
//! commit that later records the edit is authored by that agent instead of
//! `external`.

use std::path::Path;

use zeroclaw_config::policy::SecurityPolicy;

/// Record that the agent owning `security` wrote `path`.
///
/// The agent is read from its workspace directory, which ZeroClaw lays out
/// as `<install>/agents/<alias>/workspace`; a custom workspace path gives
/// the folder name instead.
pub(crate) fn record(security: &SecurityPolicy, path: &Path) {
    let alias = agent_alias(&security.workspace_dir);
    zeroclaw_spaces::record_write(
        path,
        zeroclaw_spaces::Author::new(alias.clone(), format!("{alias}@agents.zeroclaw.invalid")),
    );
}

fn agent_alias(workspace: &Path) -> String {
    let parent = workspace.parent();
    let under_agents = parent
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|n| n == "agents");
    let name = if under_agents {
        parent.and_then(Path::file_name)
    } else {
        workspace.file_name()
    };
    name.map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "agent".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_comes_from_the_workspace_layout() {
        assert_eq!(
            agent_alias(Path::new("/home/u/.zeroclaw/agents/luc/workspace")),
            "luc"
        );
        assert_eq!(agent_alias(Path::new("/srv/notes")), "notes");
        assert_eq!(agent_alias(Path::new("/")), "agent");
    }
}
