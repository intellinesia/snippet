//! Pull request review through the GitHub CLI. The daemon never holds a
//! GitHub token: every call is `gh`, run non-interactively in the session's
//! workspace, so "logged in to gh" is the only requirement and private repos
//! and GitHub Enterprise work the same way.

use super::api_error;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use super::Shared;
use super::session_routes::resolve_session_dir;
use crate::session::{SessionInfo, SessionPr};

const GH_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const PR_TTL_SECS: i64 = 300;
const BRANCH_PR_FIELDS: &str = "number,title,state,isDraft,mergeable,statusCheckRollup,url";

static PR_CACHE: LazyLock<Mutex<PrCache>> = LazyLock::new(|| Mutex::new(PrCache::default()));
static PR_SEMAPHORE: Semaphore = Semaphore::const_new(4);

#[derive(Debug, Clone)]
struct CachedPr {
    branch: String,
    folder: String,
    pr: Option<SessionPr>,
    fetched_at: i64,
}

#[derive(Default)]
pub(crate) struct PrCache {
    entries: HashMap<String, CachedPr>,
    in_flight: HashSet<String>,
}

impl PrCache {
    pub fn lookup_and_queue(
        &mut self,
        session_id: &str,
        folder: &str,
        branch: &str,
        now: i64,
    ) -> (Option<SessionPr>, bool) {
        let key = crate::conversations::canonical_session_id(session_id);
        let (pr, stale) = match self.entries.get(&key) {
            Some(entry) if entry.branch == branch && entry.folder == folder => {
                (entry.pr.clone(), now - entry.fetched_at >= PR_TTL_SECS)
            }
            _ => (None, true),
        };
        if stale && self.in_flight.insert(key) {
            (pr, true)
        } else {
            (pr, false)
        }
    }

