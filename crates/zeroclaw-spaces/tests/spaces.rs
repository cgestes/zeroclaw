//! End-to-end behaviour of a space over real git repositories: clients that
//! edit from old versions, agents that edit the working tree meanwhile, and a
//! remote that laptops push to.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use zeroclaw_spaces::{Author, CommitRequest, ConflictKind, Edit, Error, Repo, Space};

const SPACE: &str = "agents/luc/workspace/procedures";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    top: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let top = tmp.path().join("zeroclaw");
        std::fs::create_dir_all(top.join(SPACE).join("car")).expect("mkdir");
        git(&top, &["init", "-q"]);
        std::fs::write(top.join("config.toml"), "secret = 1\n").expect("write");
        std::fs::write(top.join(".gitignore"), "config.toml\n").expect("write");
        std::fs::write(top.join("AGENTS.md"), "# Agents\n").expect("write");
        std::fs::write(
            top.join(SPACE).join("car/README.md"),
            "# Car\n\nPlate: AB-123\n\n## Service\n\nOil changed in May.\n\n## Tyres\n\nWinter tyres in the garage.\n",
        )
        .expect("write");
        std::fs::write(top.join(SPACE).join("car/notes.json"), "{\"a\":1}\n").expect("write");
        std::fs::write(top.join(SPACE).join("shopping.md"), "- [ ] milk\n").expect("write");
        git(&top, &["add", "-A"]);
        git(&top, &["commit", "-qm", "initial"]);
        let top = top.canonicalize().expect("canonical");
        Self { _tmp: tmp, top }
    }

    fn space(&self) -> Space {
        Space::open(&self.top.join(SPACE)).expect("space opens")
    }

    fn head(&self) -> String {
        git(&self.top, &["rev-parse", "HEAD"])
    }

    fn disk(&self, rel: &str) -> String {
        std::fs::read_to_string(self.top.join(SPACE).join(rel)).expect("read")
    }

    fn agent_writes(&self, rel: &str, text: &str) {
        std::fs::write(self.top.join(SPACE).join(rel), text).expect("write");
    }

    /// A file at a revision, byte for byte (no trimming).
    fn at(&self, rev: &str, rel: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.top)
            .args(["show", &format!("{rev}:{SPACE}/{rel}")])
            .output()
            .expect("git runs");
        assert!(out.status.success());
        String::from_utf8(out.stdout).expect("utf8")
    }
}

fn request(id: &str, base: Option<&str>, edits: &[(&str, Option<&str>)]) -> CommitRequest {
    CommitRequest {
        change_id: id.to_string(),
        base: base.map(str::to_string),
        author: Author::new("Margin (MacBook)", "margin@example.invalid"),
        message: "Edit from Margin".into(),
        edits: edits
            .iter()
            .map(|(p, c)| Edit {
                path: p.to_string(),
                content: c.map(|t| t.as_bytes().to_vec()),
            })
            .collect(),
    }
}

#[test]
fn an_up_to_date_client_fast_forwards() {
    let f = Fixture::new();
    let base = f.head();
    let r = f
        .space()
        .commit(&request(
            "change-0001",
            Some(&base),
            &[("shopping.md", Some("- [ ] milk\n- [ ] eggs\n"))],
        ))
        .expect("commit");
    assert!(!r.merged);
    assert_eq!(r.head, r.commit, "fast-forward to the client's own commit");
    assert_eq!(f.head(), r.head);
    assert_eq!(f.disk("shopping.md"), "- [ ] milk\n- [ ] eggs\n");
    assert_eq!(
        git(&f.top, &["log", "-1", "--format=%an"]),
        "Margin (MacBook)"
    );
    assert!(git(&f.top, &["log", "-1", "--format=%B"]).contains("Change-Id: change-0001"));
    assert_eq!(
        git(&f.top, &["status", "--porcelain", "--", SPACE]),
        "",
        "index stays truthful"
    );
}

