use async_trait::async_trait;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::webhook_audit::matches_any_pattern;
use crate::hooks::traits::HookHandler;
use zeroclaw_api::hook::FileChangeEvent;
use zeroclaw_api::tool::ToolResult;
use zeroclaw_config::schema::CommandHookConfig;

struct Inner {
    config: CommandHookConfig,
    /// Bumped on every matching tool call; a pending timer only fires if it
    /// is still the latest generation when its quiet period elapses.
    generation: AtomicU64,
    /// Tool names seen since the last run (exported as `ZEROCLAW_HOOK_TOOLS`).
    pending: Mutex<BTreeSet<String>>,
    /// Serialises command runs so they never overlap.
    run_lock: tokio::sync::Mutex<()>,
    /// Completed runs (for tests / diagnostics).
    runs: AtomicU64,
}

/// Runs a local shell command once matching tool calls have been quiet for
/// `debounce_secs` (debounce / hysteresis). Fire-and-forget: never blocks
/// the agent loop.
pub struct CommandHook {
    inner: Arc<Inner>,
}

impl CommandHook {
    pub fn new(config: CommandHookConfig) -> Result<Self, String> {
        if config.command.trim().is_empty() {
            return Err("command is required when the command hook is enabled".to_string());
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                generation: AtomicU64::new(0),
                pending: Mutex::new(BTreeSet::new()),
                run_lock: tokio::sync::Mutex::new(()),
                runs: AtomicU64::new(0),
            }),
        })
    }

    #[cfg(test)]
    fn runs(&self) -> u64 {
        self.inner.runs.load(Ordering::SeqCst)
    }
}

impl Inner {
    fn take_pending(&self) -> String {
        let mut pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tools = pending.iter().cloned().collect::<Vec<_>>().join(",");
        pending.clear();
        tools
    }

    async fn run(&self, tools: String) {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&self.config.command)
            .env("ZEROCLAW_HOOK_TOOLS", &tools)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let outcome = tokio::time::timeout(timeout, cmd.output()).await;
        self.runs.fetch_add(1, Ordering::SeqCst);
        let (level_ok, message, attrs) = match outcome {
            Ok(Ok(out)) => {
                let tail = |b: &[u8]| {
                    let s = String::from_utf8_lossy(b);
                    let s = s.trim();
                    let start = s.len().saturating_sub(500);
                    let mut start = start;
                    while start < s.len() && !s.is_char_boundary(start) {
                        start += 1;
                    }
                    s[start..].to_string()
                };
                (
                    out.status.success(),
                    "command hook finished",
                    ::serde_json::json!({
                        "hook": "command-hook",
                        "tools": tools,
                        "status": out.status.to_string(),
                        "stdout": tail(&out.stdout),
                        "stderr": tail(&out.stderr),
                    }),
                )
            }
            Ok(Err(e)) => (
                false,
                "command hook failed to spawn",
                ::serde_json::json!({"hook": "command-hook", "tools": tools, "error": e.to_string()}),
            ),
            Err(_) => (
                false,
                "command hook timed out and was killed",
                ::serde_json::json!({"hook": "command-hook", "tools": tools, "timeout_secs": timeout.as_secs()}),
            ),
        };
        if level_ok {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(attrs),
                message
            );
        } else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(attrs),
                message
            );
        }
    }
}

#[async_trait]
impl HookHandler for CommandHook {
    fn name(&self) -> &str {
        "command-hook"
    }

    fn priority(&self) -> i32 {
        -110
    }

    async fn on_after_tool_call(&self, tool: &str, result: &ToolResult, _duration: Duration) {
        if !result.success || !matches_any_pattern(&self.inner.config.tool_patterns, tool) {
            return;
        }
        self.arm(tool.to_string());
    }

    async fn on_file_changed(&self, event: &FileChangeEvent) {
        if !matches_any_pattern(&self.inner.config.file_watcher_patterns, &event.watcher) {
            return;
        }
        self.arm(format!("watch:{}", event.watcher));
    }
}