    pub fn finish_refresh(
        &mut self,
        session_id: &str,
        folder: &str,
        branch: &str,
        result: Result<Option<SessionPr>, String>,
        now: i64,
    ) -> bool {
        let key = crate::conversations::canonical_session_id(session_id);
        self.in_flight.remove(&key);
        let prev = self.entries.get(&key);
        let new_pr = match result {
            Ok(pr) => pr,
            Err(_) => prev
                .filter(|e| e.branch == branch && e.folder == folder)
                .and_then(|e| e.pr.clone()),
        };
        let changed = prev.is_none_or(|e| e.branch != branch || e.pr != new_pr);
        self.entries.insert(
            key,
            CachedPr {
                branch: branch.to_string(),
                folder: folder.to_string(),
                pr: new_pr,
                fetched_at: now,
            },
        );
        changed
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(crate) fn session_pr_from_json(v: &Value) -> Option<SessionPr> {
    let number = v.get("number").and_then(Value::as_u64)?;
    let title = v.get("title").and_then(Value::as_str)?.to_string();
    let url = v.get("url").and_then(Value::as_str)?.to_string();
    let raw_state = v
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let is_draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let state = match raw_state.as_str() {
        "MERGED" => "merged",
        "CLOSED" => "closed",
        "OPEN" if is_draft => "draft",
        "OPEN" => "open",
        _ => return None,
    }
    .to_string();
    let mergeable = match v.get("mergeable").and_then(Value::as_str) {
        Some("MERGEABLE") => Some(true),
        Some("CONFLICTING") => Some(false),
        _ => None,
    };
    let checks = rollup_checks(v.get("statusCheckRollup"));
    Some(SessionPr {
        number,
        title,
        state,
        checks,
        mergeable,
        url,
    })
}

fn rollup_checks(raw: Option<&Value>) -> String {
    let items: Option<&Vec<Value>> = raw.and_then(|v| {
        v.as_array()
            .or_else(|| v.get("nodes").and_then(Value::as_array))
    });
    let Some(items) = items else {
        return "none".to_string();
    };
    if items.is_empty() {
        return "none".to_string();
    }
    let mut any_pending = false;
    for item in items {
        let conclusion = item
            .get("conclusion")
            .or_else(|| item.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        match conclusion.as_str() {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => return "failing".to_string(),
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            _ if status == "COMPLETED" => {}
            _ => any_pending = true,
        }
    }
    if any_pending {
        "pending".to_string()
    } else {
        "passing".to_string()
    }
}

async fn has_git_remote(dir: &Path) -> bool {
    let Ok(out) = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("remote")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
    else {
        return false;
    };
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

async fn fetch_branch_pr(dir: &Path, branch: &str) -> Result<Option<SessionPr>, String> {
    if !has_git_remote(dir).await {
        return Ok(None);
    }
    let list = gh_json(
        dir,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "5",
            "--json",
            BRANCH_PR_FIELDS,
        ],
    )
    .await?;
    let Some(items) = list.as_array() else {
        return Ok(None);
    };
    let picked = items
        .iter()
        .find(|item| {
            item.get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("OPEN"))
        })
        .or_else(|| items.first());
    Ok(picked.and_then(session_pr_from_json))
}

fn spawn_pr_refresh(session_id: String, folder: String, branch: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        PR_CACHE
            .lock()
            .unwrap()
            .in_flight
            .remove(&crate::conversations::canonical_session_id(&session_id));
        return;
    };
    handle.spawn(async move {
        let Ok(_permit) = PR_SEMAPHORE.acquire().await else {
            return;
        };
        let result = fetch_branch_pr(Path::new(&folder), &branch).await;
        let changed = PR_CACHE.lock().unwrap().finish_refresh(
            &session_id,
            &folder,
            &branch,
            result,
            now_unix(),
        );
        if changed {
            crate::session::emit_device_event(json!({
                "kind": "session_pr",
                "session": session_id,
            }));
        }
    });
}

/// Fill each session's `pr` from the per-session cache without ever invoking `gh`.
/// Missing, stale (>= 5m), or branch-changed entries are queued for background refresh.
pub(crate) fn attach_cached_prs(sessions: &mut [SessionInfo]) {
    let now = now_unix();
    let mut to_refresh: Vec<(String, String, String)> = Vec::new();
    {
        let mut cache = PR_CACHE.lock().unwrap();
        for s in sessions.iter_mut() {
            if !s.git {
                continue;
            }
            let Some(branch) = s.branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            let (pr, needs_refresh) = cache.lookup_and_queue(&s.id, &s.folder, branch, now);
            s.pr = pr;
            if needs_refresh {
                to_refresh.push((s.id.clone(), s.folder.clone(), branch.to_string()));
            }
        }
    }
    for (id, folder, branch) in to_refresh {
        spawn_pr_refresh(id, folder, branch);
    }
}

async fn refresh_session_pr_now(session_id: &str, dir: &Path) {
    let Some(branch) = crate::session_stats::git_branch(dir) else {
        return;
    };
    let result = fetch_branch_pr(dir, &branch).await;
    let folder = dir.display().to_string();
    let changed =
        PR_CACHE
            .lock()
            .unwrap()
            .finish_refresh(session_id, &folder, &branch, result, now_unix());
    if changed {
        crate::session::emit_device_event(json!({
            "kind": "session_pr",
            "session": crate::conversations::canonical_session_id(session_id),
        }));
    }
}

pub(crate) async fn pr_refresh_loop() {
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let to_refresh = tokio::task::spawn_blocking(|| {
            let mut sessions = crate::session::list_device_sessions();
            crate::session_stats::attach_git(&mut sessions);
            let now = now_unix();
            let mut cache = PR_CACHE.lock().unwrap();
            let mut out = Vec::new();
            for s in sessions {
                if !s.git {
                    continue;
                }
                let Some(branch) = s.branch.as_deref().filter(|b| !b.is_empty()) else {
                    continue;
                };
                let (_, needs_refresh) = cache.lookup_and_queue(&s.id, &s.folder, branch, now);
                if needs_refresh {
                    out.push((s.id, s.folder, branch.to_string()));
                }
            }
            out
        })
        .await
        .unwrap_or_default();
        for (id, folder, branch) in to_refresh {
            spawn_pr_refresh(id, folder, branch);
        }
    }
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/github/status", get(status))
        .route("/github/prs", get(list))
        .route("/github/pr", get(detail))
        .route("/github/pr/diff", get(diff))
        .route("/github/pr/review", post(review))
        .route("/github/pr/comment", post(comment))
        .route("/github/pr/merge", post(merge))
        .route("/github/pr/create", post(create))
        .route("/github/pr/reply", post(reply))
        .route("/github/pr/thread", post(resolve_thread))
        .route("/github/commit/diff", get(commit_diff))
        .route("/github/pr/state", post(pr_state))
}

