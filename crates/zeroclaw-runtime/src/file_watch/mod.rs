//! File watchers (`[file_watchers.<name>]`): daemon-level directory watching.
//!
//! Each watcher observes a directory tree, debounces raw notify events, and
//! for every changed file (deduplicated by content hash):
//! 1. fires the `on_file_changed` lifecycle hook (reusable by any hook,
//!    e.g. `hooks.builtin.command_hook.file_watcher_patterns`), then
//! 2. runs the watcher's built-in actions (canvas mirror today).
//!
//! An optional initial pass (`sync_on_start`) replays every existing file
//! through the actions — not the hooks — so in-memory targets such as the
//! canvas are rebuilt after a daemon restart.

pub mod canvas;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use glob::{MatchOptions, Pattern};
use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::hook::{FileChangeEvent, FileChangeKind};
use zeroclaw_config::schema::{FileWatcherCanvasConfig, FileWatcherConfig};

use crate::hooks::HookRunner;
use crate::tools::CanvasStore;

const GLOB_OPTS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

fn log_info(watcher: &str, msg: &str, attrs: serde_json::Value) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(serde_json::json!({"watcher": watcher, "detail": attrs})),
        msg
    );
}

fn log_warn(watcher: &str, msg: &str, attrs: serde_json::Value) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(serde_json::json!({"watcher": watcher, "detail": attrs})),
        msg
    );
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn now_local() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// Compiled include/exclude filter relative to the watched root.
struct PathFilter {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl PathFilter {
    fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        let compile = |pats: &[String]| -> Result<Vec<Pattern>> {
            pats.iter()
                .map(|p| Pattern::new(p).with_context(|| format!("invalid glob '{p}'")))
                .collect()
        };
        Ok(Self {
            include: compile(include)?,
            exclude: compile(exclude)?,
        })
    }

    /// `rel_path` is `/`-separated. Hidden components are always excluded.
    fn admits(&self, rel_path: &str) -> bool {
        if rel_path.is_empty() || rel_path.split('/').any(|c| c.starts_with('.')) {
            return false;
        }
        let included = self.include.is_empty()
            || self.include.iter().any(|p| p.matches_with(rel_path, GLOB_OPTS));
        included && !self.exclude.iter().any(|p| p.matches_with(rel_path, GLOB_OPTS))
    }
}

/// Canvas mirror state for one watcher.
struct CanvasMirror {
    cfg: FileWatcherCanvasConfig,
    store: CanvasStore,
    /// rel_path → (canvas id, title) of documents currently mirrored.
    docs: BTreeMap<String, (String, String)>,
    /// Last rendered index body (without timestamp) to avoid no-op frames.
    last_index_body: Option<String>,
}

impl CanvasMirror {
    fn push(&self, watcher: &str, id: &str, content: &str) {
        if content.len() > crate::tools::MAX_CONTENT_SIZE {
            log_warn(
                watcher,
                "file watcher: document too large for the canvas; skipped",
                serde_json::json!({"canvas_id": id, "bytes": content.len()}),
            );
            return;
        }
        if self.store.render(id, &self.cfg.content_type, content).is_none() {
            log_warn(
                watcher,
                "file watcher: canvas limit reached; document not mirrored",
                serde_json::json!({"canvas_id": id}),
            );
        }
    }

    fn apply(&mut self, watcher: &str, root_name: &str, rel_path: &str, content: Option<&str>) {
        let id = canvas::canvas_id(rel_path, &self.cfg.index_canvas_id);
        match content {
            Some(text) if !(self.cfg.skip_stubs && canvas::is_stub(text)) => {
                let rendered = canvas::render_document(
                    text,
                    rel_path,
                    &self.cfg.link_base_url,
                    &self.cfg.header_template,
                    root_name,
                    &now_local(),
                );
                self.push(watcher, &id, &rendered);
                self.docs
                    .insert(rel_path.to_string(), (id, canvas::title(text, rel_path)));
            }
            // Removed, or became a stub: clear it if we had mirrored it.
            _ => {
                if self.docs.remove(rel_path).is_some() {
                    self.store.clear(&id);
                }
            }
        }
    }

