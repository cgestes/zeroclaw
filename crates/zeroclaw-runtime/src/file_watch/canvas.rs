//! Canvas mirror action for file watchers: pure document transforms.
//!
//! Everything here is side-effect free so it can be unit tested; the watcher
//! service (`super`) owns the canvas store and the per-file state.

use regex::Regex;
use std::sync::LazyLock;

/// Strip the extension of the last path segment (`a/b.md` → `a/b`).
pub fn strip_ext(rel_path: &str) -> &str {
    let file_start = rel_path.rfind('/').map_or(0, |i| i + 1);
    match rel_path[file_start..].rfind('.') {
        Some(dot) if dot > 0 => &rel_path[..file_start + dot],
        _ => rel_path,
    }
}

/// Canvas id for a document: relative path without extension, `/` → `-`.
/// A folder `README` maps to the folder name; the root `README` maps to
/// `<index_id>-readme` (or `readme` without an index id).
pub fn canvas_id(rel_path: &str, index_id: &str) -> String {
    let stem = strip_ext(rel_path);
    let parts: Vec<&str> = stem.split('/').filter(|p| !p.is_empty()).collect();
    match parts.split_last() {
        Some((last, dirs)) if last.eq_ignore_ascii_case("readme") => {
            if dirs.is_empty() {
                if index_id.is_empty() {
                    "readme".to_string()
                } else {
                    format!("{index_id}-readme")
                }
            } else {
                dirs.join("-")
            }
        }
        _ => parts.join("-"),
    }
}

/// Source URL of a document under `base` (`.md` dropped, other files kept).
pub fn source_url(base: &str, rel_path: &str) -> String {
    if base.is_empty() {
        return String::new();
    }
    let page = rel_path.strip_suffix(".md").unwrap_or(rel_path);
    if base.ends_with('/') {
        format!("{base}{page}")
    } else {
        format!("{base}/{page}")
    }
}

/// Normalise `dir/target` (handles `.` and `..`); `None` if it escapes the root.
fn resolve_relative(from_rel_path: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = from_rel_path.split('/').collect();
    parts.pop(); // drop the file name -> containing directory
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\]\(([^)\s]+)\)").expect("static regex"));

/// Rewrite relative `.md` links to absolute source URLs under `base`.
/// External links, absolute paths and pure anchors are left untouched.
pub fn rewrite_links(content: &str, rel_path: &str, base: &str) -> String {
    if base.is_empty() {
        return content.to_string();
    }
    MD_LINK
        .replace_all(content, |caps: &regex::Captures<'_>| {
            let target = &caps[1];
            let untouched = || caps[0].to_string();
            if target.contains("://")
                || target.starts_with('/')
                || target.starts_with('#')
                || target.starts_with("mailto:")
            {
                return untouched();
            }
            let (path, anchor) = match target.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (target, None),
            };
            if !path.ends_with(".md") {
                return untouched();
            }
            let Some(resolved) = resolve_relative(rel_path, path) else {
                return untouched();
            };
            let mut url = source_url(base, &resolved);
            if let Some(a) = anchor {
                url.push('#');
                url.push_str(a);
            }
            format!("]({url})")
        })
        .into_owned()
}

/// Insert `line` right under the first `# ` heading (or at the top).
pub fn insert_header(content: &str, line: &str) -> String {
    if line.is_empty() {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    let trailing_nl = if content.ends_with('\n') { "\n" } else { "" };
    match lines.iter().position(|l| l.starts_with("# ")) {
        Some(i) => {
            let mut out = lines[..=i].join("\n");
            out.push('\n');
            out.push_str(line);
            out.push('\n');
            let rest = &lines[i + 1..];
            if rest.first().is_some_and(|l| !l.trim().is_empty()) {
                out.push('\n');
            }
            out.push_str(&rest.join("\n"));
            if !rest.is_empty() {
                out.push_str(trailing_nl);
            }
            out
        }
        None => format!("{line}\n\n{content}"),
    }
}

/// A stub has nothing but headings / blank lines.
pub fn is_stub(content: &str) -> bool {
    content
        .lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.starts_with('#'))
}

/// Document title: the first `# ` heading, else the relative path.
pub fn title(content: &str, rel_path: &str) -> String {
    content
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| rel_path.to_string())
}

/// Full mirrored document: header under the H1, links rewritten.
///
/// `header_template` placeholders: `{path}` (`<root_name>/<rel_path>`),
/// `{rel_path}`, `{synced}`, `{url}`.
pub fn render_document(
    content: &str,
    rel_path: &str,
    base: &str,
    header_template: &str,
    root_name: &str,
    synced: &str,
) -> String {
    let url = source_url(base, rel_path);
    let path = if root_name.is_empty() {
        rel_path.to_string()
    } else {
        format!("{root_name}/{rel_path}")
    };
    let header = header_template
        .replace("{path}", &path)
        .replace("{rel_path}", rel_path)
        .replace("{synced}", synced)
        .replace("{url}", &url);
    insert_header(&rewrite_links(content, rel_path, base), &header)
}

/// One entry of the generated index.
pub struct IndexEntry<'a> {
    pub rel_path: &'a str,
    pub title: &'a str,
    pub canvas_id: &'a str,
}

