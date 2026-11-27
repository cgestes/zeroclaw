//! `/api/spaces`: offline-first clients read and edit the folders declared as
//! `[spaces.<id>]`, backed by git (see the `zeroclaw-spaces` crate).
//!
//! Every route requires the same bearer as the rest of the API. A client
//! keeps its own copy and an outbox: it sends each edit with the commit it
//! started from and a unique `change_id`, and catches up with `changes`.

use std::convert::Infallible;
use std::path::PathBuf;

use axum::{
    Json,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use zeroclaw_spaces::{Author, CommitRequest, Edit, Error as SpaceError, Space};

use super::AppState;
use super::api::require_auth;

/// Largest commit body: a handful of documents and attachments.
pub const COMMIT_BODY_CEILING_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct SpaceInfo {
    pub id: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_agent: Option<String>,
    pub title: String,
    /// Current commit; `None` when the folder is not in a git repository.
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct FileQuery {
    pub path: String,
    #[serde(default)]
    pub at: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    pub path: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CommitBody {
    pub change_id: String,
    #[serde(default)]
    pub base: Option<String>,
    /// Shown as the git author, e.g. `Margin (MacBook)`.
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    pub changes: Vec<ChangeBody>,
}

#[derive(Debug, Deserialize)]
pub struct ChangeBody {
    pub path: String,
    /// UTF-8 text.
    #[serde(default)]
    pub content: Option<String>,
    /// Bytes, base64 (standard alphabet).
    #[serde(default)]
    pub content_b64: Option<String>,
    #[serde(default)]
    pub delete: bool,
}

fn error_response(e: &SpaceError) -> Response {
    let status = match e {
        SpaceError::InvalidPath(_) | SpaceError::InvalidChangeId(_) | SpaceError::Invalid(_) => {
            StatusCode::BAD_REQUEST
        }
        SpaceError::NotFound(_) => StatusCode::NOT_FOUND,
        SpaceError::DiskFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
        SpaceError::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        SpaceError::NotARepo(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(json!({"error": e.to_string()})),
            "spaces: request failed"
        );
    }
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

fn not_declared(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("no space '{id}'") })),
    )
        .into_response()
}

fn space_dir(state: &AppState, id: &str) -> Option<PathBuf> {
    state.config.read().space_dir(id)
}

/// Run blocking engine work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SpaceError> + Send + 'static,
) -> Result<T, SpaceError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SpaceError::Invalid(format!("worker stopped: {e}")))?
}

/// Authenticate, resolve the space and open it; any failure is the response.
async fn open(state: &AppState, headers: &HeaderMap, id: &str) -> Result<Space, Box<Response>> {
    require_auth(state, headers).map_err(|e| Box::new(e.into_response()))?;
    let dir = space_dir(state, id).ok_or_else(|| Box::new(not_declared(id)))?;
    blocking(move || Space::open(&dir))
        .await
        .map_err(|e| Box::new(error_response(&e)))
}

/// `GET /api/spaces`
pub async fn handle_list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }
    let declared: Vec<(String, String, Option<String>, String, Option<PathBuf>)> = {
        let config = state.config.read();
        let mut v: Vec<_> = config
            .spaces
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(id, s)| {
                let title = if s.title.is_empty() {
                    id.clone()
                } else {
                    s.title.clone()
                };
                let review_agent = (!s.review_agent.is_empty()).then(|| s.review_agent.clone());
                (
                    id.clone(),
                    s.agent.clone(),
                    review_agent,
                    title,
                    config.space_dir(id),
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    let mut out = Vec::new();
    for (id, agent, review_agent, title, dir) in declared {
        let head = match dir {
            Some(dir) => blocking(move || Space::open(&dir)?.head()).await,
            None => Err(SpaceError::Invalid("space needs `agent` and `path`".into())),
        };
        let (head, error) = match head {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e.to_string())),
        };
        out.push(SpaceInfo {
            id,
            agent,
            review_agent,
            title,
            head,
            error,
        });
    }
    Json(out).into_response()
}

/// `GET /api/spaces/{id}/tree`
pub async fn handle_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match blocking(move || space.tree()).await {
        Ok(tree) => Json(tree).into_response(),
        Err(e) => error_response(&e),
    }
}

/// `GET /api/spaces/{id}/file?path=<rel>&at=<commit>`: raw bytes.
pub async fn handle_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<FileQuery>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let file = match blocking(move || space.read(&q.path, q.at.as_deref())).await {
        Ok(f) => f,
        Err(e) => return error_response(&e),
    };
    let mut response = Response::new(Body::from(file.bytes));
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&file.path)),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", file.blob)) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&file.commit) {
        h.insert("x-commit", v);
    }
    response
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("md" | "markdown") => "text/markdown; charset=utf-8",
        Some("txt" | "csv" | "tsv" | "yml" | "yaml" | "toml") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// `GET /api/spaces/{id}/changes?since=<commit>`