async fn gh(dir: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("gh isn't available: {e}"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    let out = tokio::time::timeout(GH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "gh timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("gh exited with {}", out.status)
        } else {
            err
        })
    }
}

async fn gh_json(dir: &Path, args: &[&str]) -> Result<Value, String> {
    let text = gh(dir, args, None).await?;
    serde_json::from_str(&text).map_err(|e| format!("gh returned unreadable JSON: {e}"))
}

fn session_dir(session: &str) -> Result<PathBuf, Response> {
    resolve_session_dir(session)
}

#[derive(Deserialize)]
struct SessionQuery {
    session: String,
}

/// Whether PR review is available for this session: gh installed and logged
/// in, and the workspace is a GitHub repository.
async fn status(Query(q): Query<SessionQuery>) -> Response {
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(_) => {
            return Json(json!({ "available": false, "reason": "no workspace" })).into_response();
        }
    };
    if gh(&dir, &["--version"], None).await.is_err() {
        return Json(json!({ "available": false, "reason": "gh is not installed" }))
            .into_response();
    }
    if gh(&dir, &["auth", "status"], None).await.is_err() {
        return Json(json!({ "available": false, "reason": "gh is not logged in" }))
            .into_response();
    }
    let repo = match gh_json(
        &dir,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner,url,viewerPermission",
        ],
    )
    .await
    {
        Ok(repo) => repo,
        Err(_) => {
            return Json(json!({ "available": false, "reason": "not a GitHub repository" }))
                .into_response();
        }
    };
    let viewer = gh(&dir, &["api", "user", "--jq", ".login"], None)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let settings = gh_json(&dir, &["api", "repos/{owner}/{repo}"])
        .await
        .unwrap_or(Value::Null);
    let allowed = |key: &str| settings.get(key).and_then(Value::as_bool).unwrap_or(true);
    Json(json!({
        "available": true,
        "repo": repo.get("nameWithOwner"),
        "url": repo.get("url"),
        "permission": repo.get("viewerPermission"),
        "viewer": viewer,
        "merge_methods": {
            "merge": allowed("allow_merge_commit"),
            "squash": allowed("allow_squash_merge"),
            "rebase": allowed("allow_rebase_merge"),
        },
        "auto_merge": settings.get("allow_auto_merge").and_then(Value::as_bool).unwrap_or(false),
        "delete_branch_on_merge": settings.get("delete_branch_on_merge").and_then(Value::as_bool).unwrap_or(false),
    }))
    .into_response()
}

// The per-check detail `gh pr list` asks for costs more than every other field
// together on a busy repo; a row only shows the commit's overall state.
const PR_ROW_FIELDS: &str = "number title author{login} headRefName baseRefName isDraft state reviewDecision additions deletions updatedAt url reviewRequests(first:10){nodes{requestedReviewer{...on User{login}...on Bot{login}...on Team{name}}}} commits(last:1){nodes{commit{statusCheckRollup{state}}}}";
const LIST_PAGE: u32 = 25;
const LIST_PAGE_MAX: u32 = 100;

