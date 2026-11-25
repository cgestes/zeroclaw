//! Git-backed document spaces that many offline-first clients edit at once.
//!
//! A *space* is a folder inside a git repository whose working tree agents
//! also edit directly. Guarantees:
//!
//! - **A client's edit is stored before it is merged.** [`Space::commit`]
//!   first records the client's exact files as a commit whose parent is the
//!   version the client started from, under `refs/spaces/changes/<id>`.
//! - **Merges never drop content** (see [`merge`]).
//! - **Working-tree writes never clobber an unseen edit** (see `worktree`).
//! - **Retries are idempotent**: a repeated `change_id` returns the first
//!   answer.
//! - **One writer at a time**: every mutation holds an in-process mutex and
//!   an `flock` on `<git dir>/zeroclaw-spaces.lock`, which scripts can share.
//!
//! The engine is synchronous and shells out to `git` (≥ 2.43).

mod git;
pub mod merge;
mod worktree;

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

pub use merge::{Conflict, ConflictKind};

use git::{Entry, Git, TempIndex};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git {args} failed ({code}): {stderr}")]
    Git {
        args: String,
        code: i32,
        stderr: String,
    },
    #[error("git {0} timed out")]
    Timeout(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid path '{0}'")]
    InvalidPath(String),
    #[error("invalid change id '{0}': use 8-64 characters from [A-Za-z0-9_-]")]
    InvalidChangeId(String),
    #[error("'{0}' not found")]
    NotFound(String),
    #[error("disk space low: {available} bytes free, {needed} needed")]
    DiskFull { available: u64, needed: u64 },
    #[error("files under '{0}' kept changing during the write; try again")]
    Busy(String),
    #[error("'{0}' is not inside a git repository with a commit")]
    NotARepo(PathBuf),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A git author.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Author {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// Edits nobody claimed: shells, SSH, other programs.
    pub fn external() -> Self {
        Self::new("external", "external@zeroclaw.invalid")
    }
}

/// One file of a space at a commit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub blob: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tree {
    pub head: String,
    pub files: Vec<FileEntry>,
}

/// A file's new blob since some commit; `None` when it was deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub blob: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Changes {
    pub head: String,
    /// True when `since` was unknown: `changes` then lists every file, and
    /// the client should drop files that are not in it.
    pub full: bool,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub blob: String,
    pub commit: String,
    pub bytes: Vec<u8>,
}

/// An edit to one file of a space; `content: None` deletes it.
#[derive(Clone, Debug)]
pub struct Edit {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct CommitRequest {
    /// Client-generated id; a retry with the same id gets the same answer.
    pub change_id: String,
    /// The commit the client's copy was at. `None` or an unknown commit:
    /// every edited file is merged as if both sides created it.
    pub base: Option<String>,
    pub author: Author,
    pub message: String,
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitResult {
    /// The client's own commit (its edits on top of `base`).
    pub commit: String,
    /// HEAD after the merge; the client syncs `changes?since=` its old head.
    pub head: String,
    /// True when others' changes were folded in.
    pub merged: bool,
    pub conflicts: Vec<Conflict>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub commit: String,
    pub author: String,
    pub time: String,
    pub message: String,
}

/// Published after every commit that moves HEAD.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    /// Repository top.
    pub repo: PathBuf,
    pub head: String,
    /// Repository-relative paths that changed.
    pub paths: Vec<String>,
    pub author: String,
}

static EVENTS: LazyLock<tokio::sync::broadcast::Sender<Event>> =
    LazyLock::new(|| tokio::sync::broadcast::channel(256).0);

/// Every commit any space makes in this process.
pub fn subscribe() -> tokio::sync::broadcast::Receiver<Event> {
    EVENTS.subscribe()
}

fn publish(event: Event) {
    // No subscribers is fine.
    let _ = EVENTS.send(event);
}

/// Who last wrote each working-tree file, keyed by absolute path, so the
/// committer can author agent edits correctly. Filled by file tools.
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, Author>>> = LazyLock::new(Default::default);

/// Record that `author` just wrote `path` (absolute). Tools call this after
/// a successful write; the next snapshot uses it as the commit author.
pub fn record_write(path: &Path, author: Author) {
    let key = normalize(path);
    WRITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, author);
}