pub async fn handle_changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<ChangesQuery>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match blocking(move || space.changes(q.since.as_deref())).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => error_response(&e),
    }
}

/// `GET /api/spaces/{id}/history?path=<rel>&limit=<n>`
pub async fn handle_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let limit = q.limit.unwrap_or(50);
    match blocking(move || space.history(&q.path, limit)).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => error_response(&e),
    }
}

/// `POST /api/spaces/{id}/commits`
pub async fn handle_commit(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<CommitBody>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let mut edits = Vec::with_capacity(body.changes.len());
    for c in body.changes {
        let content = if c.delete {
            None
        } else if let Some(text) = c.content {
            Some(text.into_bytes())
        } else if let Some(b64) = c.content_b64 {
            match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    return error_response(&SpaceError::Invalid(format!(
                        "{}: content_b64 is not base64: {e}",
                        c.path
                    )));
                }
            }
        } else {
            return error_response(&SpaceError::Invalid(format!(
                "{}: give `content`, `content_b64` or `delete: true`",
                c.path
            )));
        };
        edits.push(Edit {
            path: c.path,
            content,
        });
    }
    let name = body
        .author
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "device".to_string());
    let email = format!("{}@devices.zeroclaw.invalid", slug(&name));
    let req = CommitRequest {
        change_id: body.change_id,
        base: body.base,
        author: Author::new(name, email),
        message: body
            .message
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "Edit".to_string()),
        edits,
    };
    match blocking(move || space.commit(&req)).await {
        Ok(result) => {
            if !result.conflicts.is_empty() {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(json!({"space": id, "conflicts": result.conflicts.len()})),
                    "spaces: merged a commit with overlapping edits"
                );
            }
            Json(result).into_response()
        }
        Err(e) => error_response(&e),
    }
}