#[test]
fn a_stale_client_merges_with_an_uncommitted_agent_edit() {
    let f = Fixture::new();
    let base = f.head();
    // luc edits the Tyres section on disk and has not committed.
    let agent = f
        .disk("car/README.md")
        .replace("Winter tyres in the garage.", "Winter tyres fitted 20 Nov.");
    f.agent_writes("car/README.md", &agent);
    zeroclaw_spaces::record_write(
        &f.top.join(SPACE).join("car/README.md"),
        Author::new("luc", "luc@zeroclaw.invalid"),
    );
    // The phone, offline since `base`, edited the Service section.
    let phone = f
        .at(&base, "car/README.md")
        .replace("Oil changed in May.", "Oil changed in May. Next: November.");
    let r = f
        .space()
        .commit(&request(
            "change-0002",
            Some(&base),
            &[("car/README.md", Some(&phone))],
        ))
        .expect("commit");
    assert!(r.merged);
    assert!(
        r.conflicts.is_empty(),
        "different sections merge cleanly: {:?}",
        r.conflicts
    );
    let text = f.disk("car/README.md");
    assert!(
        text.contains("Winter tyres fitted 20 Nov."),
        "agent's edit kept"
    );
    assert!(text.contains("Next: November."), "phone's edit kept");
    assert_eq!(f.at("HEAD", "car/README.md"), text, "disk matches HEAD");
    // The agent's edit is its own commit, authored by the agent.
    let authors = git(&f.top, &["log", "--format=%an", "-4"]);
    assert!(authors.lines().any(|a| a == "luc"), "authors: {authors}");
}

#[test]
fn overlapping_text_edits_keep_both_sides() {
    let f = Fixture::new();
    let base = f.head();
    f.agent_writes(
        "car/README.md",
        &f.disk("car/README.md").replace("AB-123", "AB-124"),
    );
    let mine = f.at(&base, "car/README.md").replace("AB-123", "AB-125");
    let r = f
        .space()
        .commit(&request(
            "change-0003",
            Some(&base),
            &[("car/README.md", Some(&mine))],
        ))
        .expect("commit");
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].kind, ConflictKind::Both);
    assert_eq!(r.conflicts[0].path, "car/README.md");
    let text = f.disk("car/README.md");
    assert!(
        text.contains("AB-124") && text.contains("AB-125"),
        "both kept:\n{text}"
    );
    assert!(!text.contains("<<<<<<<"), "no conflict markers");
}

#[test]
fn a_json_conflict_becomes_a_copy() {
    let f = Fixture::new();
    let base = f.head();
    f.agent_writes("car/notes.json", "{\"a\":2}\n");
    let r = f
        .space()
        .commit(&request(
            "change-0004",
            Some(&base),
            &[("car/notes.json", Some("{\"a\":3}\n"))],
        ))
        .expect("commit");
    assert_eq!(r.conflicts.len(), 1);
    let c = &r.conflicts[0];
    assert_eq!(c.kind, ConflictKind::Copy);
    let copy = c.copy.as_deref().expect("copy path");
    assert!(
        copy.starts_with("car/notes.conflict-margin-macbook-"),
        "{copy}"
    );
    assert!(copy.ends_with(".json"));
    assert_eq!(
        f.disk("car/notes.json"),
        "{\"a\":2}\n",
        "HEAD's version stays in place"
    );
    assert_eq!(
        f.disk(copy),
        "{\"a\":3}\n",
        "the client's version is kept beside it"
    );
}

#[test]
fn an_edit_beats_a_delete_both_ways() {
    let f = Fixture::new();
    let base = f.head();
    // The agent edits shopping.md while the client deletes it.
    f.agent_writes("shopping.md", "- [ ] milk\n- [ ] bread\n");
    let r = f
        .space()
        .commit(&request(
            "change-0005",
            Some(&base),
            &[("shopping.md", None)],
        ))
        .expect("commit");
    assert_eq!(r.conflicts[0].kind, ConflictKind::KeptEdit);
    assert_eq!(f.disk("shopping.md"), "- [ ] milk\n- [ ] bread\n");

    // The agent deletes car/notes.json while the client edits it.
    let base = f.head();
    std::fs::remove_file(f.top.join(SPACE).join("car/notes.json")).expect("rm");
    let r = f
        .space()
        .commit(&request(
            "change-0006",
            Some(&base),
            &[("car/notes.json", Some("{\"a\":9}\n"))],
        ))
        .expect("commit");
    assert_eq!(r.conflicts[0].kind, ConflictKind::Restored);
    assert_eq!(f.disk("car/notes.json"), "{\"a\":9}\n");
}

