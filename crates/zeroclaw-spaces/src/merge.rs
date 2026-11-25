//! Merging an incoming commit into HEAD without ever dropping content.
//!
//! Per file, between the merge base (`b`), HEAD (`o`) and incoming (`t`):
//! one-sided changes win; overlapping text edits keep both sides (git's
//! union merge) and are reported; other files keep HEAD's version at the
//! path and the incoming one as a conflict copy; an edit beats a delete.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::git::{self, Entry, Git, TempIndex};
use crate::{Author, Error};

/// What happened to a file whose two sides could not simply be combined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// Path relative to the space (or the repository for remote merges).
    pub path: String,
    pub kind: ConflictKind,
    /// For [`ConflictKind::Copy`]: where the incoming version was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Overlapping text edits; both sides' lines were kept.
    Both,
    /// Not mergeable as text; the incoming version is in `copy`.
    Copy,
    /// Incoming deleted a file HEAD had edited; HEAD's edit was kept.
    KeptEdit,
    /// HEAD deleted a file incoming had edited; the edit was restored.
    Restored,
}

/// Extensions merged line by line. Anything else (JSON, binaries, files
/// without an extension) gets a conflict copy, because a line merge could
/// leave it unparsable.
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "markdown", "mdx", "txt", "text", "org", "rst", "adoc", "csv", "tsv", "yml", "yaml",
];

pub(crate) struct Merged {
    pub commit: String,
    pub conflicts: Vec<Conflict>,
}

/// Which paths a merge may change, and how conflicts name them.
pub(crate) struct Scope<'a> {
    /// Paths incoming may change; others keep HEAD's version.
    pub keep: &'a dyn Fn(&str) -> bool,
    /// Prefix removed from reported conflict paths.
    pub strip: &'a str,
}

/// Merge `incoming` into `head` and return the merge commit.
pub(crate) fn merge(
    git: &Git,
    git_dir: &std::path::Path,
    head: &str,
    incoming: &str,
    (author, message): (&Author, &str),
    scope: &Scope<'_>,
) -> Result<Merged, Error> {
    let (keep, strip) = (scope.keep, scope.strip);
    let base = match git
        .output(&["merge-base", head, incoming], None, &[], None)?
        .stdout
    {
        out if !out.is_empty() => String::from_utf8_lossy(&out).trim().to_string(),
        _ => git.empty_tree()?,
    };
    let incoming_changes: Vec<git::Diff> = git::diff(git, &base, incoming, &[])?
        .into_iter()
        .filter(|d| keep(&d.path))
        .collect();
    let paths: Vec<String> = incoming_changes.iter().map(|d| d.path.clone()).collect();
    let ours = git::ls_tree(git, head, &paths)?;
    let head_tree = git::ls_tree(git, head, &[])?;
    let mut taken: BTreeSet<String> = head_tree.keys().cloned().collect();
    let empty_blob = git.write_blob(b"")?;
    let when = stamp();

    let mut updates: Vec<(String, Option<Entry>)> = Vec::new();
    let mut conflicts = Vec::new();
    for change in incoming_changes {
        let path = change.path;
        let b = change.old;
        let t = change.new;
        let o = ours.get(&path).cloned();
        if t == o {
            continue;
        }
        let report = |kind, copy: Option<String>| Conflict {
            path: path.strip_prefix(strip).unwrap_or(&path).to_string(),
            kind,
            copy: copy.map(|c| c.strip_prefix(strip).unwrap_or(&c).to_string()),
        };
        if o == b {
            // Only incoming changed it.
            updates.push((path.clone(), t));
            continue;
        }
        match (o, t) {
            (Some(_), None) => conflicts.push(report(ConflictKind::KeptEdit, None)),
            (None, Some(t)) => {
                updates.push((path.clone(), Some(t)));
                conflicts.push(report(ConflictKind::Restored, None));
            }
            (Some(o), Some(t)) => {
                let base_id = b.as_ref().map_or(empty_blob.as_str(), |e| e.id.as_str());
                // Without a common ancestor a line merge would duplicate the
                // whole file, so two independent versions get a copy.
                if b.is_some() && mergeable(git, &path, &o, &t)? {
                    let (id, clean) = merge_text(git, &o.id, base_id, &t.id)?;
                    updates.push((
                        path.clone(),
                        Some(Entry {
                            mode: o.mode.clone(),
                            id,
                        }),
                    ));
                    if !clean {
                        conflicts.push(report(ConflictKind::Both, None));
                    }
                } else {
                    let copy = copy_path(&path, &author.name, &when, &taken);
                    taken.insert(copy.clone());
                    updates.push((copy.clone(), Some(t)));
                    conflicts.push(report(ConflictKind::Copy, Some(copy)));
                }
            }
            (None, None) => {}
        }
    }

    let index = TempIndex::new(git_dir);
    let ig = git.with_index(index.path());
    ig.run(&["read-tree", head])?;
    git::index_info(&ig, &git.zero_id()?, &updates)?;
    let tree = ig.line(&["write-tree"])?;
    let commit = git.commit_tree(&tree, &[head, incoming], message, author)?;
    Ok(Merged { commit, conflicts })
}