/// `GET /api/spaces/{id}/events`: SSE, one `commit` event per commit that
/// touched the space, with space-relative paths. The first event carries the
/// current head so a client can tell whether it missed anything.
pub async fn handle_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let space = match open(&state, &headers, &id).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    // Subscribe before reading the head, so nothing falls in between.
    let rx = zeroclaw_spaces::subscribe();
    let first = {
        let space = space.clone();
        match blocking(move || space.head()).await {
            Ok(head) => json!({ "head": head, "paths": [], "author": "" }),
            Err(e) => return error_response(&e),
        }
    };
    let top = space.repo().top().to_path_buf();
    let hello = tokio_stream::once(Ok::<_, Infallible>(
        SseEvent::default().event("head").data(first.to_string()),
    ));
    let commits = BroadcastStream::new(rx).filter_map(move |item| {
        let event = item.ok()?;
        if event.repo != top {
            return None;
        }
        let paths: Vec<&str> = event
            .paths
            .iter()
            .filter_map(|p| space.relative(p))
            .collect();
        if paths.is_empty() {
            return None;
        }
        let data = json!({ "head": event.head, "paths": paths, "author": event.author });
        Some(Ok::<_, Infallible>(
            SseEvent::default().event("commit").data(data.to_string()),
        ))
    });
    Sse::new(hello.chain(commits))
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "device".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(
            content_type("car/README.md"),
            "text/markdown; charset=utf-8"
        );
        assert_eq!(content_type("x.JSON"), "application/json");
        assert_eq!(content_type("Makefile"), "application/octet-stream");
    }

    #[test]
    fn device_emails_are_slugs() {
        assert_eq!(slug("Margin (MacBook)"), "margin-macbook");
        assert_eq!(slug("!!"), "device");
    }

    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::routing::{get, post};
    use tower::ServiceExt;

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

    /// A git repo holding luc's workspace, with `procedures/` declared as
    /// space `home`.
    fn fixture(require_pairing: bool) -> (tempfile::TempDir, axum::Router, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(ws.join("procedures/car")).expect("mkdir");
        std::fs::write(ws.join("procedures/car/README.md"), "# Car\n").expect("write");
        git(&ws, &["init", "-q"]);
        git(&ws, &["add", "-A"]);
        git(&ws, &["commit", "-qm", "init"]);
        let head = git(&ws, &["rev-parse", "HEAD"]);
        let mut config = zeroclaw_config::schema::Config::default();
        config.gateway.require_pairing = require_pairing;
        let mut agent = zeroclaw_config::schema::AliasedAgentConfig::default();
        agent.workspace.path = Some(ws.clone());
        config.agents.insert("luc".into(), agent);
        config.spaces.insert(
            "home".into(),
            zeroclaw_config::schema::SpaceConfig {
                agent: "luc".into(),
                path: "procedures".into(),
                title: "Home".into(),
                ..Default::default()
            },
        );
        let mut state = crate::api::tests::test_state(config);
        state.pairing =
            std::sync::Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
                require_pairing,
                &["zc_space_test".to_string()],
                zeroclaw_config::pairing::PairingCodePolicy::default(),
            ));
        let app = axum::Router::new()
            .route("/api/spaces", get(handle_list))
            .route("/api/spaces/{id}/tree", get(handle_tree))
            .route("/api/spaces/{id}/file", get(handle_file))
            .route("/api/spaces/{id}/changes", get(handle_changes))
            .route("/api/spaces/{id}/commits", post(handle_commit))
            .with_state(state);
        (tmp, app, head)
    }

    async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let res = app.clone().oneshot(req).await.expect("response");
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), usize::MAX).await.expect("body");
        (status, headers, body.to_vec())
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).expect("request")
    }

    fn json_of(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).expect("json")
    }

    #[tokio::test]
    async fn a_client_reads_commits_and_catches_up() {
        let (_tmp, app, head) = fixture(false);

        let (s, _, b) = call(&app, get_req("/api/spaces")).await;
        assert_eq!(s, StatusCode::OK);
        let list = json_of(&b);
        assert_eq!(list[0]["id"], "home");
        assert_eq!(list[0]["title"], "Home");
        assert_eq!(list[0]["head"], head.as_str());

        let (s, _, b) = call(&app, get_req("/api/spaces/home/tree")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json_of(&b)["files"][0]["path"], "car/README.md");

        let commit = json!({
            "change_id": "test-change-1",
            "base": head,
            "author": "Margin (MacBook)",
            "message": "Add the plate",
            "changes": [
                {"path": "car/README.md", "content": "# Car\n\nPlate: AB-123\n"},
                {"path": "car/photo.bin", "content_b64": "AAEC"}
            ]
        });
        let req = Request::post("/api/spaces/home/commits")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(commit.to_string()))
            .expect("request");
        let (s, _, b) = call(&app, req).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        let result = json_of(&b);
        let new_head = result["head"].as_str().expect("head").to_string();
        assert_ne!(new_head, head);

        let (s, h, b) = call(&app, get_req("/api/spaces/home/file?path=car/README.md")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, b"# Car\n\nPlate: AB-123\n");
        assert_eq!(h[header::CONTENT_TYPE], "text/markdown; charset=utf-8");
        assert!(h.contains_key(header::ETAG));
        assert_eq!(h["x-commit"], new_head.as_str());

        let (s, _, b) = call(&app, get_req("/api/spaces/home/file?path=car/photo.bin")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, vec![0u8, 1, 2]);

        let (s, _, b) = call(
            &app,
            get_req(&format!("/api/spaces/home/changes?since={head}")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let changes = json_of(&b);
        assert_eq!(changes["head"], new_head.as_str());
        assert_eq!(changes["changes"].as_array().map(Vec::len), Some(2));

        let (s, _, _) = call(&app, get_req("/api/spaces/home/file?path=../../etc/passwd")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = call(&app, get_req("/api/spaces/nope/tree")).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn spaces_require_the_bearer_when_pairing_is_on() {
        let (_tmp, app, _) = fixture(true);
        for uri in [
            "/api/spaces",
            "/api/spaces/home/tree",
            "/api/spaces/home/file?path=car/README.md",
        ] {
            let (s, _, _) = call(&app, get_req(uri)).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
            let authed = Request::get(uri)
                .header(header::AUTHORIZATION, "Bearer zc_space_test")
                .body(Body::empty())
                .expect("request");
            let (s, _, _) = call(&app, authed).await;
            assert_eq!(s, StatusCode::OK, "{uri} with the bearer");
        }
    }

    #[test]
    fn engine_errors_map_to_http_statuses() {
        let status = |e: SpaceError| error_response(&e).status();
        assert_eq!(
            status(SpaceError::InvalidPath("../x".into())),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(SpaceError::NotFound("a".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(SpaceError::DiskFull {
                available: 1,
                needed: 2
            }),
            StatusCode::INSUFFICIENT_STORAGE
        );
        assert_eq!(
            status(SpaceError::Busy("x".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