/// Index body grouped by folder (no timestamp, so callers can diff it).
/// `entries` must be sorted by `rel_path`.
pub fn render_index_body(root_name: &str, base: &str, entries: &[IndexEntry<'_>]) -> String {
    let mut out = String::new();
    let mut last_dir: Option<&str> = None;
    for e in entries {
        let dir = e.rel_path.rsplit_once('/').map_or("", |(d, _)| d);
        if last_dir != Some(dir) {
            let heading = if dir.is_empty() { root_name } else { dir };
            out.push_str(&format!("\n**{heading}/**\n\n"));
            last_dir = Some(dir);
        }
        let url = source_url(base, e.rel_path);
        let name = if url.is_empty() {
            e.title.to_string()
        } else {
            format!("[{}]({url})", e.title)
        };
        out.push_str(&format!(
            "- {name} — `{}` → canvas `{}`\n",
            e.rel_path, e.canvas_id
        ));
    }
    out
}

/// Full index document.
pub fn render_index(root_name: &str, base: &str, synced: &str, body: &str, count: usize) -> String {
    let browse = if base.is_empty() {
        String::new()
    } else {
        format!(" · [browse / edit]({base})")
    };
    format!(
        "# {root_name}\n*Index of {root_name}/ · {count} docs · synced {synced}{browse}*\n{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_ids_follow_convention() {
        assert_eq!(canvas_id("wedding/nz.md", "procedures"), "wedding-nz");
        assert_eq!(canvas_id("wedding/README.md", "procedures"), "wedding");
        assert_eq!(canvas_id("reminders.md", "procedures"), "reminders");
        assert_eq!(canvas_id("README.md", "procedures"), "procedures-readme");
        assert_eq!(canvas_id("README.md", ""), "readme");
        assert_eq!(canvas_id("a/b/c.v2.md", ""), "a-b-c.v2");
        assert_eq!(canvas_id("noext", ""), "noext");
    }

    #[test]
    fn source_urls_drop_md() {
        let base = "https://sb.example:8443/";
        assert_eq!(source_url(base, "wedding/nz.md"), "https://sb.example:8443/wedding/nz");
        assert_eq!(source_url("https://x", "a.md"), "https://x/a");
        assert_eq!(source_url("", "a.md"), "");
    }

    #[test]
    fn links_are_rewritten_relative_to_the_document() {
        let base = "https://sb/";
        let src = "See [NZ](nz.md), [top](../reminders.md#due), [web](https://e.com/x.md), \
                   [anchor](#h), [img](pic.png) and [out](../../etc.md).";
        let got = rewrite_links(src, "wedding/README.md", base);
        assert_eq!(
            got,
            "See [NZ](https://sb/wedding/nz), [top](https://sb/reminders#due), \
             [web](https://e.com/x.md), [anchor](#h), [img](pic.png) and [out](../../etc.md)."
        );
        assert_eq!(rewrite_links(src, "wedding/README.md", ""), src);
    }

    #[test]
    fn header_goes_under_first_h1() {
        assert_eq!(
            insert_header("# T\n\nbody\n", "*h*"),
            "# T\n*h*\n\nbody\n"
        );
        assert_eq!(insert_header("# T\nbody", "*h*"), "# T\n*h*\n\nbody");
        assert_eq!(insert_header("# T\n", "*h*"), "# T\n*h*\n");
        assert_eq!(insert_header("no heading", "*h*"), "*h*\n\nno heading");
        assert_eq!(insert_header("# T\n", ""), "# T\n");
    }

    #[test]
    fn stubs_and_titles() {
        assert!(is_stub("# DM — misc\n\n"));
        assert!(is_stub(""));
        assert!(!is_stub("# T\n\nreal text"));
        assert_eq!(title("intro\n# Hello \nx", "a.md"), "Hello");
        assert_eq!(title("no title", "a.md"), "a.md");
    }

    #[test]
    fn document_render_fills_template() {
        let got = render_document(
            "# NZ\n\nSee [r](../reminders.md).\n",
            "wedding/nz.md",
            "https://sb/",
            "*Mirror of {path} · synced {synced} · [edit]({url})*",
            "procedures",
            "2026-11-25 01:00",
        );
        assert_eq!(
            got,
            "# NZ\n*Mirror of procedures/wedding/nz.md · synced 2026-11-25 01:00 · \
             [edit](https://sb/wedding/nz)*\n\nSee [r](https://sb/reminders).\n"
        );
    }

    #[test]
    fn index_groups_by_folder() {
        let entries = [
            IndexEntry { rel_path: "README.md", title: "Procedures", canvas_id: "p-readme" },
            IndexEntry { rel_path: "wedding/nz.md", title: "NZ", canvas_id: "wedding-nz" },
        ];
        let body = render_index_body("procedures", "https://sb/", &entries);
        assert_eq!(
            body,
            "\n**procedures/**\n\n- [Procedures](https://sb/README) — `README.md` → canvas `p-readme`\n\
             \n**wedding/**\n\n- [NZ](https://sb/wedding/nz) — `wedding/nz.md` → canvas `wedding-nz`\n"
        );
    }
}