impl CommandHook {
    /// Record `trigger` and (re)arm the quiet-period timer.
    fn arm(&self, trigger: String) {
        match self.inner.pending.lock() {
            Ok(mut p) => p.insert(trigger),
            Err(poisoned) => poisoned.into_inner().insert(trigger),
        };
        let my_gen = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let inner = Arc::clone(&self.inner);
        let quiet = Duration::from_secs(inner.config.debounce_secs);

        zeroclaw_spawn::spawn!(async move {
            tokio::time::sleep(quiet).await;
            if inner.generation.load(Ordering::SeqCst) != my_gen {
                return; // newer activity re-armed the timer
            }
            let _guard = inner.run_lock.lock().await;
            if inner.generation.load(Ordering::SeqCst) != my_gen {
                return; // activity happened while a previous run held the lock
            }
            let tools = inner.take_pending();
            inner.run(tools).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> ToolResult {
        ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        }
    }

    fn hook(debounce_secs: u64, command: &str) -> CommandHook {
        CommandHook::new(CommandHookConfig {
            enabled: true,
            command: command.to_string(),
            tool_patterns: vec!["file_*".to_string()],
            file_watcher_patterns: vec!["docs".to_string()],
            debounce_secs,
            timeout_secs: 5,
        })
        .expect("valid config")
    }

    #[test]
    fn rejects_empty_command() {
        assert!(CommandHook::new(CommandHookConfig::default()).is_err());
    }

    #[tokio::test]
    async fn burst_of_calls_runs_once_after_quiet_period() {
        let h = hook(1, "true");
        for _ in 0..5 {
            h.on_after_tool_call("file_write", &ok(), Duration::ZERO).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(h.runs(), 0, "must not run while activity continues");
        tokio::time::sleep(Duration::from_millis(2000)).await;
        assert_eq!(h.runs(), 1);
    }

    #[tokio::test]
    async fn non_matching_or_failed_calls_do_not_arm() {
        let h = hook(0, "true");
        h.on_after_tool_call("shell", &ok(), Duration::ZERO).await;
        let failed = ToolResult {
            success: false,
            output: String::new().into(),
            error: Some("x".into()),
        };
        h.on_after_tool_call("file_write", &failed, Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.runs(), 0);
    }

    #[tokio::test]
    async fn exports_tool_names() {
        let dir = std::env::temp_dir().join(format!("zc-cmdhook-{}", std::process::id()));
        let file = dir.with_extension("out");
        let h = hook(0, &format!("printf %s \"$ZEROCLAW_HOOK_TOOLS\" > {}", file.display()));
        h.on_after_tool_call("file_write", &ok(), Duration::ZERO).await;
        h.on_after_tool_call("file_edit", &ok(), Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let got = std::fs::read_to_string(&file).unwrap_or_default();
        let _ = std::fs::remove_file(&file);
        assert_eq!(got, "file_edit,file_write");
    }

    #[tokio::test]
    async fn file_events_arm_for_matching_watchers_only() {
        let file = std::env::temp_dir().join(format!("zc-cmdhook-fs-{}.out", std::process::id()));
        let h = hook(0, &format!("printf %s \"$ZEROCLAW_HOOK_TOOLS\" > {}", file.display()));
        let event = |watcher: &str| FileChangeEvent {
            watcher: watcher.to_string(),
            root: "/tmp".into(),
            path: "/tmp/a.md".into(),
            rel_path: "a.md".to_string(),
            kind: zeroclaw_api::hook::FileChangeKind::Changed,
        };
        h.on_file_changed(&event("other")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.runs(), 0, "non-matching watcher must not arm");
        h.on_file_changed(&event("docs")).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let got = std::fs::read_to_string(&file).unwrap_or_default();
        let _ = std::fs::remove_file(&file);
        assert_eq!(h.runs(), 1);
        assert_eq!(got, "watch:docs");
    }
}
