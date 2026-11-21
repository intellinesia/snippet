use super::api_error;
use super::*;

#[derive(Serialize)]
struct FsEntry {
    name: String,
    path: String,
    is_dir: bool,
    git: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_status: Option<&'static str>,
}

#[derive(Serialize)]
struct FsListing {
    path: String,
    parent: Option<String>,
    entries: Vec<FsEntry>,
}

#[derive(Deserialize)]
pub(super) struct FsQuery {
    path: Option<String>,
}

fn status_rank(s: &str) -> u8 {
    match s {
        "modified" => 5,
        "added" => 4,
        "renamed" => 3,
        "deleted" => 2,
        "untracked" => 1,
        _ => 0,
    }
}

fn classify_porcelain_status(x: char, y: char) -> Option<&'static str> {
    match (x, y) {
        ('?', '?') => Some("untracked"),
        ('!', '!') => None,
        ('D', _) | (_, 'D') => Some("deleted"),
        ('R', _) | (_, 'R') => Some("renamed"),
        ('A', _) => Some("added"),
        ('M', _) | (_, 'M') | ('T', _) | (_, 'T') | ('U', _) | (_, 'U') | ('C', _) => {
            Some("modified")
        }
        _ => None,
    }
}

fn parse_git_dir_statuses(
    porcelain_z: &str,
    dir_prefix: Option<&str>,
) -> std::collections::HashMap<String, &'static str> {
    let mut map: std::collections::HashMap<String, &'static str> = std::collections::HashMap::new();
    let parts: Vec<&str> = porcelain_z.split('\0').collect();
    let mut i = 0;
    while i < parts.len() {
        let tok = parts[i];
        i += 1;
        let bytes = tok.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            continue;
        }
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        let raw_path = &tok[3..];
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            i += 1; // skip the original path token in -z rename/copy records
        }
        let Some(status) = classify_porcelain_status(x, y) else {
            continue;
        };
        let rel = match dir_prefix {
            Some(prefix) => {
                let Some(rest) = raw_path
                    .strip_prefix(prefix)
                    .and_then(|r| r.strip_prefix('/'))
                else {
                    continue;
                };
                rest
            }
            None => raw_path,
        };
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let entry_name = rel.split('/').next().unwrap_or(rel);
        if entry_name.is_empty() || entry_name.starts_with('.') {
            continue;
        }
        map.entry(entry_name.to_string())
            .and_modify(|cur| {
                if status_rank(status) > status_rank(cur) {
                    *cur = status;
                }
            })
            .or_insert(status);
    }
    map
}

fn git_dir_statuses(dir: &Path) -> std::collections::HashMap<String, &'static str> {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let Some(repo_root) = canon.ancestors().find(|a| a.join(".git").exists()) else {
        return std::collections::HashMap::new();
    };
    let prefix = canon
        .strip_prefix(repo_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty());
    let Ok(out) = std::process::Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(&canon)
        .args([
            "-c",
            "core.quotepath=off",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
            "--",
            ".",
        ])
        .output()
    else {
        return std::collections::HashMap::new();
    };
    if !out.status.success() {
        return std::collections::HashMap::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_git_dir_statuses(&stdout, prefix.as_deref())
}

fn list_dir_entries(path: &Path) -> FsListing {
    let git_statuses = git_dir_statuses(path);
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // hide dotfiles
            }
            let p = e.path();
            let meta = std::fs::metadata(&p).ok();
            let is_dir = meta
                .as_ref()
                .map(|m| m.is_dir())
                .unwrap_or_else(|| p.is_dir());
            let size = (!is_dir).then(|| meta.as_ref().map(|m| m.len())).flatten();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let git_status = git_statuses.get(&name).copied();
            entries.push(FsEntry {
                git: is_dir && p.join(".git").exists(),
                name,
                path: p.display().to_string(),
                is_dir,
                size,
                modified,
                git_status,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    FsListing {
        parent: path.parent().map(|p| p.display().to_string()),
        path: path.display().to_string(),
        entries,
    }
}

// GET /fs?path= — one directory level (lazy folder tree). Defaults to $HOME.
pub(super) async fn browse_fs(Query(q): Query<FsQuery>) -> Response {
    let path = q
        .path
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    match tokio::task::spawn_blocking(move || list_dir_entries(&path)).await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Serialize)]
struct FsFiles {
    root: String,
    files: Vec<String>,
    truncated: bool,
}

const FS_FILES_CAP: usize = 20_000;