fn mergeable(git: &Git, path: &str, o: &Entry, t: &Entry) -> Result<bool, Error> {
    if o.mode == "120000" || t.mode == "120000" {
        return Ok(false);
    }
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()))
        .map(|(_, ext)| ext.to_ascii_lowercase());
    if !ext.is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str())) {
        return Ok(false);
    }
    let text = |id: &str| -> Result<bool, Error> {
        let bytes = git.blob(id)?;
        Ok(!bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok())
    };
    Ok(text(&o.id)? && text(&t.id)?)
}

/// Three-way merge of blobs. Returns the result blob and whether the merge
/// was clean; overlapping hunks keep both sides' lines.
fn merge_text(git: &Git, ours: &str, base: &str, theirs: &str) -> Result<(String, bool), Error> {
    let plain = git.output(
        &["merge-file", "--object-id", ours, base, theirs],
        None,
        &[],
        None,
    )?;
    if plain.code == 0 {
        return Ok((
            String::from_utf8_lossy(&plain.stdout).trim().to_string(),
            true,
        ));
    }
    if !(1..=127).contains(&plain.code) {
        return Err(Error::Git {
            args: "merge-file --object-id".into(),
            code: plain.code,
            stderr: String::from_utf8_lossy(&plain.stderr).trim().to_string(),
        });
    }
    let union = git.line(&["merge-file", "--object-id", "--union", ours, base, theirs])?;
    Ok((union, false))
}

/// `dir/name.conflict-<author>-<when>.ext`, unique among `taken`.
fn copy_path(path: &str, author: &str, when: &str, taken: &BTreeSet<String>) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n),
        None => (String::new(), path),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    let who = slug(author);
    let mut n = 1;
    loop {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let candidate = format!("{dir}{stem}.conflict-{who}-{when}{suffix}{ext}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}

/// Current UTC time as `YYYYMMDD-HHMMSS`.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// Civil date from Unix seconds (Howard Hinnant's algorithm).
pub(crate) fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_names_keep_the_extension_and_stay_unique() {
        let mut taken = BTreeSet::new();
        let a = copy_path(
            "car/notes.json",
            "Margin (MacBook)",
            "20261125-101500",
            &taken,
        );
        assert_eq!(a, "car/notes.conflict-margin-macbook-20261125-101500.json");
        taken.insert(a);
        let b = copy_path(
            "car/notes.json",
            "Margin (MacBook)",
            "20261125-101500",
            &taken,
        );
        assert_eq!(
            b,
            "car/notes.conflict-margin-macbook-20261125-101500-2.json"
        );
        assert_eq!(
            copy_path("Makefile", "luc", "x", &BTreeSet::new()),
            "Makefile.conflict-luc-x"
        );
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "19700101-000000");
        assert_eq!(format_utc(1_795_633_000), "20261125-185640");
    }

    #[test]
    fn conflict_kinds_serialize_in_snake_case() {
        let c = Conflict {
            path: "a.md".into(),
            kind: ConflictKind::KeptEdit,
            copy: None,
        };
        assert_eq!(
            serde_json::to_string(&c).ok().as_deref(),
            Some(r#"{"path":"a.md","kind":"kept_edit"}"#)
        );
    }
}
