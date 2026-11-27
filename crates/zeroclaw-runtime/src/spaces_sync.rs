//! The spaces committer (`[spaces_sync]`): the one thing that commits and
//! pushes the git repository holding the `[spaces.<id>]` folders.
//!
//! Every tick it commits agent edits inside a space once they have been
//! quiet for `space_quiet_secs` (so clients see them within seconds), the
//! rest of the repository after `quiet_secs`, and with a `remote`, runs a
//! fetch / merge / push round every `remote_interval_secs`, backing off on
//! failure. All git work goes through `zeroclaw-spaces`, under its lock.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use tokio_util::sync::CancellationToken;
use zeroclaw_config::schema::{Config, SpacesSyncConfig};
use zeroclaw_spaces::{Repo, Space};

const TICK: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// What the committer needs between ticks.
struct State {
    sync: SpacesSyncConfig,
    /// Space id → folder.
    spaces: BTreeMap<String, PathBuf>,
    space_pending: BTreeMap<String, Option<SystemTime>>,
    repo_pending: BTreeMap<PathBuf, Option<SystemTime>>,
    next_remote: Instant,
    remote_backoff: Duration,
}

/// Start the committer when `[spaces_sync] enabled` and at least one space
/// is declared. Config changes apply on the next daemon reload.
pub fn spawn(config: &Config, cancel: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
    if !config.spaces_sync.enabled {
        return None;
    }
    let spaces: BTreeMap<String, PathBuf> = config
        .spaces
        .keys()
        .filter_map(|id| Some((id.clone(), config.space_dir(id)?)))
        .collect();
    if spaces.is_empty() {
        return None;
    }
    let state = State {
        sync: config.spaces_sync.clone(),
        space_pending: spaces.keys().map(|id| (id.clone(), None)).collect(),
        spaces,
        repo_pending: BTreeMap::new(),
        next_remote: Instant::now(),
        remote_backoff: Duration::ZERO,
    };
    Some(zeroclaw_spawn::spawn!(run(state, cancel)))
}

async fn run(mut state: State, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => {}
        }
        state = match tokio::task::spawn_blocking(move || {
            round(&mut state);
            state
        })
        .await
        {
            Ok(s) => s,
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(json!({"error": e.to_string()})),
                    "spaces committer stopped"
                );
                return;
            }
        };
    }
    // Leave nothing uncommitted on an orderly stop.
    let _ = tokio::task::spawn_blocking(move || final_commit(&state)).await;
}

fn warn(msg: &str, attrs: serde_json::Value) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(attrs),
        msg
    );
}

fn note(msg: &str, attrs: serde_json::Value) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(attrs),
        msg
    );
}

fn round(state: &mut State) {
    let sync = state.sync.clone();
    let max_wait = Duration::from_secs(sync.max_wait_secs);
    let mut repos: BTreeMap<PathBuf, Repo> = BTreeMap::new();

    for (id, dir) in &state.spaces {
        let space = match Space::open(dir) {
            Ok(s) => s,
            Err(e) => {
                warn(
                    "spaces: cannot open space",
                    json!({"space": id, "error": e.to_string()}),
                );
                continue;
            }
        };
        let pending = state.space_pending.entry(id.clone()).or_default();
        if let Err(e) = space.commit_quiet(
            Duration::from_secs(sync.space_quiet_secs),
            pending,
            max_wait,
        ) {
            warn(
                "spaces: commit failed",
                json!({"space": id, "error": e.to_string()}),
            );
        }
        repos
            .entry(space.repo().top().to_path_buf())
            .or_insert_with(|| space.repo().clone());
    }

    let pathspec = zeroclaw_spaces::repo_pathspec(&sync.exclude);
    for (top, repo) in &repos {
        let pending = state.repo_pending.entry(top.clone()).or_default();
        if let Err(e) = repo.commit_quiet(
            &pathspec,
            Duration::from_secs(sync.quiet_secs),
            pending,
            max_wait,
        ) {
            warn(
                "spaces: commit failed",
                json!({"repo": top, "error": e.to_string()}),
            );
        }
    }

    if sync.remote.is_empty() || Instant::now() < state.next_remote {
        return;
    }
    let interval = Duration::from_secs(sync.remote_interval_secs.max(10));
    let mut failed = false;
    for (top, repo) in &repos {
        match repo.sync_remote(
            &sync.remote,
            &sync.branch,
            &sync.exclude,
            Duration::from_secs(sync.timeout_secs.max(5)),
        ) {
            Ok(out) if out.merged || out.pushed => note(
                "spaces: synced with the remote",
                json!({"repo": top, "merged": out.merged, "pushed": out.pushed,
                       "conflicts": out.conflicts.len()}),
            ),
            Ok(_) => {}
            Err(e) => {
                failed = true;
                warn(
                    "spaces: remote round failed",
                    json!({"repo": top, "error": e.to_string()}),
                );
            }
        }
    }
    state.remote_backoff = if failed {
        (state.remote_backoff * 2).clamp(interval, MAX_BACKOFF)
    } else {
        Duration::ZERO
    };
    state.next_remote = Instant::now() + interval.max(state.remote_backoff);
}

fn final_commit(state: &State) {
    let pathspec = zeroclaw_spaces::repo_pathspec(&state.sync.exclude);
    let mut done = std::collections::BTreeSet::new();
    for dir in state.spaces.values() {
        let Ok(space) = Space::open(dir) else {
            continue;
        };
        let top = space.repo().top().to_path_buf();
        if done.insert(top.clone())
            && let Err(e) = space.repo().snapshot(&pathspec, None)
        {
            warn(
                "spaces: final commit failed",
                json!({"repo": top, "error": e.to_string()}),
            );
        }
    }
}