#[derive(Deserialize)]
struct ListQuery {
    session: String,
    /// open (default), mine, review, closed
    filter: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

async fn list(Query(q): Query<ListQuery>) -> Response {
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let first = format!(
        "first={}",
        q.limit.unwrap_or(LIST_PAGE).clamp(1, LIST_PAGE_MAX)
    );
    let filter = q.filter.as_deref().unwrap_or("open");
    let search = match filter {
        "mine" => Some("author:@me"),
        "review" => Some("review-requested:@me"),
        _ => None,
    };
    let query;
    let search_field;
    let mut args = vec!["api", "graphql", "-F", first.as_str()];
    let pointer = if let Some(clause) = search {
        let repo = match gh(
            &dir,
            &[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ],
            None,
        )
        .await
        {
            Ok(repo) => repo,
            Err(e) => return api_error(StatusCode::BAD_GATEWAY, e),
        };
        search_field = format!(
            "q=repo:{} is:pr is:open {clause} sort:created-desc",
            repo.trim()
        );
        query = format!(
            "query=query($q:String!,$first:Int!,$after:String){{search(query:$q,type:ISSUE,first:$first,after:$after){{pageInfo{{hasNextPage endCursor}}nodes{{...on PullRequest{{{PR_ROW_FIELDS}}}}}}}}}"
        );
        args.extend(["-f", search_field.as_str()]);
        "/data/search"
    } else {
        let states = if filter == "closed" {
            "[CLOSED,MERGED]"
        } else {
            "[OPEN]"
        };
        query = format!(
            "query=query($owner:String!,$repo:String!,$first:Int!,$after:String){{repository(owner:$owner,name:$repo){{pullRequests(states:{states},first:$first,after:$after,orderBy:{{field:CREATED_AT,direction:DESC}}){{pageInfo{{hasNextPage endCursor}}nodes{{{PR_ROW_FIELDS}}}}}}}}}"
        );
        args.extend(["-F", "owner={owner}", "-F", "repo={repo}"]);
        "/data/repository/pullRequests"
    };
    let after = q
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| format!("after={c}"));
    if let Some(after) = &after {
        args.extend(["-f", after.as_str()]);
    }
    args.extend(["-f", query.as_str()]);
    let page = match gh_json(&dir, &args).await {
        Ok(page) => page,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, e),
    };
    let Some(page) = page.pointer(pointer) else {
        return api_error(StatusCode::BAD_GATEWAY, "gh returned no pull requests");
    };
    let prs: Vec<Value> = page
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| n.get("number").is_some())
                .map(pr_row)
                .collect()
        })
        .unwrap_or_default();
    let next = page
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        .then(|| page.pointer("/pageInfo/endCursor").cloned())
        .flatten();
    Json(json!({ "prs": prs, "next_cursor": next })).into_response()
}

/// A GraphQL row in the shape `gh pr view --json` gives, which the app parses.
fn pr_row(node: &Value) -> Value {
    let mut row = node.clone();
    let reviewers: Vec<Value> = node
        .pointer("/reviewRequests/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n.get("requestedReviewer").cloned())
                .collect()
        })
        .unwrap_or_default();
    let checks: Vec<Value> = node
        .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
        .map(|state| json!({ "state": state }))
        .into_iter()
        .collect();
    if let Some(obj) = row.as_object_mut() {
        obj.remove("commits");
        obj.insert("reviewRequests".to_string(), Value::Array(reviewers));
        obj.insert("statusCheckRollup".to_string(), Value::Array(checks));
    }
    row
}

const DETAIL_FIELDS: &str = "number,title,body,author,state,isDraft,headRefName,baseRefName,additions,deletions,changedFiles,files,commits,reviews,comments,reviewDecision,statusCheckRollup,mergeable,mergeStateStatus,url,createdAt,updatedAt,labels,reviewRequests,headRefOid,autoMergeRequest,isCrossRepository";