#[test]
fn a_retry_returns_the_first_answer() {
    let f = Fixture::new();
    let base = f.head();
    let req = request(
        "change-0007",
        Some(&base),
        &[("shopping.md", Some("- [x] milk\n"))],
    );
    let first = f.space().commit(&req).expect("first");
    let second = f.space().commit(&req).expect("retry");
    assert_eq!(first, second);
    let count = git(
        &f.top,
        &["log", "--format=%H", "--grep=Change-Id: change-0007"],
    );
    assert_eq!(count.lines().count(), 1, "one client commit");
}

#[test]
fn a_crash_after_intake_is_finished_by_the_retry() {
    let f = Fixture::new();
    let base = f.head();
    let req = request(
        "change-0008",
        Some(&base),
        &[("shopping.md", Some("- [ ] milk\n- [ ] jam\n"))],
    );
    f.space().commit(&req).expect("commit");
    // Undo everything but the intake, as if the daemon died right after it.
    git(
        &f.top,
        &["update-ref", "-d", "refs/spaces/results/change-0008"],
    );
    git(&f.top, &["reset", "-q", "--hard", &base]);
    assert_eq!(f.disk("shopping.md"), "- [ ] milk\n");
    let r = f.space().commit(&req).expect("retry");
    assert_eq!(f.disk("shopping.md"), "- [ ] milk\n- [ ] jam\n");
    assert_eq!(f.head(), r.head);
}

#[test]
fn an_unknown_base_never_overwrites() {
    let f = Fixture::new();
    let r = f
        .space()
        .commit(&request(
            "change-0009",
            Some("0123456789abcdef0123456789abcdef01234567"),
            &[
                ("shopping.md", Some("- [ ] tea\n")),
                ("new.md", Some("# New\n")),
            ],
        ))
        .expect("commit");
    assert_eq!(
        f.disk("shopping.md"),
        "- [ ] milk\n",
        "HEAD's version stays"
    );
    assert_eq!(f.disk("new.md"), "# New\n", "new files are simply added");
    let copy = r
        .conflicts
        .iter()
        .find_map(|c| c.copy.clone())
        .expect("a copy");
    assert_eq!(f.disk(&copy), "- [ ] tea\n");
}

#[test]
fn paths_cannot_leave_the_space() {
    let f = Fixture::new();
    for bad in ["../../AGENTS.md", "/etc/passwd", ".git/config"] {
        let err = f
            .space()
            .commit(&request("change-0010", None, &[(bad, Some("x"))]))
            .expect_err("refused");
        assert!(matches!(err, Error::InvalidPath(_)), "{bad}: {err}");
    }
    assert_eq!(
        std::fs::read_to_string(f.top.join("AGENTS.md")).expect("read"),
        "# Agents\n"
    );
}

#[test]
fn changes_cover_only_the_space_and_report_deletions() {
    let f = Fixture::new();
    let since = f.head();
    std::fs::write(f.top.join("AGENTS.md"), "# Agents\n\nchanged\n").expect("write");
    git(&f.top, &["commit", "-qam", "outside"]);
    std::fs::remove_file(f.top.join(SPACE).join("shopping.md")).expect("rm");
    f.agent_writes("car/README.md", "# Car\n");
    let c = f.space().changes(Some(&since)).expect("changes");
    assert!(!c.full);
    let mut paths: Vec<(String, bool)> = c
        .changes
        .iter()
        .map(|x| (x.path.clone(), x.blob.is_some()))
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("car/README.md".to_string(), true),
            ("shopping.md".to_string(), false)
        ]
    );
    let full = f.space().changes(Some("not-a-commit")).expect("changes");
    assert!(full.full, "an unknown `since` lists everything");
    assert_eq!(full.changes.len(), 2);
    let tree = f.space().tree().expect("tree");
    assert_eq!(tree.files.len(), 2);
    assert!(tree.files.iter().all(|e| e.size > 0));
    let file = f.space().read("car/README.md", None).expect("read");
    assert_eq!(file.bytes, b"# Car\n");
    assert_eq!(
        f.space()
            .history("car/README.md", 10)
            .expect("history")
            .len(),
        2
    );
}