fn normalize(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => dir
            .canonicalize()
            .map(|d| d.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

/// A git repository whose working tree is live.
#[derive(Clone, Debug)]
pub struct Repo {
    top: PathBuf,
    git_dir: PathBuf,
    git: Git,
    lock: Arc<Mutex<()>>,
}

struct Locked<'a> {
    _mutex: MutexGuard<'a, ()>,
    _file: std::fs::File,
}

/// What one remote round did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoteSync {
    pub merged: bool,
    pub pushed: bool,
    pub conflicts: Vec<Conflict>,
}

impl Repo {
    /// The repository containing `dir`. It must have at least one commit.
    pub fn open(dir: &Path) -> Result<Self> {
        let probe = Git::new(dir);
        let out = probe.output(
            &["rev-parse", "--show-toplevel", "--absolute-git-dir"],
            None,
            &[],
            None,
        )?;
        if out.code != 0 {
            return Err(Error::NotARepo(dir.to_path_buf()));
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut lines = text.lines();
        let (Some(top), Some(git_dir)) = (lines.next(), lines.next()) else {
            return Err(Error::NotARepo(dir.to_path_buf()));
        };
        let top = PathBuf::from(top).canonicalize()?;
        let git = Git::new(&top);
        if git.resolve("HEAD^{commit}")?.is_none() {
            return Err(Error::NotARepo(dir.to_path_buf()));
        }
        let lock = LOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(top.clone())
            .or_default()
            .clone();
        Ok(Self {
            top,
            git_dir: PathBuf::from(git_dir),
            git,
            lock,
        })
    }

    pub fn top(&self) -> &Path {
        &self.top
    }

    fn lock(&self) -> Result<Locked<'_>> {
        let mutex = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.git_dir.join("zeroclaw-spaces.lock"))?;
        flock(&file)?;
        Ok(Locked {
            _mutex: mutex,
            _file: file,
        })
    }

    fn head(&self) -> Result<String> {
        self.git
            .resolve("HEAD^{commit}")?
            .ok_or_else(|| Error::NotARepo(self.top.clone()))
    }

    /// Repository-relative files that differ from HEAD under `pathspec`,
    /// with the newest modification time among them.
    fn dirty(&self, pathspec: &[String]) -> Result<(Vec<String>, Option<SystemTime>)> {
        let mut args = vec![
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
            "--",
        ];
        args.extend(pathspec.iter().map(String::as_str));
        let out = self
            .git
            .output(&args, None, &[("GIT_OPTIONAL_LOCKS", "0")], None)?;
        if out.code != 0 {
            return Err(Error::Git {
                args: "status".into(),
                code: out.code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        let mut paths = Vec::new();
        let mut newest: Option<SystemTime> = None;
        for rec in git::records(&out.stdout) {
            let Some(path) = rec.get(3..) else { continue };
            if is_engine_temp(path) {
                continue;
            }
            if let Ok(m) = std::fs::symlink_metadata(self.top.join(path)).and_then(|m| m.modified())
            {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
            paths.push(path.to_string());
        }
        Ok((paths, newest))
    }

    /// Commit uncommitted working-tree changes under `pathspec`, one commit
    /// per author. Caller holds the lock.
    fn snapshot_locked(&self, pathspec: &[String], note: Option<&str>) -> Result<Vec<Event>> {
        let (paths, _) = self.dirty(pathspec)?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut by_author: BTreeMap<Author, Vec<String>> = BTreeMap::new();
        {
            let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
            for p in paths {
                let author = writers
                    .remove(&normalize(&self.top.join(&p)))
                    .unwrap_or_else(Author::external);
                by_author.entry(author).or_default().push(p);
            }
        }
        let zero = self.git.zero_id()?;
        let mut events = Vec::new();
        for (author, paths) in by_author {
            let head = self.head()?;
            let mut updates = Vec::new();
            for p in &paths {
                match worktree::read(&self.top.join(p))? {
                    Some(disk) => {
                        let id = self.git.write_blob(&disk.bytes)?;
                        updates.push((
                            p.clone(),
                            Some(Entry {
                                mode: disk.mode.into(),
                                id,
                            }),
                        ));
                    }
                    None => updates.push((p.clone(), None)),
                }
            }
            let index = TempIndex::new(&self.git_dir);
            let ig = self.git.with_index(index.path());
            ig.run(&["read-tree", &head])?;
            git::index_info(&ig, &zero, &updates)?;
            let tree = ig.line(&["write-tree"])?;
            if tree == self.git.tree_of(&head)? {
                self.sync_index(&paths)?;
                continue;
            }
            let message = note
                .map(str::to_string)
                .unwrap_or_else(|| summary(&author.name, &paths));
            let commit = self.git.commit_tree(&tree, &[&head], &message, &author)?;
            self.git
                .update_ref("HEAD", &commit, &head, "spaces: snapshot")?;
            self.sync_index(&paths)?;
            events.push(Event {
                repo: self.top.clone(),
                head: commit,
                paths,
                author: author.name.clone(),
            });
        }
        Ok(events)
    }

    /// Make the shared index match HEAD for `paths`, so `git status` stays
    /// truthful for people and scripts.
    fn sync_index(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(git::literal(p).as_bytes());
            input.push(0);
        }
        self.git
            .run_with(
                &[
                    "reset",
                    "-q",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                Some(&input),
                &[],
            )
            .map(|_| ())
    }

    /// Commit working-tree changes under `pathspec` once the newest of them
    /// is `quiet` old, or once they have been pending for `max_wait` (since
    /// `pending_since`, which the caller keeps between calls).
    pub fn commit_quiet(
        &self,
        pathspec: &[String],
        quiet: Duration,
        pending_since: &mut Option<SystemTime>,
        max_wait: Duration,
    ) -> Result<Vec<Event>> {
        let (paths, newest) = self.dirty(pathspec)?;
        if paths.is_empty() {
            *pending_since = None;
            return Ok(Vec::new());
        }
        let now = SystemTime::now();
        let since = *pending_since.get_or_insert(now);
        let calm = newest.is_none_or(|n| now.duration_since(n).unwrap_or_default() >= quiet);
        let overdue = now.duration_since(since).unwrap_or_default() >= max_wait;
        if !(calm || overdue) {
            return Ok(Vec::new());
        }
        let _g = self.lock()?;
        let events = self.snapshot_locked(pathspec, None)?;
        *pending_since = None;
        for e in &events {
            publish(e.clone());
        }
        Ok(events)
    }

    /// Write the difference between `from` and `to` into the working tree.
    /// `Ok(false)`: someone edited a file meanwhile; nothing of theirs was
    /// lost, and the caller should snapshot and redo the merge.
    fn apply(
        &self,
        from: &str,
        to: &str,
        keep: &dyn Fn(&str) -> bool,
    ) -> Result<(bool, Vec<String>)> {
        let mut changed = Vec::new();
        for d in git::diff(&self.git, from, to, &[])? {
            if !keep(&d.path) {
                continue;
            }
            let placed = match (&d.old, &d.new) {
                (old, Some(new)) => {
                    let bytes = self.git.blob(&new.id)?;
                    worktree::write(
                        &self.git,
                        &self.top,
                        &d.path,
                        &bytes,
                        &new.mode,
                        old.as_ref().map(|e| e.id.as_str()),
                    )?
                }
                (Some(old), None) => worktree::remove(&self.git, &self.top, &d.path, &old.id)?,
                (None, None) => continue,
            };
            if let worktree::Placed::Displaced = placed {
                return Ok((false, changed));
            }
            changed.push(d.path);
        }
        Ok((true, changed))
    }

    /// Bring `incoming` into HEAD: fast-forward or merge, write the working
    /// tree, move HEAD. Retries when the working tree moves underneath.
    /// Caller holds the lock and has just snapshotted `pathspec`.
    fn integrate(
        &self,
        incoming: &str,
        author: &Author,
        message: &str,
        pathspec: &[String],
        scope: &merge::Scope<'_>,
    ) -> Result<(String, bool, Vec<Conflict>, Vec<Event>)> {
        let mut events = Vec::new();
        for _ in 0..5 {
            let head = self.head()?;
            if self.git.is_ancestor(incoming, &head)? {
                return Ok((head, false, Vec::new(), events));
            }
            let (target, merged, conflicts) = if self.git.is_ancestor(&head, incoming)? {
                (incoming.to_string(), false, Vec::new())
            } else {
                let m = merge::merge(
                    &self.git,
                    &self.git_dir,
                    &head,
                    incoming,
                    (author, message),
                    scope,
                )?;
                (m.commit, true, m.conflicts)
            };
            let (ok, changed) = self.apply(&head, &target, scope.keep)?;
            if !ok {
                // Commit what appeared, then merge again on top of it.
                events.extend(self.snapshot_locked(pathspec, None)?);
                continue;
            }
            self.git
                .update_ref("HEAD", &target, &head, "spaces: integrate")?;
            self.sync_index(&changed)?;
            events.push(Event {
                repo: self.top.clone(),
                head: target.clone(),
                paths: changed,
                author: author.name.clone(),
            });
            return Ok((target, merged, conflicts, events));
        }
        Err(Error::Busy(self.top.display().to_string()))
    }

    /// One round with the remote: commit everything (except `exclude`),
    /// fetch, merge what others pushed, push. Never force-pushes.
    pub fn sync_remote(
        &self,
        remote: &str,
        branch: &str,
        exclude: &[String],
        timeout: Duration,
    ) -> Result<RemoteSync> {
        let mut pathspec = vec![":/".to_string()];
        pathspec.extend(exclude.iter().map(|e| format!(":(top,exclude,literal){e}")));
        let keep = |p: &str| {
            !exclude
                .iter()
                .any(|e| p == e || p.starts_with(&format!("{e}/")))
        };
        let scope = merge::Scope {
            keep: &keep,
            strip: "",
        };
        let tracking = format!("refs/spaces/remote/{branch}");
        let _g = self.lock()?;
        let mut events = self.snapshot_locked(&pathspec, None)?;
        let fetch = self.git.output(
            &[
                "fetch",
                "--no-tags",
                "--quiet",
                remote,
                &format!("+refs/heads/{branch}:{tracking}"),
            ],
            None,
            &[],
            Some(timeout),
        )?;
        if fetch.code != 0 {
            for e in events {
                publish(e);
            }
            return Err(Error::Git {
                args: format!("fetch {remote} {branch}"),
                code: fetch.code,
                stderr: String::from_utf8_lossy(&fetch.stderr).trim().to_string(),
            });
        }
        let mut out = RemoteSync::default();
        if let Some(theirs) = self.git.resolve(&tracking)? {
            let author = Author::new(format!("{remote}/{branch}"), "remote@zeroclaw.invalid");
            let message = format!("Merge {remote}/{branch}");
            let (_, merged, conflicts, more) =
                self.integrate(&theirs, &author, &message, &pathspec, &scope)?;
            events.extend(more);
            out.merged = merged;
            out.conflicts = conflicts;
        }
        let head = self.head()?;
        let ahead = match self.git.resolve(&tracking)? {
            Some(theirs) => theirs != head,
            None => true,
        };
        if ahead {
            let push = self.git.output(
                &[
                    "push",
                    "--quiet",
                    remote,
                    &format!("HEAD:refs/heads/{branch}"),
                ],
                None,
                &[],
                Some(timeout),
            )?;
            out.pushed = push.code == 0;
            if push.code == 0 {
                self.git
                    .run(&["update-ref", &tracking, &head])
                    .map(|_| ())?;
            } else {
                for e in events {
                    publish(e);
                }
                return Err(Error::Git {
                    args: format!("push {remote} {branch}"),
                    code: push.code,
                    stderr: String::from_utf8_lossy(&push.stderr).trim().to_string(),
                });
            }
        }
        for e in events {
            publish(e);
        }
        Ok(out)
    }

    /// Commit everything under `pathspec` now (e.g. before a shutdown), with
    /// an optional message such as an agent's commit note.
    pub fn snapshot(&self, pathspec: &[String], note: Option<&str>) -> Result<Vec<Event>> {
        let _g = self.lock()?;
        let events = self.snapshot_locked(pathspec, note)?;
        for e in &events {
            publish(e.clone());
        }
        Ok(events)
    }
}

/// A folder of a [`Repo`] that clients sync.
#[derive(Clone, Debug)]
pub struct Space {
    repo: Repo,
    /// Repository-relative folder, without a trailing slash; empty for the
    /// whole repository.
    prefix: String,
}

impl Space {
    /// The space rooted at `dir`, which must exist inside a git repository.
    pub fn open(dir: &Path) -> Result<Self> {
        let repo = Repo::open(dir)?;
        let dir = dir.canonicalize()?;
        let rel = dir
            .strip_prefix(&repo.top)
            .map_err(|_| Error::InvalidPath(dir.display().to_string()))?;
        let prefix = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self { repo, prefix })
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Repository-relative folder of the space.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The space-relative path of a repository path, if it is inside.
    pub fn relative<'a>(&self, repo_path: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            return Some(repo_path);
        }
        repo_path
            .strip_prefix(&self.prefix)
            .and_then(|r| r.strip_prefix('/'))
    }

    fn full(&self, rel: &str) -> Result<String> {
        let clean = validate(rel)?;
        Ok(if self.prefix.is_empty() {
            clean
        } else {
            format!("{}/{clean}", self.prefix)
        })
    }

    fn pathspec(&self) -> Vec<String> {
        vec![if self.prefix.is_empty() {
            ":/".to_string()
        } else {
            git::literal(&self.prefix)
        }]
    }

    fn strip(&self) -> String {
        if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        }
    }

    /// Commit agent edits in the space so reads see them.
    fn fresh(&self) -> Result<String> {
        let (dirty, _) = self.repo.dirty(&self.pathspec())?;
        if !dirty.is_empty() {
            let _g = self.repo.lock()?;
            for e in self.repo.snapshot_locked(&self.pathspec(), None)? {
                publish(e);
            }
        }
        self.repo.head()
    }

    /// Every file of the space at HEAD.
    pub fn tree(&self) -> Result<Tree> {
        let head = self.fresh()?;
        let scope: Vec<String> = if self.prefix.is_empty() {
            Vec::new()
        } else {
            vec![self.prefix.clone()]
        };
        let entries = git::ls_tree(&self.repo.git, &head, &scope)?;
        let sizes = self.sizes(&head)?;
        let files = entries
            .into_iter()
            .filter_map(|(path, e)| {
                let rel = self.relative(&path)?.to_string();
                let size = sizes.get(&path).copied().unwrap_or(0);
                Some(FileEntry {
                    path: rel,
                    blob: e.id,
                    size,
                })
            })
            .collect();
        Ok(Tree { head, files })
    }

    fn sizes(&self, commit: &str) -> Result<BTreeMap<String, u64>> {
        let mut args = vec!["ls-tree", "-r", "-z", "--long", "--full-tree", commit, "--"];
        let spec = self.pathspec();
        args.extend(spec.iter().map(String::as_str));
        let mut out = BTreeMap::new();
        for rec in git::records(&self.repo.git.run(&args)?) {
            let Some((meta, path)) = rec.split_once('\t') else {
                continue;
            };
            if let Some(size) = meta.split_whitespace().nth(3).and_then(|s| s.parse().ok()) {
                out.insert(path.to_string(), size);
            }
        }
        Ok(out)
    }

    /// A file at `at` (default HEAD).
    pub fn read(&self, rel: &str, at: Option<&str>) -> Result<File> {
        let full = self.full(rel)?;
        let commit = match at {
            Some(c) => self
                .repo
                .git
                .resolve(&format!("{c}^{{commit}}"))?
                .ok_or_else(|| Error::NotFound(c.to_string()))?,
            None => self.fresh()?,
        };
        let entries = git::ls_tree(&self.repo.git, &commit, std::slice::from_ref(&full))?;
        let entry = entries
            .get(&full)
            .ok_or_else(|| Error::NotFound(rel.to_string()))?;
        Ok(File {
            path: rel.to_string(),
            blob: entry.id.clone(),
            commit,
            bytes: self.repo.git.blob(&entry.id)?,
        })
    }

    /// What changed in the space between `since` and HEAD.
    pub fn changes(&self, since: Option<&str>) -> Result<Changes> {
        let head = self.fresh()?;
        let known = match since {
            Some(s) => self.repo.git.resolve(&format!("{s}^{{commit}}"))?,
            None => None,
        };
        let Some(since) = known else {
            let tree = self.tree()?;
            return Ok(Changes {
                head: tree.head,
                full: true,
                changes: tree
                    .files
                    .into_iter()
                    .map(|f| Change {
                        path: f.path,
                        blob: Some(f.blob),
                    })
                    .collect(),
            });
        };
        let changes = git::diff(&self.repo.git, &since, &head, &self.pathspec())?
            .into_iter()
            .filter_map(|d| {
                Some(Change {
                    path: self.relative(&d.path)?.to_string(),
                    blob: d.new.map(|e| e.id),
                })
            })
            .collect();
        Ok(Changes {
            head,
            full: false,
            changes,
        })
    }

    /// Commits that touched a file, newest first.
    pub fn history(&self, rel: &str, limit: usize) -> Result<Vec<LogEntry>> {
        let full = self.full(rel)?;
        let n = format!("-{}", limit.clamp(1, 500));
        let out = self.repo.git.run(&[
            "log",
            &n,
            "--format=%H%x1f%an%x1f%aI%x1f%s%x1e",
            "--",
            &git::literal(&full),
        ])?;
        Ok(String::from_utf8_lossy(&out)
            .split('\u{1e}')
            .filter_map(|rec| {
                let mut f = rec.trim().split('\u{1f}');
                Some(LogEntry {
                    commit: f.next()?.to_string(),
                    author: f.next()?.to_string(),
                    time: f.next()?.to_string(),
                    message: f.next()?.to_string(),
                })
            })
            .filter(|e| !e.commit.is_empty())
            .collect())
    }

    /// Store a client's edits, then merge them into HEAD (see crate docs).
    pub fn commit(&self, req: &CommitRequest) -> Result<CommitResult> {
        validate_change_id(&req.change_id)?;
        if req.edits.is_empty() {
            return Err(Error::Invalid("a commit needs at least one edit".into()));
        }
        let mut edits = Vec::with_capacity(req.edits.len());
        for e in &req.edits {
            edits.push((self.full(&e.path)?, e.content.as_deref()));
        }
        let git = &self.repo.git;
        let change_ref = format!("refs/spaces/changes/{}", req.change_id);
        let result_ref = format!("refs/spaces/results/{}", req.change_id);
        let _g = self.repo.lock()?;
        if let Some(stored) = git.resolve(&result_ref)? {
            let bytes = git.blob(&stored)?;
            return serde_json::from_slice(&bytes)
                .map_err(|e| Error::Invalid(format!("stored result: {e}")));
        }

        // 1. Durable intake: the client's files as a commit on its base.
        let client = match git.resolve(&change_ref)? {
            Some(c) => c,
            None => {
                let base = match &req.base {
                    Some(b) => git.resolve(&format!("{b}^{{commit}}"))?,
                    None => None,
                };
                let index = TempIndex::new(&self.repo.git_dir);
                let ig = git.with_index(index.path());
                let modes = match &base {
                    Some(b) => {
                        ig.run(&["read-tree", b])?;
                        let paths: Vec<String> = edits.iter().map(|(f, _)| f.clone()).collect();
                        git::ls_tree(git, b, &paths)?
                    }
                    None => {
                        ig.run(&["read-tree", "--empty"])?;
                        BTreeMap::new()
                    }
                };
                let mut updates = Vec::new();
                for (full, content) in &edits {
                    match content {
                        Some(bytes) => {
                            let id = git.write_blob(bytes)?;
                            // Keep an existing file's mode (executable, …).
                            let mode = modes
                                .get(full)
                                .map_or_else(|| "100644".to_string(), |e| e.mode.clone());
                            updates.push((full.clone(), Some(Entry { mode, id })));
                        }
                        None => updates.push((full.clone(), None)),
                    }
                }
                git::index_info(&ig, &git.zero_id()?, &updates)?;
                let tree = ig.line(&["write-tree"])?;
                if let Some(b) = &base
                    && tree == git.tree_of(b)?
                {
                    // Nothing changed relative to the client's base.
                    let result = CommitResult {
                        commit: b.clone(),
                        head: self.repo.head()?,
                        merged: false,
                        conflicts: Vec::new(),
                    };
                    let json =
                        serde_json::to_vec(&result).map_err(|e| Error::Invalid(e.to_string()))?;
                    let blob = git.write_blob(&json)?;
                    git.update_ref(&result_ref, &blob, &git.zero_id()?, "spaces: result")?;
                    return Ok(result);
                }
                let message = format!("{}\n\nChange-Id: {}\n", req.message.trim(), req.change_id);
                let parents: Vec<&str> = base.iter().map(String::as_str).collect();
                let c = git.commit_tree(&tree, &parents, &message, &req.author)?;
                git.update_ref(&change_ref, &c, &git.zero_id()?, "spaces: intake")?;
                c
            }
        };

        // 2. Commit agent edits first, then bring the client's commit in.
        let pathspec = self.pathspec();
        let mut events = self.repo.snapshot_locked(&pathspec, None)?;
        let strip = self.strip();
        let prefix = self.prefix.clone();
        let keep = move |p: &str| prefix.is_empty() || p.starts_with(&format!("{prefix}/"));
        let scope = merge::Scope {
            keep: &keep,
            strip: &strip,
        };
        let message = format!("Merge {}: {}", req.author.name, req.message.trim());
        let (head, merged, conflicts, more) =
            self.repo
                .integrate(&client, &req.author, &message, &pathspec, &scope)?;
        events.extend(more);
        let result = CommitResult {
            commit: client,
            head,
            merged,
            conflicts,
        };
        let json = serde_json::to_vec(&result).map_err(|e| Error::Invalid(e.to_string()))?;
        let blob = git.write_blob(&json)?;
        git.update_ref(&result_ref, &blob, &git.zero_id()?, "spaces: result")?;
        for e in events {
            publish(e);
        }
        Ok(result)
    }

    /// Commit the space's pending working-tree edits now, with `note` as the
    /// message (an agent's own description of its change).
    pub fn snapshot(&self, note: Option<&str>) -> Result<Vec<Event>> {
        self.repo.snapshot(&self.pathspec(), note)
    }
}