// GET /fs/files?path= — every file under a workspace, honouring .gitignore.
pub(super) async fn list_fs_files(Query(q): Query<FsQuery>) -> Response {
    let Some(root) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    };
    if !root.is_dir() {
        return api_error(StatusCode::NOT_FOUND, "not a directory");
    }
    let walk_root = root.clone();
    let listed = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut truncated = false;
        let walker = ignore::WalkBuilder::new(&walk_root)
            .hidden(true)
            .git_ignore(true)
            .git_exclude(true)
            .require_git(false)
            .max_depth(Some(24))
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if files.len() >= FS_FILES_CAP {
                truncated = true;
                break;
            }
            if let Ok(rel) = entry.path().strip_prefix(&walk_root) {
                files.push(rel.to_string_lossy().to_string());
            }
        }
        files.sort();
        (files, truncated)
    })
    .await;
    match listed {
        Ok((files, truncated)) => Json(FsFiles {
            root: root.display().to_string(),
            files,
            truncated,
        })
        .into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Serialize)]
struct FileContent {
    path: String,
    content: String,
    size: u64,
    truncated: bool,
    binary: bool,
    /// Fingerprint of the full on-disk bytes — the editor sends it back on save
    /// for optimistic-concurrency conflict detection.
    hash: String,
}

/// Fast non-cryptographic fingerprint of file bytes (same scheme used elsewhere).
fn hash_bytes(b: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Deserialize)]
pub(super) struct FsWriteReq {
    path: String,
    content: String,
    /// If set, the write is refused when the file on disk no longer matches it
    /// (someone — the agent or another editor — changed it since it was opened).
    #[serde(default)]
    prev_hash: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct FsMkdirReq {
    /// Absolute path of the new directory to create.
    path: String,
}

#[derive(Deserialize)]
pub(super) struct FsDeleteReq {
    /// Absolute path of the file or directory to delete (dirs are removed recursively).
    path: String,
}

// POST /fs/write {path, content, prev_hash?} — atomic write (temp + rename) with
// optimistic-concurrency conflict detection. Token-gated; UTF-8 text only.
pub(super) async fn write_fs_file(Json(req): Json<FsWriteReq>) -> Response {
    if req.path.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    }
    let path = PathBuf::from(&req.path);
    if let Some(prev) = req.prev_hash.as_deref() {
        match std::fs::read(&path) {
            Ok(cur) if hash_bytes(&cur) != prev => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "ok": false,
                        "conflict": true,
                        "error": "file changed on disk since you opened it — reload before saving",
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"ok": false, "conflict": true, "error": "file no longer exists"})),
                )
                    .into_response();
            }
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
    let parent = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&parent) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create dir: {e}"),
        );
    }
    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{fname}.snippet.tmp"));
    let bytes = req.content.into_bytes();
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write temp: {e}"),
        );
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}"));
    }
    Json(serde_json::json!({"ok": true, "hash": hash_bytes(&bytes), "size": bytes.len()}))
        .into_response()
}

// POST /fs/mkdir {path} — create a new directory (token-gated).
pub(super) async fn make_fs_dir(Json(req): Json<FsMkdirReq>) -> Response {
    if req.path.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    }
    let path = PathBuf::from(&req.path);
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "error": "a file or folder with that name already exists"})),
        )
            .into_response();
    }
    if let Err(e) = std::fs::create_dir_all(&path) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create dir: {e}"),
        );
    }
    Json(serde_json::json!({"ok": true, "path": path.to_string_lossy()})).into_response()
}

// POST /fs/delete {path} — remove a file or directory (recursive). Token-gated.
pub(super) async fn delete_fs_path(Json(req): Json<FsDeleteReq>) -> Response {
    if req.path.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    }
    let path = PathBuf::from(&req.path);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Json(serde_json::json!({"ok": true, "already_gone": true})).into_response();
        }
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let res = if meta.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match res {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")),
    }
}

#[derive(Deserialize)]
pub(super) struct FsRenameReq {
    from: String,
    to: String,
}

// POST /fs/rename {from, to}: rename or move a file or directory. Never overwrites.
pub(super) async fn rename_fs_path(Json(req): Json<FsRenameReq>) -> Response {
    if req.from.trim().is_empty() || req.to.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "from and to required");
    }
    let (from, to) = (PathBuf::from(&req.from), PathBuf::from(&req.to));
    match std::fs::symlink_metadata(&from) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return api_error(StatusCode::NOT_FOUND, "from not found");
        }
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    if std::fs::symlink_metadata(&to).is_ok() {
        return api_error(StatusCode::CONFLICT, "to already exists");
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => Json(serde_json::json!({"ok": true, "path": req.to})).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "rename: from and to are on different filesystems",
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")),
    }
}