#[test]
fn the_committer_waits_for_quiet_files() {
    let f = Fixture::new();
    let repo = Repo::open(&f.top).expect("repo");
    let spec = vec![":/".to_string()];
    f.agent_writes("shopping.md", "- [ ] milk\n- [ ] rice\n");
    let mut pending = None;
    let events = repo
        .commit_quiet(
            &spec,
            Duration::from_secs(3600),
            &mut pending,
            Duration::from_secs(3600),
        )
        .expect("quiet check");
    assert!(events.is_empty(), "a fresh edit waits");
    assert!(pending.is_some());
    // Pretend it has been pending for a long time.
    pending = Some(SystemTime::now() - Duration::from_secs(7200));
    let events = repo
        .commit_quiet(
            &spec,
            Duration::from_secs(3600),
            &mut pending,
            Duration::from_secs(3600),
        )
        .expect("overdue");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].paths, vec![format!("{SPACE}/shopping.md")]);
    assert_eq!(events[0].author, "external");
    assert!(pending.is_none());
    assert_eq!(
        git(&f.top, &["status", "--porcelain"]),
        "",
        "config.toml is ignored, everything else committed"
    );
}

#[test]
fn a_remote_round_trip_merges_laptop_commits_and_pushes() {
    let f = Fixture::new();
    let remote = f.top.parent().expect("parent").join("remote.git");
    git(
        f.top.parent().expect("parent"),
        &["init", "-q", "--bare", "remote.git"],
    );
    git(
        &f.top,
        &["remote", "add", "origin", remote.to_str().expect("utf8")],
    );
    git(&f.top, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    // A laptop clones, edits the Service section and pushes.
    let laptop = f.top.parent().expect("parent").join("laptop");
    git(
        f.top.parent().expect("parent"),
        &["clone", "-q", remote.to_str().expect("utf8"), "laptop"],
    );
    let path = laptop.join(SPACE).join("car/README.md");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(
        &path,
        text.replace("Oil changed in May.", "Oil changed in May (laptop)."),
    )
    .expect("write");
    git(&laptop, &["commit", "-qam", "laptop edit"]);
    git(&laptop, &["push", "-q", "origin", "HEAD:main"]);

    // Meanwhile luc edits the Tyres section on the server.
    f.agent_writes(
        "car/README.md",
        &f.disk("car/README.md")
            .replace("Winter tyres in the garage.", "Winter tyres fitted."),
    );

    let repo = Repo::open(&f.top).expect("repo");
    let out = repo
        .sync_remote(
            "origin",
            "main",
            &["config.toml".to_string()],
            Duration::from_secs(30),
        )
        .expect("sync");
    assert!(out.merged && out.pushed, "{out:?}");
    let text = f.disk("car/README.md");
    assert!(
        text.contains("(laptop)") && text.contains("fitted"),
        "{text}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "main"]),
        f.head(),
        "the remote has the merge"
    );

    // The laptop's next pull is a fast-forward.
    git(&laptop, &["pull", "-q", "--ff-only", "origin", "main"]);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        text,
        "laptop and server agree"
    );
    let again = repo
        .sync_remote(
            "origin",
            "main",
            &["config.toml".to_string()],
            Duration::from_secs(30),
        )
        .expect("second sync");
    assert!(
        !again.merged && !again.pushed,
        "nothing left to do: {again:?}"
    );
}

#[test]
fn every_commit_is_announced() {
    let f = Fixture::new();
    let mut rx = zeroclaw_spaces::subscribe();
    let base = f.head();
    let r = f
        .space()
        .commit(&request(
            "change-0011",
            Some(&base),
            &[("shopping.md", Some("- [x] milk\n"))],
        ))
        .expect("commit");
    let mut seen = None;
    while let Ok(e) = rx.try_recv() {
        if e.repo == f.top && e.head == r.head {
            seen = Some(e);
        }
    }
    let e = seen.expect("event for the commit");
    assert_eq!(e.paths, vec![format!("{SPACE}/shopping.md")]);
}