fn summary(author: &str, paths: &[String]) -> String {
    let names: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
    let more = if paths.len() > 3 {
        format!(" and {} more", paths.len() - 3)
    } else {
        String::new()
    };
    format!("{author}: edit {}{more}", names.join(", "))
}

fn is_engine_temp(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with('.')
        && name.contains(".zc-")
        && (name.ends_with(".tmp") || name.ends_with(".aside"))
}

/// A clean, relative, `/`-separated path that cannot leave the space or
/// touch git's own files.
fn validate(rel: &str) -> Result<String> {
    let bad = || Error::InvalidPath(rel.to_string());
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return Err(bad());
    }
    let mut parts = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => {
                let p = p.to_str().ok_or_else(bad)?;
                if p == ".git" {
                    return Err(bad());
                }
                parts.push(p);
            }
            Component::CurDir => {}
            _ => return Err(bad()),
        }
    }
    if parts.is_empty() {
        return Err(bad());
    }
    Ok(parts.join("/"))
}

fn validate_change_id(id: &str) -> Result<()> {
    let ok = (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidChangeId(id.to_string()))
    }
}

#[cfg(unix)]
fn flock(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: the descriptor belongs to `file`, which outlives the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err.into());
        }
    }
}

#[cfg(not(unix))]
fn flock(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_space() {
        assert_eq!(
            validate("car/README.md").ok().as_deref(),
            Some("car/README.md")
        );
        assert_eq!(validate("./car//a.md").ok().as_deref(), Some("car/a.md"));
        for bad in [
            "",
            "../x",
            "/etc/passwd",
            "a/../../b",
            ".git/config",
            "a/.git/x",
            "a\\b",
        ] {
            assert!(validate(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn change_ids_are_bounded() {
        assert!(validate_change_id("01JD-abc_9").is_ok());
        assert!(validate_change_id("short").is_err());
        assert!(validate_change_id("has space!").is_err());
    }

    #[test]
    fn engine_temp_files_are_not_content() {
        assert!(is_engine_temp("car/.README.md.zc-12-0.tmp"));
        assert!(!is_engine_temp("car/README.md"));
    }
}
