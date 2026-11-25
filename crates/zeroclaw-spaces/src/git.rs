//! A thin runner over the `git` CLI.
//!
//! Every call runs at the repository top with prompts, hooks, signing and
//! path quoting turned off, and names its index file explicitly, so the
//! engine never depends on the user's interactive git setup.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Error;

pub(crate) const COMMITTER_NAME: &str = "ZeroClaw spaces";
pub(crate) const COMMITTER_EMAIL: &str = "spaces@zeroclaw.invalid";

#[derive(Clone, Debug)]
pub(crate) struct Git {
    top: PathBuf,
    index: Option<PathBuf>,
}

pub(crate) struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Git {
    pub(crate) fn new(top: &Path) -> Self {
        Self {
            top: top.to_path_buf(),
            index: None,
        }
    }

    /// The same repository, reading and writing `index` instead of the
    /// shared index, so trees can be built without touching the working tree.
    pub(crate) fn with_index(&self, index: &Path) -> Self {
        Self {
            top: self.top.clone(),
            index: Some(index.to_path_buf()),
        }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(&self.top)
            .args([
                "-c",
                "core.quotepath=off",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_COMMITTER_NAME", COMMITTER_NAME)
            .env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_PREFIX");
        if let Some(index) = &self.index {
            c.env("GIT_INDEX_FILE", index);
        }
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    /// Run and return the exit code with both streams. Only spawn and I/O
    /// failures are errors; the caller decides what a non-zero code means.
    pub(crate) fn output(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        env: &[(&str, &str)],
        timeout: Option<Duration>,
    ) -> Result<Output, Error> {
        let mut child = self
            .command(args, env)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Feed stdin and drain both pipes on their own threads so a large
        // input or output can never deadlock against the child.
        let writer = match (child.stdin.take(), input) {
            (Some(mut stdin), Some(bytes)) => {
                let bytes = bytes.to_vec();
                Some(std::thread::spawn(move || stdin.write_all(&bytes)))
            }
            _ => None,
        };
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    p.read_to_end(&mut buf)?;
                }
                Ok::<_, std::io::Error>(buf)
            })
        };
        let out = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let err = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let status = match timeout {
            None => child.wait()?,
            Some(limit) => {
                let started = Instant::now();
                loop {
                    if let Some(status) = child.try_wait()? {
                        break status;
                    }
                    if started.elapsed() >= limit {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::Timeout(args.join(" ")));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        if let Some(w) = writer {
            // A child that exits without reading all of stdin closes the
            // pipe; that is its business, the exit code still decides.
            let _ = w.join();
        }
        let stdout = out
            .join()
            .map_err(|_| Error::Invalid("stdout reader panicked".into()))??;
        let stderr = err
            .join()
            .map_err(|_| Error::Invalid("stderr reader panicked".into()))??;
        Ok(Output {
            code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    /// Run, requiring success, and return stdout.
    pub(crate) fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        self.run_with(args, None, &[])
    }

    pub(crate) fn run_with(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        env: &[(&str, &str)],
    ) -> Result<Vec<u8>, Error> {
        let out = self.output(args, input, env, None)?;
        if out.code != 0 {
            return Err(Error::Git {
                args: args.join(" "),
                code: out.code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(out.stdout)
    }

    /// Run, requiring success, and return trimmed stdout as text.
    pub(crate) fn line(&self, args: &[&str]) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.run(args)?).trim().to_string())
    }

    /// `rev-parse --verify --quiet`: `None` when the name does not resolve.
    pub(crate) fn resolve(&self, name: &str) -> Result<Option<String>, Error> {
        let out = self.output(&["rev-parse", "--verify", "--quiet", name], None, &[], None)?;
        Ok((out.code == 0).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    pub(crate) fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool, Error> {
        let out = self.output(
            &["merge-base", "--is-ancestor", ancestor, of],
            None,
            &[],
            None,
        )?;
        match out.code {
            0 => Ok(true),
            1 => Ok(false),
            code => Err(Error::Git {
                args: format!("merge-base --is-ancestor {ancestor} {of}"),
                code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }),
        }
    }

    /// Store `bytes` as a blob and return its id.
    pub(crate) fn write_blob(&self, bytes: &[u8]) -> Result<String, Error> {
        let out = self.run_with(
            &["hash-object", "-w", "--no-filters", "--stdin"],
            Some(bytes),
            &[],
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// The blob id `bytes` would have, without storing it.
    pub(crate) fn hash_blob(&self, bytes: &[u8]) -> Result<String, Error> {
        let out = self.run_with(
            &["hash-object", "--no-filters", "--stdin"],
            Some(bytes),
            &[],
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    pub(crate) fn blob(&self, id: &str) -> Result<Vec<u8>, Error> {
        self.run(&["cat-file", "blob", id])
    }

    pub(crate) fn tree_of(&self, commit: &str) -> Result<String, Error> {
        self.line(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    pub(crate) fn empty_tree(&self) -> Result<String, Error> {
        let out = self.run_with(&["hash-object", "-t", "tree", "--stdin"], Some(b""), &[])?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// The all-zero object id for this repository's hash.
    pub(crate) fn zero_id(&self) -> Result<String, Error> {
        let format = self.line(&["rev-parse", "--show-object-format"])?;
        Ok("0".repeat(if format == "sha256" { 64 } else { 40 }))
    }

    /// Create a commit object for `tree` with `parents`.
    pub(crate) fn commit_tree(
        &self,
        tree: &str,
        parents: &[&str],
        message: &str,
        author: &crate::Author,
    ) -> Result<String, Error> {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        let out = self.run_with(
            &args,
            Some(message.as_bytes()),
            &[
                ("GIT_AUTHOR_NAME", author.name.as_str()),
                ("GIT_AUTHOR_EMAIL", author.email.as_str()),
            ],
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Move `name` from `old` to `new`, failing if it no longer points at `old`.
    pub(crate) fn update_ref(
        &self,
        name: &str,
        new: &str,
        old: &str,
        why: &str,
    ) -> Result<(), Error> {
        self.run(&["update-ref", "-m", why, name, new, old])
            .map(|_| ())
    }
}

/// A pathspec that matches `path` literally, whatever characters it holds.
pub(crate) fn literal(path: &str) -> String {
    format!(":(top,literal){path}")
}

/// Split NUL-terminated output into UTF-8 records, skipping empties.
pub(crate) fn records(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect()
}

/// One entry of `ls-tree -z` or `diff --raw -z`: `(mode, id)` of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub mode: String,
    pub id: String,
}

/// `ls-tree -r -z` of `paths` (repository-relative, taken literally; empty
/// for the whole tree) in `treeish`, keyed by path.
pub(crate) fn ls_tree(
    git: &Git,
    treeish: &str,
    paths: &[String],
) -> Result<std::collections::BTreeMap<String, Entry>, Error> {
    let mut out = std::collections::BTreeMap::new();
    let whole = [Vec::new()];
    let chunks: Vec<Vec<String>> = if paths.is_empty() {
        whole.to_vec()
    } else {
        paths
            .chunks(200)
            .map(|c| c.iter().map(|p| literal(p)).collect())
            .collect()
    };
    for specs in chunks {
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", treeish, "--"];
        args.extend(specs.iter().map(String::as_str));
        for rec in records(&git.run(&args)?) {
            // "<mode> <type> <id>\t<path>"
            let Some((meta, path)) = rec.split_once('\t') else {
                continue;
            };
            let mut it = meta.split(' ');
            let (Some(mode), Some(_), Some(id)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            out.insert(
                path.to_string(),
                Entry {
                    mode: mode.to_string(),
                    id: id.to_string(),
                },
            );
        }
    }
    Ok(out)
}

/// One path changed between two trees, from `diff --raw -z --no-renames`.
#[derive(Clone, Debug)]
pub(crate) struct Diff {
    pub path: String,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
}

pub(crate) fn diff(
    git: &Git,
    from: &str,
    to: &str,
    pathspec: &[String],
) -> Result<Vec<Diff>, Error> {
    let mut args = vec![
        "diff",
        "--raw",
        "-z",
        "--no-renames",
        "--no-abbrev",
        "--ignore-submodules=all",
        from,
        to,
        "--",
    ];
    args.extend(pathspec.iter().map(String::as_str));
    let recs = records(&git.run(&args)?);
    let mut out = Vec::new();
    let mut it = recs.into_iter();
    while let (Some(meta), Some(path)) = (it.next(), it.next()) {
        // ":<old mode> <new mode> <old id> <new id> <status>"
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if f.len() < 5 {
            continue;
        }
        let side = |mode: &str, id: &str| {
            (mode != "000000").then(|| Entry {
                mode: mode.to_string(),
                id: id.to_string(),
            })
        };
        out.push(Diff {
            path,
            old: side(f[0], f[2]),
            new: side(f[1], f[3]),
        });
    }
    Ok(out)
}

/// A private index file under the git dir, removed when dropped.
pub(crate) struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    pub(crate) fn new(git_dir: &Path) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: git_dir.join(format!("zeroclaw-spaces-index-{}-{n}", std::process::id())),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("lock"));
    }
}

/// Apply `(path, entry or removal)` updates to an index with `--index-info`.
pub(crate) fn index_info(
    git: &Git,
    zero: &str,
    updates: &[(String, Option<Entry>)],
) -> Result<(), Error> {
    if updates.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for (path, entry) in updates {
        match entry {
            Some(e) => input.extend_from_slice(format!("{} {}\t{path}", e.mode, e.id).as_bytes()),
            None => input.extend_from_slice(format!("0 {zero}\t{path}").as_bytes()),
        }
        input.push(0);
    }
    git.run_with(&["update-index", "-z", "--index-info"], Some(&input), &[])
        .map(|_| ())
}