    fn refresh_index(&mut self, watcher: &str, root_name: &str) {
        if self.cfg.index_canvas_id.is_empty() {
            return;
        }
        let entries: Vec<canvas::IndexEntry<'_>> = self
            .docs
            .iter()
            .map(|(rel, (id, title))| canvas::IndexEntry {
                rel_path: rel,
                title,
                canvas_id: id,
            })
            .collect();
        let body = canvas::render_index_body(root_name, &self.cfg.link_base_url, &entries);
        if self.last_index_body.as_deref() == Some(body.as_str()) {
            return;
        }
        let doc = canvas::render_index(
            root_name,
            &self.cfg.link_base_url,
            &now_local(),
            &body,
            entries.len(),
        );
        let id = self.cfg.index_canvas_id.clone();
        self.push(watcher, &id, &doc);
        self.last_index_body = Some(body);
    }
}

/// One configured watcher and its per-file state.
pub struct FileWatcher {
    name: String,
    root: PathBuf,
    root_name: String,
    filter: PathFilter,
    debounce: Duration,
    sync_on_start: bool,
    /// rel_path → content hash of files last seen (dedups no-op writes).
    hashes: HashMap<String, u64>,
    mirror: Option<CanvasMirror>,
    hooks: Option<Arc<HookRunner>>,
}