#[derive(Deserialize)]
struct PrQuery {
    session: String,
    number: u64,
}

async fn detail(Query(q): Query<PrQuery>) -> Response {
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = q.number.to_string();
    let view_args = ["pr", "view", number.as_str(), "--json", DETAIL_FIELDS];
    let number_field = format!("n={number}");
    let query_field = format!("query={THREADS_QUERY}");
    let threads_args = [
        "api",
        "graphql",
        "-F",
        "owner={owner}",
        "-F",
        "repo={repo}",
        "-F",
        number_field.as_str(),
        "-f",
        query_field.as_str(),
    ];
    let (view, threads) = tokio::join!(gh_json(&dir, &view_args), gh_json(&dir, &threads_args));
    let mut pr = match view {
        Ok(pr) => pr,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, e),
    };
    let threads: Vec<Value> = threads
        .ok()
        .and_then(|t| t.pointer("/data/repository/pullRequest/reviewThreads/nodes").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            let comments: Vec<Value> = t
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    json!({
                        "id": c.get("databaseId"),
                        "body": c.get("body"),
                        "author": c.pointer("/author/login"),
                        "created_at": c.get("createdAt"),
                        "review_id": c.pointer("/pullRequestReview/id"),
                    })
                })
                .collect();
            json!({
                "id": t.get("id"),
                "path": t.get("path"),
                "line": t.get("line").filter(|l| !l.is_null()).or(t.get("originalLine")),
                "start_line": t.get("startLine").filter(|l| !l.is_null()).or(t.get("originalStartLine")),
                "side": t.get("diffSide"),
                "resolved": t.get("isResolved"),
                "outdated": t.get("isOutdated"),
                "comments": comments,
            })
        })
        .collect();
    if let Some(obj) = pr.as_object_mut() {
        obj.insert("threads".to_string(), Value::Array(threads));
    }
    Json(pr).into_response()
}