// GET /fs/download?path= — serve a file's raw bytes (any type). Sets a real
// content-type from the extension and honors HTTP Range requests (206 Partial
// Content), so the app can stream/seek images, video, and audio — not just
// download whole files. Token-gated.
pub(super) async fn download_fs_file(
    Query(q): Query<FsQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return api_error(StatusCode::NOT_FOUND, "not found");
        }
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if meta.is_dir() {
        return api_error(StatusCode::BAD_REQUEST, "path is a directory");
    }
    let total = meta.len();
    let ctype = content_type_for(&path);

    // Range request → 206, STREAMING the requested slice (the whole range, not a
    // capped chunk — capping made ExoPlayer stall). The body streams off disk, so
    // a large range never buffers into memory.
    if let Some(raw) = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        let Some((start, end)) = parse_byte_range(raw, total) else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes */{total}"),
                )],
            )
                .into_response();
        };
        let len = end - start + 1;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        let stream = tokio_util::io::ReaderStream::new(file.take(len));
        let body = axum::body::Body::from_stream(stream);
        return match axum::response::Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(axum::http::header::CONTENT_TYPE, ctype)
            .header(axum::http::header::ACCEPT_RANGES, "bytes")
            .header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}"),
            )
            .header(axum::http::header::CONTENT_LENGTH, len.to_string())
            .body(body)
        {
            Ok(resp) => resp,
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    }

    // No Range → stream the whole file. No size cap: the body streams off
    // disk via ReaderStream so memory stays bounded regardless of file size.
    {
        use tokio::io::AsyncSeekExt;
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        if let Err(e) = file.seek(std::io::SeekFrom::Start(0)).await {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        let stream = tokio_util::io::ReaderStream::new(file);
        let body = axum::body::Body::from_stream(stream);
        match axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, ctype)
            .header(axum::http::header::CONTENT_LENGTH, total.to_string())
            .header(axum::http::header::ACCEPT_RANGES, "bytes")
            .body(body)
        {
            Ok(resp) => resp,
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

/// Content-type from a file extension — enough for the app to render images,
/// play video/audio, and view PDFs. Unknown types stay a generic download.
fn content_type_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("heic" | "heif") => "image/heic",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a" | "aac") => "audio/mp4",
        Some("ogg" | "oga") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Parse a single-range `Range: bytes=…` header against a known total size.
/// Supports `start-end`, `start-`, and `-suffix`. Returns an inclusive (start,
/// end) clamped to the file, or None if unsatisfiable.
fn parse_byte_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = raw.trim().strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let (start, end) = if s.is_empty() {
        let suffix: u64 = e.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start: u64 = s.trim().parse().ok()?;
        let end: u64 = if e.trim().is_empty() {
            total - 1
        } else {
            e.trim().parse().ok()?
        };
        (start, end.min(total - 1))
    };
    (start <= end && start < total).then_some((start, end))
}

const MAX_FS_FILE_BYTES: usize = 512 * 1024;

// GET /fs/file?path= — read one text file's contents (for the in-app file viewer).
pub(super) async fn read_fs_file(Query(q): Query<FsQuery>) -> Response {
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return api_error(StatusCode::BAD_REQUEST, "path required");
    };
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => {
            let size = m.len();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
                }
            };
            let truncated = bytes.len() > MAX_FS_FILE_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_FS_FILE_BYTES)];
            let binary = slice.contains(&0) || std::str::from_utf8(slice).is_err();
            let content = if binary {
                String::new()
            } else {
                String::from_utf8_lossy(slice).to_string()
            };
            Json(FileContent {
                path: path.display().to_string(),
                content,
                size,
                truncated,
                binary,
                hash: hash_bytes(&bytes),
            })
            .into_response()
        }
        Ok(_) => api_error(StatusCode::BAD_REQUEST, "not a file"),
        Err(e) => api_error(StatusCode::NOT_FOUND, e.to_string()),
    }
}

#[derive(Deserialize)]
pub(super) struct UploadReq {
    data_base64: String,
    #[serde(default)]
    name: Option<String>,
    /// When set, save into this directory under the original [name] (instead of
    /// a temp dir with a generated name) — used by the file-explorer upload.
    #[serde(default)]
    dir: Option<String>,
}