/// Lifecycle hook that commits an agent's space edits as soon as its turn ends,
/// strictly scoped to spaces owned by `event.agent` (zero cross-space touch).
pub struct SpacesTurnHook {
    /// Agent alias → list of `(space_id, space_dir)` owned by that agent.
    by_agent: BTreeMap<String, Vec<(String, PathBuf)>>,
}

impl SpacesTurnHook {
    pub fn from_config(config: &Config) -> Self {
        let mut by_agent: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
        for (id, space) in &config.spaces {
            if !space.enabled || space.agent.is_empty() {
                continue;
            }
            if let Some(dir) = config.space_dir(id) {
                by_agent
                    .entry(space.agent.clone())
                    .or_default()
                    .push((id.clone(), dir));
            }
        }
        Self { by_agent }
    }

    pub fn new(by_agent: BTreeMap<String, Vec<(String, PathBuf)>>) -> Self {
        Self { by_agent }
    }

    pub fn is_empty(&self) -> bool {
        self.by_agent.is_empty()
    }
}

#[async_trait::async_trait]
impl zeroclaw_api::hook::HookHandler for SpacesTurnHook {
    fn name(&self) -> &str {
        "spaces-turn-commit"
    }

    async fn on_turn_end(&self, event: &zeroclaw_api::hook::TurnEndEvent) {
        let Some(owned) = self.by_agent.get(&event.agent).cloned() else {
            return;
        };
        let _ = tokio::task::spawn_blocking(move || {
            for (id, dir) in owned {
                let Ok(space) = Space::open(&dir) else {
                    continue;
                };
                if let Err(e) = space.snapshot(None) {
                    warn(
                        "spaces: post-turn commit failed",
                        json!({"space": id, "error": e.to_string()}),
                    );
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_round_commits_quiet_space_edits_and_leaves_fresh_ones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("procedures")).expect("mkdir");
        std::fs::write(ws.join("procedures/a.md"), "a\n").expect("write");
        git(&ws, &["init", "-q"]);
        git(&ws, &["add", "-A"]);
        git(&ws, &["commit", "-qm", "init"]);
        let mut state = State {
            sync: SpacesSyncConfig {
                enabled: true,
                space_quiet_secs: 0,
                quiet_secs: 3600,
                ..SpacesSyncConfig::default()
            },
            spaces: [("home".to_string(), ws.join("procedures"))].into(),
            space_pending: BTreeMap::new(),
            repo_pending: BTreeMap::new(),
            next_remote: Instant::now(),
            remote_backoff: Duration::ZERO,
        };
        std::fs::write(ws.join("procedures/a.md"), "a\nb\n").expect("write");
        std::fs::write(ws.join("notes.md"), "outside\n").expect("write");
        round(&mut state);
        let status = git(&ws, &["status", "--porcelain"]);
        assert_eq!(
            status, "?? notes.md",
            "the space edit is committed, the fresh outside edit waits"
        );
        assert_eq!(git(&ws, &["log", "-1", "--format=%an"]), "external");
        final_commit(&state);
        assert_eq!(
            git(&ws, &["status", "--porcelain"]),
            "",
            "an orderly stop commits the rest"
        );
    }

    #[tokio::test]
    async fn turn_end_hook_commits_only_owned_space_and_preserves_isolation() {
        use zeroclaw_api::hook::{HookHandler, TurnEndEvent};
        let tmp = tempfile::tempdir().expect("tempdir");
        let personal = tmp.path().join("personal");
        let family = tmp.path().join("family");
        for dir in [&personal, &family] {
            std::fs::create_dir_all(dir).expect("mkdir");
            std::fs::write(dir.join("README.md"), "# Init\n").expect("write");
            git(dir, &["init", "-q"]);
            git(dir, &["add", "-A"]);
            git(dir, &["commit", "-qm", "init"]);
        }
        std::fs::write(personal.join("README.md"), "# Personal updated\n").expect("write");
        std::fs::write(family.join("README.md"), "# Family dirty\n").expect("write");

        let hook = SpacesTurnHook::new(
            [
                ("luc".to_string(), vec![("personal".to_string(), personal.clone())]),
                ("family".to_string(), vec![("family".to_string(), family.clone())]),
            ]
            .into(),
        );
        hook.on_turn_end(&TurnEndEvent {
            agent: "luc".into(),
            session_id: "osom-topic-personal-car".into(),
            channel: "gateway".into(),
            response: "Updated.".into(),
        })
        .await;

        assert_eq!(git(&personal, &["status", "--porcelain"]), "", "luc's space committed on turn end");
        assert!(!git(&family, &["status", "--porcelain"]).is_empty(), "family's space stayed isolated and untouched");

        let mut cfg = Config::default();
        cfg.spaces.insert(
            "personal".into(),
            zeroclaw_config::schema::SpaceConfig {
                enabled: true,
                agent: "luc".into(),
                review_agent: "scribe-personal".into(),
                path: ".".into(),
                files_dir: "files/personal".into(),
                title: "Personal".into(),
            },
        );
        cfg.spaces.insert(
            "family".into(),
            zeroclaw_config::schema::SpaceConfig {
                enabled: true,
                agent: "family".into(),
                review_agent: "scribe-family".into(),
                path: ".".into(),
                files_dir: "files/family".into(),
                title: "Family".into(),
            },
        );
        assert!(cfg.space_isolation_allows("luc", "personal"));
        assert!(cfg.space_isolation_allows("scribe-personal", "personal"));
        assert!(!cfg.space_isolation_allows("luc", "family"));
        assert!(!cfg.space_isolation_allows("family", "personal"));
        assert!(!cfg.space_isolation_allows("scribe-family", "personal"));
        assert!(cfg.space_files_dir("personal").is_some());
    }
}