async fn diff(Query(q): Query<PrQuery>) -> Response {
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = q.number.to_string();
    let text = match gh(&dir, &["pr", "diff", &number], None).await {
        Err(e) if e.contains("too_large") || e.contains("HTTP 406") => {
            diff_from_files(&dir, &number).await
        }
        other => other,
    };
    match text {
        Ok(text) => Json(json!({ "diff": text })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

/// GitHub refuses a whole diff past 300 files or 20,000 lines, but still lists
/// each file's patch.
async fn diff_from_files(dir: &Path, number: &str) -> Result<String, String> {
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{number}/files?per_page=100");
    let lines = gh(dir, &["api", "--paginate", "--jq", ".[]", &path], None).await?;
    let mut diff = String::new();
    for line in lines.lines() {
        let file: Value =
            serde_json::from_str(line).map_err(|e| format!("gh returned unreadable JSON: {e}"))?;
        let Some(name) = file.get("filename").and_then(Value::as_str) else {
            continue;
        };
        let old = file
            .get("previous_filename")
            .and_then(Value::as_str)
            .unwrap_or(name);
        diff.push_str(&format!(
            "diff --git a/{old} b/{name}\n--- a/{old}\n+++ b/{name}\n"
        ));
        if let Some(patch) = file.get("patch").and_then(Value::as_str) {
            diff.push_str(patch);
            diff.push('\n');
        }
    }
    Ok(diff)
}

#[derive(Deserialize)]
struct InlineComment {
    path: String,
    line: u64,
    side: Option<String>,
    start_line: Option<u64>,
    start_side: Option<String>,
    body: String,
}

#[derive(Deserialize)]
struct ReviewReq {
    session: String,
    number: u64,
    /// APPROVE, REQUEST_CHANGES or COMMENT
    event: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comments: Vec<InlineComment>,
}

async fn review(Json(req): Json<ReviewReq>) -> Response {
    let event = req.event.to_ascii_uppercase();
    if !matches!(event.as_str(), "APPROVE" | "REQUEST_CHANGES" | "COMMENT") {
        return api_error(
            StatusCode::BAD_REQUEST,
            "event must be APPROVE, REQUEST_CHANGES or COMMENT",
        );
    }
    if event != "APPROVE" && req.body.trim().is_empty() && req.comments.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "a comment or request for changes needs a summary or an inline comment",
        );
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let comments: Vec<Value> = req
        .comments
        .iter()
        .filter(|c| !c.body.trim().is_empty())
        .map(|c| {
            let side = c.side.clone().unwrap_or_else(|| "RIGHT".to_string());
            let mut comment = json!({
                "path": c.path,
                "line": c.line,
                "side": side,
                "body": c.body,
            });
            if let Some(start) = c.start_line.filter(|start| *start < c.line) {
                comment["start_line"] = json!(start);
                comment["start_side"] = json!(c.start_side.clone().unwrap_or(side));
            }
            comment
        })
        .collect();
    let payload = json!({ "event": event, "body": req.body, "comments": comments }).to_string();
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews", req.number);
    match gh(
        &dir,
        &["api", "-X", "POST", &path, "--input", "-"],
        Some(&payload),
    )
    .await
    {
        Ok(text) => {
            let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            Json(json!({ "submitted": true, "url": reply.get("html_url") })).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CommentReq {
    session: String,
    number: u64,
    body: String,
}

async fn comment(Json(req): Json<CommentReq>) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "the comment is empty");
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    match gh(
        &dir,
        &["pr", "comment", &req.number.to_string(), "--body-file", "-"],
        Some(&req.body),
    )
    .await
    {
        Ok(text) => Json(json!({ "posted": true, "url": text.trim() })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CreateReq {
    session: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
}

async fn create(Json(req): Json<CreateReq>) -> Response {
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let push = tokio::process::Command::new("git")
        .args(["push", "-u", "origin", "HEAD"])
        .current_dir(&dir)
        .output()
        .await;
    match push {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            );
        }
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let mut args: Vec<&str> = vec!["pr", "create"];
    match title {
        Some(t) => {
            args.extend(["--title", t, "--body-file", "-"]);
        }
        None => args.push("--fill"),
    }
    if req.draft {
        args.push("--draft");
    }
    let body = req.body.clone().unwrap_or_default();
    match gh(&dir, &args, title.map(|_| body.as_str())).await {
        Ok(text) => {
            refresh_session_pr_now(&req.session, &dir).await;
            Json(json!({ "created": true, "url": text.trim().lines().last().unwrap_or("") }))
                .into_response()
        }
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct MergeReq {
    session: String,
    number: u64,
    /// merge (default), squash or rebase
    method: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    /// The head commit the user reviewed; the merge is refused if it moved.
    #[serde(default)]
    head: Option<String>,
    /// Merge once requirements are met instead of now.
    #[serde(default)]
    auto: bool,
    /// Turn off a pending auto-merge.
    #[serde(default)]
    disable_auto: bool,
    /// Merge without waiting for requirements (admins only).
    #[serde(default)]
    admin: bool,
    /// Delete the remote head branch once merged.
    #[serde(default)]
    delete_branch: bool,
    #[serde(default)]
    branch: Option<String>,
}

async fn merge(Json(req): Json<MergeReq>) -> Response {
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = req.number.to_string();
    if req.disable_auto {
        return match gh(&dir, &["pr", "merge", &number, "--disable-auto"], None).await {
            Ok(_) => {
                refresh_session_pr_now(&req.session, &dir).await;
                Json(json!({ "auto": false })).into_response()
            }
            Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
        };
    }
    let flag = match req.method.as_deref().unwrap_or("merge") {
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => "--merge",
    };
    let mut args: Vec<String> = vec!["pr".into(), "merge".into(), number.clone(), flag.into()];
    if let Some(subject) = req
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.extend(["--subject".into(), subject.to_string()]);
    }
    if let Some(body) = req.body.as_deref() {
        args.extend(["--body".into(), body.to_string()]);
    }
    if let Some(head) = req
        .head
        .as_deref()
        .filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()) && !h.is_empty())
    {
        args.extend(["--match-head-commit".into(), head.to_string()]);
    }
    if req.auto {
        args.push("--auto".into());
    } else if req.admin {
        args.push("--admin".into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = gh(&dir, &refs, None).await {
        return api_error(StatusCode::BAD_GATEWAY, e);
    }
    if req.auto {
        refresh_session_pr_now(&req.session, &dir).await;
        return Json(json!({ "auto": true })).into_response();
    }
    let mut deleted = false;
    if req.delete_branch
        && let Some(branch) = req
            .branch
            .as_deref()
            .filter(|b| !b.is_empty() && !b.contains(".."))
    {
        let path = format!("repos/{{owner}}/{{repo}}/git/refs/heads/{branch}");
        deleted = gh(&dir, &["api", "-X", "DELETE", &path], None)
            .await
            .is_ok();
    }
    refresh_session_pr_now(&req.session, &dir).await;
    Json(json!({ "merged": true, "branch_deleted": deleted })).into_response()
}

#[derive(Deserialize)]
struct StateReq {
    session: String,
    number: u64,
    /// ready, draft, close, reopen, update or update_rebase
    action: String,
}

async fn pr_state(Json(req): Json<StateReq>) -> Response {
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = req.number.to_string();
    let args: Vec<&str> = match req.action.as_str() {
        "ready" => vec!["pr", "ready", &number],
        "draft" => vec!["pr", "ready", &number, "--undo"],
        "close" => vec!["pr", "close", &number],
        "reopen" => vec!["pr", "reopen", &number],
        "update" => vec!["pr", "update-branch", &number],
        "update_rebase" => vec!["pr", "update-branch", &number, "--rebase"],
        _ => return api_error(StatusCode::BAD_REQUEST, "unknown action"),
    };
    match gh(&dir, &args, None).await {
        Ok(_) => {
            refresh_session_pr_now(&req.session, &dir).await;
            Json(json!({ "done": true })).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

const THREADS_QUERY: &str = "query($owner: String!, $repo: String!, $n: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $n) { reviewThreads(first: 100) { nodes { id isResolved isOutdated path line originalLine startLine originalStartLine diffSide comments(first: 50) { nodes { databaseId body createdAt author { login } pullRequestReview { id } } } } } } } }";

#[derive(Deserialize)]
struct ReplyReq {
    session: String,
    number: u64,
    comment_id: u64,
    body: String,
}

async fn reply(Json(req): Json<ReplyReq>) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "the reply is empty");
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let path = format!(
        "repos/{{owner}}/{{repo}}/pulls/{}/comments/{}/replies",
        req.number, req.comment_id
    );
    let payload = json!({ "body": req.body }).to_string();
    match gh(
        &dir,
        &["api", "-X", "POST", &path, "--input", "-"],
        Some(&payload),
    )
    .await
    {
        Ok(_) => Json(json!({ "posted": true })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct ThreadReq {
    session: String,
    thread_id: String,
    resolved: bool,
}

async fn resolve_thread(Json(req): Json<ThreadReq>) -> Response {
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let mutation = if req.resolved {
        "mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"
    } else {
        "mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"
    };
    let id_field = format!("id={}", req.thread_id);
    match gh(
        &dir,
        &[
            "api",
            "graphql",
            "-F",
            &id_field,
            "-f",
            &format!("query={mutation}"),
        ],
        None,
    )
    .await
    {
        Ok(_) => Json(json!({ "resolved": req.resolved })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CommitQuery {
    session: String,
    sha: String,
}

async fn commit_diff(Query(q): Query<CommitQuery>) -> Response {
    if q.sha.is_empty() || !q.sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return api_error(StatusCode::BAD_REQUEST, "bad commit id");
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let path = format!("repos/{{owner}}/{{repo}}/commits/{}", q.sha);
    match gh(
        &dir,
        &["api", "-H", "Accept: application/vnd.github.diff", &path],
        None,
    )
    .await
    {
        Ok(text) => Json(json!({ "diff": text })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_pr_from_json_maps_states_checks_and_mergeable() {
        let open = session_pr_from_json(&json!({
            "number": 280,
            "title": "Add session rows",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "url": "https://github.com/org/repo/pull/280",
            "statusCheckRollup": [{"conclusion": "SUCCESS", "status": "COMPLETED"}]
        }))
        .unwrap();
        assert_eq!(open.number, 280);
        assert_eq!(open.state, "open");
        assert_eq!(open.checks, "passing");
        assert_eq!(open.mergeable, Some(true));

        let draft = session_pr_from_json(&json!({
            "number": 281,
            "title": "WIP",
            "state": "OPEN",
            "isDraft": true,
            "mergeable": "CONFLICTING",
            "url": "https://github.com/org/repo/pull/281",
            "statusCheckRollup": [{"status": "IN_PROGRESS"}]
        }))
        .unwrap();
        assert_eq!(draft.state, "draft");
        assert_eq!(draft.checks, "pending");
        assert_eq!(draft.mergeable, Some(false));

        let merged = session_pr_from_json(&json!({
            "number": 282,
            "title": "Done",
            "state": "MERGED",
            "isDraft": false,
            "mergeable": "UNKNOWN",
            "url": "https://github.com/org/repo/pull/282",
            "statusCheckRollup": [
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "FAILURE", "status": "COMPLETED"}
            ]
        }))
        .unwrap();
        assert_eq!(merged.state, "merged");
        assert_eq!(merged.checks, "failing");
        assert_eq!(merged.mergeable, None);

        let closed = session_pr_from_json(&json!({
            "number": 283,
            "title": "Closed",
            "state": "CLOSED",
            "url": "https://github.com/org/repo/pull/283",
            "statusCheckRollup": []
        }))
        .unwrap();
        assert_eq!(closed.state, "closed");
        assert_eq!(closed.checks, "none");
    }

    #[test]
    fn pr_cache_dedups_in_flight_expires_at_5m_and_resets_on_branch_change() {
        let mut cache = PrCache::default();
        // Initial miss queues refresh and returns None.
        assert_eq!(
            cache.lookup_and_queue("s1", "/repo", "feat/a", 1000),
            (None, true)
        );
        // Second call while in-flight does not double-queue.
        assert_eq!(
            cache.lookup_and_queue("s1", "/repo", "feat/a", 1001),
            (None, false)
        );

        let pr_a = SessionPr {
            number: 10,
            title: "A".into(),
            state: "open".into(),
            checks: "passing".into(),
            mergeable: Some(true),
            url: "https://github.com/o/r/pull/10".into(),
        };
        assert!(cache.finish_refresh("s1", "/repo", "feat/a", Ok(Some(pr_a.clone())), 1002));

        // Fresh cache hit within 5 minutes does not queue.
        assert_eq!(
            cache.lookup_and_queue("s1", "/repo", "feat/a", 1299),
            (Some(pr_a.clone()), false)
        );
        // After 5 minutes (300s), returns stale cached PR while queueing refresh.
        assert_eq!(
            cache.lookup_and_queue("s1", "/repo", "feat/a", 1302),
            (Some(pr_a), true)
        );
        assert!(cache.finish_refresh("s1", "/repo", "feat/a", Ok(None), 1303));
        // Branch change drops old PR and queues immediately.
        assert_eq!(
            cache.lookup_and_queue("s1", "/repo", "feat/b", 1304),
            (None, true)
        );
    }
}