// POST /fs/upload {data_base64, name?} — save an uploaded file (e.g. an image the
// user posts from the app) to a temp dir and return its absolute path, which the
// agent can then view with `view_image` (or read).
pub(super) async fn upload_fs_file(Json(req): Json<UploadReq>) -> Response {
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data_base64.trim()) {
        Ok(b) => b,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("bad base64: {e}")),
    };
    if bytes.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "empty upload");
    }
    const MAX_UPLOAD_FILE_BYTES: usize = 1024 * 1024 * 1024;
    if bytes.len() > MAX_UPLOAD_FILE_BYTES {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "upload exceeds {} MB limit",
                MAX_UPLOAD_FILE_BYTES / (1024 * 1024)
            ),
        );
    }
    // Targeted upload into a directory under the original filename, or (default)
    // a temp dir with a generated name (for chat attachments).
    let path = if let Some(dir) = req.dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let name = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).file_name())
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return api_error(
                StatusCode::BAD_REQUEST,
                "name required when uploading to a directory",
            );
        };
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok": false, "error": "a file with that name already exists"})),
            )
                .into_response();
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        p
    } else {
        let ext = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).extension())
            .and_then(|e| e.to_str())
            .filter(|e| e.len() <= 5)
            .unwrap_or("png");
        let dir = std::env::temp_dir().join("snippet-uploads");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        dir.join(format!("{}.{ext}", uuid::Uuid::new_v4().simple()))
    };
    if let Err(e) = std::fs::write(&path, &bytes) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(serde_json::json!({"path": path.display().to_string(), "size": bytes.len()}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn rename_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let req = FsRenameReq {
            from: a.display().to_string(),
            to: b.display().to_string(),
        };
        let res = rename_fs_path(Json(req)).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
        let (d, moved) = (dir.path().join("d"), dir.path().join("e"));
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("x"), "x").unwrap();
        for (from, to) in [(&a, dir.path().join("c.txt")), (&d, moved.clone())] {
            let req = FsRenameReq {
                from: from.display().to_string(),
                to: to.display().to_string(),
            };
            assert_eq!(rename_fs_path(Json(req)).await.status(), StatusCode::OK);
            assert!(to.exists() && !from.exists());
        }
        assert!(moved.join("x").exists());
    }

    #[test]
    fn parse_git_dir_statuses_handles_subtrees_and_renames() {
        let z = " M src/main.rs\0A  src/new.rs\0R  src/renamed.rs\0src/old.rs\0?? src/sub/extra.rs\0 D src/gone/dead.rs\0?? top.txt\0";
        let root_map = parse_git_dir_statuses(z, None);
        assert_eq!(root_map.get("src"), Some(&"modified"));
        assert_eq!(root_map.get("top.txt"), Some(&"untracked"));

        let src_map = parse_git_dir_statuses(z, Some("src"));
        assert_eq!(src_map.get("main.rs"), Some(&"modified"));
        assert_eq!(src_map.get("new.rs"), Some(&"added"));
        assert_eq!(src_map.get("renamed.rs"), Some(&"renamed"));
        assert_eq!(src_map.get("old.rs"), None);
        assert_eq!(src_map.get("sub"), Some(&"untracked"));
        assert_eq!(src_map.get("gone"), Some(&"deleted"));
        assert_eq!(src_map.get("top.txt"), None);
    }

    #[test]
    fn list_dir_entries_includes_size_modified_and_git_status() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);

        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("clean.txt"), "hello").unwrap();
        std::fs::write(root.join("mod.txt"), "v1").unwrap();
        std::fs::write(root.join("sub/nested.txt"), "n1").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "init"]);

        std::fs::write(root.join("mod.txt"), "v2 changed").unwrap();
        std::fs::write(root.join("added.txt"), "staged").unwrap();
        run_git(root, &["add", "added.txt"]);
        std::fs::write(root.join("untracked.txt"), "new").unwrap();
        std::fs::write(root.join("sub/nested.txt"), "n2").unwrap();

        let top = list_dir_entries(root);
        let by_name: std::collections::HashMap<_, _> =
            top.entries.iter().map(|e| (e.name.as_str(), e)).collect();

        assert_eq!(by_name["sub"].is_dir, true);
        assert_eq!(by_name["sub"].size, None);
        assert!(by_name["sub"].modified.is_some());
        assert_eq!(by_name["sub"].git_status, Some("modified"));

        assert_eq!(by_name["clean.txt"].size, Some(5));
        assert!(by_name["clean.txt"].modified.is_some());
        assert_eq!(by_name["clean.txt"].git_status, None);

        assert_eq!(by_name["mod.txt"].size, Some(10));
        assert_eq!(by_name["mod.txt"].git_status, Some("modified"));
        assert_eq!(by_name["added.txt"].git_status, Some("added"));
        assert_eq!(by_name["untracked.txt"].git_status, Some("untracked"));

        let sub = list_dir_entries(&root.join("sub"));
        assert_eq!(sub.entries.len(), 1);
        assert_eq!(sub.entries[0].name, "nested.txt");
        assert_eq!(sub.entries[0].git_status, Some("modified"));
    }

    #[test]
    fn list_dir_entries_non_git_dir_has_no_git_status() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plain.txt"), "abc").unwrap();
        let listing = list_dir_entries(tmp.path());
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].size, Some(3));
        assert!(listing.entries[0].modified.is_some());
        assert_eq!(listing.entries[0].git_status, None);
    }
}