impl FileWatcher {
    pub fn new(
        name: &str,
        cfg: &FileWatcherConfig,
        canvas_store: Option<CanvasStore>,
        hooks: Option<Arc<HookRunner>>,
    ) -> Result<Self> {
        if cfg.path.trim().is_empty() {
            anyhow::bail!("file_watchers.{name}.path is required");
        }
        let root = PathBuf::from(shellexpand::tilde(cfg.path.trim()).into_owned());
        if !root.is_absolute() {
            anyhow::bail!("file_watchers.{name}.path must be absolute");
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("file_watchers.{name}.path {}", root.display()))?;
        if !root.is_dir() {
            anyhow::bail!("file_watchers.{name}.path is not a directory");
        }
        let mirror = match (cfg.canvas.enabled, canvas_store) {
            (false, _) => None,
            (true, None) => {
                log_warn(
                    name,
                    "file watcher: canvas mirror enabled but no canvas store (gateway off?)",
                    serde_json::json!({}),
                );
                None
            }
            (true, Some(store)) => {
                if !crate::tools::ALLOWED_CONTENT_TYPES.contains(&cfg.canvas.content_type.as_str())
                {
                    anyhow::bail!(
                        "file_watchers.{name}.canvas.content_type '{}' is not one of {:?}",
                        cfg.canvas.content_type,
                        crate::tools::ALLOWED_CONTENT_TYPES
                    );
                }
                Some(CanvasMirror {
                    cfg: cfg.canvas.clone(),
                    store,
                    docs: BTreeMap::new(),
                    last_index_body: None,
                })
            }
        };
        let root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            name: name.to_string(),
            filter: PathFilter::new(&cfg.include, &cfg.exclude)?,
            debounce: Duration::from_millis(cfg.debounce_ms.max(50)),
            sync_on_start: cfg.sync_on_start,
            hashes: HashMap::new(),
            mirror,
            hooks,
            root,
            root_name,
        })
    }

    fn rel_path(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// Recursively list regular files under `dir` (no symlinked dirs, no
    /// hidden entries).
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                Self::walk(&entry.path(), out);
            } else if ft.is_file() {
                out.push(entry.path());
            }
        }
    }

    /// Process a batch of raw paths from notify (or the initial walk).
    /// Returns the effective changes, in path order.
    pub async fn process(&mut self, raw: Vec<PathBuf>, fire_hooks: bool) -> Vec<FileChangeEvent> {
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for path in raw {
            if path.is_dir() {
                let mut files = Vec::new();
                Self::walk(&path, &mut files);
                candidates.extend(files.iter().filter_map(|f| self.rel_path(f)));
            } else if let Some(rel) = self.rel_path(&path) {
                if !path.exists() {
                    // A removed directory only reports itself: expand to the
                    // tracked files that lived under it.
                    let prefix = format!("{rel}/");
                    candidates.extend(
                        self.hashes
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned(),
                    );
                }
                candidates.insert(rel);
            }
        }

        let mut events = Vec::new();
        for rel in candidates {
            if !self.filter.admits(&rel) {
                continue;
            }
            let full = self.root.join(&rel);
            let (kind, content) = if full.is_file() {
                let Ok(bytes) = tokio::fs::read(&full).await else {
                    continue;
                };
                let h = content_hash(&bytes);
                if self.hashes.insert(rel.clone(), h) == Some(h) {
                    continue; // touched/rewritten with identical content
                }
                (
                    FileChangeKind::Changed,
                    Some(String::from_utf8_lossy(&bytes).into_owned()),
                )
            } else if self.hashes.remove(&rel).is_some() {
                (FileChangeKind::Removed, None)
            } else {
                continue;
            };

            if let Some(mirror) = self.mirror.as_mut() {
                mirror.apply(&self.name, &self.root_name, &rel, content.as_deref());
            }
            events.push(FileChangeEvent {
                watcher: self.name.clone(),
                root: self.root.clone(),
                path: full,
                rel_path: rel,
                kind,
            });
        }

        if let Some(mirror) = self.mirror.as_mut() {
            mirror.refresh_index(&self.name, &self.root_name);
        }
        if fire_hooks && let Some(hooks) = &self.hooks {
            for event in &events {
                hooks.fire_file_changed(event).await;
            }
        }
        events
    }

    /// Initial pass over every existing file (actions only, no hooks).
    pub async fn sync_all(&mut self) -> usize {
        let root = self.root.clone();
        self.process(vec![root], false).await.len()
    }

    /// Watch until `cancel` fires. Errors only on setup failure.
    pub async fn run(mut self, cancel: CancellationToken) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && !matches!(event.kind, EventKind::Access(_))
            {
                for p in event.paths {
                    let _ = tx.send(p);
                }
            }
        })
        .context("creating filesystem watcher")?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", self.root.display()))?;

        if self.sync_on_start {
            let n = self.sync_all().await;
            log_info(
                &self.name,
                "file watcher: initial sync done",
                serde_json::json!({"root": self.root, "files": n}),
            );
        } else {
            // Seed hashes so the first real edit is detected as a change
            // and untouched files are not reported.
            let mirror = self.mirror.take();
            let root = self.root.clone();
            self.process(vec![root], false).await;
            self.mirror = mirror;
        }
        log_info(
            &self.name,
            "file watcher started",
            serde_json::json!({"root": self.root}),
        );

        loop {
            let first = tokio::select! {
                () = cancel.cancelled() => break,
                p = rx.recv() => match p { Some(p) => p, None => break },
            };
            let mut batch = vec![first];
            // Debounce: keep collecting until the tree is quiet.
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    () = tokio::time::sleep(self.debounce) => break,
                    p = rx.recv() => match p {
                        Some(p) => batch.push(p),
                        None => break,
                    },
                }
            }
            batch.sort();
            batch.dedup();
            let events = self.process(batch, true).await;
            if !events.is_empty() {
                log_info(
                    &self.name,
                    "file watcher: changes processed",
                    serde_json::json!({
                        "changes": events
                            .iter()
                            .map(|e| format!("{} {}", e.kind.as_str(), e.rel_path))
                            .collect::<Vec<_>>(),
                    }),
                );
            }
        }
        drop(watcher);
        Ok(())
    }
}

/// Spawn every enabled `[file_watchers.*]` entry. Invalid entries are logged
/// and skipped so one bad watcher cannot take the daemon down.
pub fn spawn_all(
    watchers: &HashMap<String, FileWatcherConfig>,
    canvas_store: Option<CanvasStore>,
    hooks: Option<Arc<HookRunner>>,
    cancel: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut names: Vec<&String> = watchers.keys().collect();
    names.sort();
    let mut handles = Vec::new();
    for name in names {
        let cfg = &watchers[name];
        if !cfg.enabled {
            continue;
        }
        match FileWatcher::new(name, cfg, canvas_store.clone(), hooks.clone()) {
            Ok(w) => {
                let cancel = cancel.clone();
                let name = name.clone();
                handles.push(zeroclaw_spawn::spawn!(async move {
                    if let Err(e) = w.run(cancel).await {
                        log_warn(
                            &name,
                            "file watcher stopped with an error",
                            serde_json::json!({"error": format!("{e:#}")}),
                        );
                    }
                }));
            }
            Err(e) => log_warn(
                name,
                "file watcher configuration is invalid; watcher disabled",
                serde_json::json!({"error": format!("{e:#}")}),
            ),
        }
    }
    handles
}

#[cfg(test)]
mod tests;
