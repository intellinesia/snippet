use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};

pub const SESSION_ENV: &str = "SNIPPET_DEVIN_MCP_URL";
pub const MCP_SERVER_NAME: &str = "snippet";
pub const MCP_TOOL_PREFIX: &str = "mcp__snippet__";

const ALLOWED_META_TOOLS: &[&str] = &[
    "mcp_list_tools",
    "mcp_list_servers",
    "mcp_read_resource",
    "list_resources",
    "read_resource",
];

pub const DISABLED_TOOLS: &[&str] = &[
    "exec",
    "read",
    "edit",
    "write",
    "grep",
    "glob",
    "todo_write",
    "ask_user_question",
    "browser_preview",
    "close_browser_preview",
    "find_file_by_name",
    "get_output",
    "kill_shell",
    "notebook_edit",
    "notebook_read",
    "request_scope",
    "web_search",
    "webfetch",
    "write_to_process",
];

pub fn binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNIPPET_DEVIN_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("devin"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }
    let home = real_home()?;
    [
        home.join(".local/bin/devin"),
        home.join(".local/share/devin/cli/_versions/current/bin/devin"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

pub fn real_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn real_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        return Some(xdg.join("devin"));
    }
    Some(real_home()?.join(".local/share/devin"))
}

pub fn run_hook() -> Result<(), String> {
    let mut raw = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw);
    println!("{}", decision(&raw, std::env::var(SESSION_ENV).is_ok()));
    Ok(())
}

pub fn decision(raw: &str, in_snippet: bool) -> Value {
    if !in_snippet {
        return json!({"decision": "approve"});
    }
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.starts_with(MCP_TOOL_PREFIX) || ALLOWED_META_TOOLS.contains(&name) {
        return json!({"decision": "approve"});
    }
    if name == "mcp_call_tool" {
        let server = payload
            .pointer("/tool_input/server_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if server == MCP_SERVER_NAME {
            return json!({"decision": "approve"});
        }
        return json!({
            "decision": "block",
            "reason": "Only the snippet MCP server (`server_name: \"snippet\"`) is available in a snippet session.",
        });
    }
    if name.starts_with("mcp__") {
        return json!({
            "decision": "block",
            "reason": "Only the snippet MCP server is available in a snippet session.",
        });
    }
    let instead = match name {
        "read" | "notebook_read" | "grep" | "glob" | "find_file_by_name" | "get_output" => {
            "the snippet `bash` tool (`mcp__snippet__bash`: cat, sed -n, ls, rg)"
        }
        "exec" | "shell_command" | "write_to_process" | "kill_shell" => {
            "the snippet `bash` tool (`mcp__snippet__bash`)"
        }
        "write" | "edit" | "apply_patch" | "notebook_edit" => {
            "the snippet `change_files` tool (`mcp__snippet__change_files`)"
        }
        "webfetch" | "web_search" => {
            "the snippet `web_search` or `web_read` tool (`mcp__snippet__web_search`, `mcp__snippet__web_read`)"
        }
        "ask_user_question" | "request_permission" => {
            "the snippet `ask_user` tool (`mcp__snippet__ask_user`)"
        }
        "run_subagent" | "read_subagent" => {
            "the snippet `delegate_task` tool (`mcp__snippet__delegate_task`)"
        }
        "todo_write" | "exit_plan_mode" => {
            "the snippet `update_plan` tool (`mcp__snippet__update_plan`)"
        }
        "skill" => "the snippet `search_skills` and `skill` tools",
        _ => "the matching `mcp__snippet__<tool>` tool",
    };
    json!({
        "decision": "block",
        "reason": format!(
            "In this snippet session `{name}` is handled by snippet: call {instead} instead."
        ),
    })
}

pub struct PreparedEnv {
    pub home: PathBuf,
    pub xdg_config_home: PathBuf,
    pub xdg_data_home: PathBuf,
    pub xdg_cache_home: PathBuf,
    pub config_path: PathBuf,
    pub api_key: Option<String>,
}

pub fn real_org_id() -> Option<String> {
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| real_home().map(|h| h.join(".config")))?
        .join("devin/config.json");
    let raw = std::fs::read_to_string(cfg).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    val.pointer("/devin/org_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn account_token(id: &str) -> Option<String> {
    if id == crate::accounts::MAIN {
        return None;
    }
    let raw = crate::accounts::read_credential("devin", id)?;
    parse_credential_token(&raw)
}

pub fn parse_credential_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for key in ["token", "windsurf_api_key", "api_key"] {
            if let Some(tok) = value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                return Some(tok.to_string());
            }
        }
    }
    if let Ok(table) = trimmed.parse::<toml::Table>() {
        for key in ["windsurf_api_key", "token", "api_key"] {
            if let Some(tok) = table
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                return Some(tok.to_string());
            }
        }
    }
    if !trimmed.contains('\n') && !trimmed.contains(' ') {
        return Some(trimmed.to_string());
    }
    None
}

pub fn build_config_json(exe_path: &Path, org_id: Option<&str>) -> Value {
    let devin_block = match org_id {
        Some(id) if !id.is_empty() => json!({ "org_id": id }),
        _ => json!({}),
    };
    json!({
        "version": 1,
        "devin": devin_block,
        "shell": { "setup_complete": true },
        "respect_workspace_trust": false,
        "permission_mode": "dangerous",
        "subagents_enabled": false,
        "auto_update": false,
        "show_hints": false,
        "notify": "never",
        "attribution": false,
        "disabled_tools": DISABLED_TOOLS,
        "read_config_from": {
            "agents_standard": false,
            "cursor": false,
            "windsurf": false,
            "claude": false,
            "copilot": false,
            "codex": false,
            "kiro": false,
            "opencode": false,
            "zed": false
        },
        "hooks": {
            "PreToolUse": [{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": format!("'{}' devin-hook", exe_path.display()),
                    "timeout": 10,
                    "fail_closed": true
                }]
            }]
        }
    })
}

pub fn prepare_home(
    key: &str,
    mcp_url: &str,
    prompt: &str,
    account: Option<&str>,
) -> Result<PreparedEnv, String> {
    prepare_home_in(
        &crate::config::snippet_home(),
        real_data_dir().as_deref(),
        key,
        mcp_url,
        prompt,
        account,
    )
}

fn prepare_home_in(
    snippet_home: &Path,
    real_data: Option<&Path>,
    key: &str,
    mcp_url: &str,
    prompt: &str,
    account: Option<&str>,
) -> Result<PreparedEnv, String> {
    let slot: String = account
        .unwrap_or(crate::accounts::MAIN)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let acct_root = snippet_home.join("devin/accounts").join(&slot);
    let home = acct_root.join("home");
    let xdg_data_home = acct_root.join("data");
    let xdg_cache_home = acct_root.join("cache");
    let data_dir = xdg_data_home.join("devin");

    let sess_root = snippet_home.join("devin/homes").join(key);
    let xdg_config_home = sess_root.join("config");
    let cfg_dir = xdg_config_home.join("devin");

    std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;
    std::fs::create_dir_all(&cfg_dir).map_err(|e| format!("create {}: {e}", cfg_dir.display()))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("create {}: {e}", data_dir.display()))?;
    std::fs::create_dir_all(&xdg_cache_home)
        .map_err(|e| format!("create {}: {e}", xdg_cache_home.display()))?;

    let shared_cli = snippet_home.join("devin/shared-cli");
    std::fs::create_dir_all(&shared_cli)
        .map_err(|e| format!("create {}: {e}", shared_cli.display()))?;
    ensure_symlink(&shared_cli, &data_dir.join("cli"))?;

    let token = account.and_then(account_token);
    let cred_target = data_dir.join("credentials.toml");
    if let Some(tok) = &token {
        write_credentials_file(&cred_target, tok)?;
    } else if (account.is_none() || account == Some(crate::accounts::MAIN))
        && let Some(real_creds) = real_data
            .map(|d| d.join("credentials.toml"))
            .filter(|p| p.is_file())
    {
        ensure_symlink(&real_creds, &cred_target)?;
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = match exe.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
        Some(path) => PathBuf::from(path),
        None => exe,
    };
    let exe = if exe
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "deps")
    {
        let sibling = exe
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("snippet"));
        sibling.filter(|p| p.is_file()).unwrap_or(exe)
    } else {
        exe
    };
    let org_id = real_org_id();
    let config = build_config_json(&exe, org_id.as_deref());
    let config_path = cfg_dir.join("config.json");
    write_if_changed(&config_path, &config)?;
    write_if_changed(
        &cfg_dir.join("mcp_config.json"),
        &json!({
            "mcpServers": {
                MCP_SERVER_NAME: { "type": "http", "url": mcp_url }
            }
        }),
    )?;
    write_text_if_changed(&cfg_dir.join("AGENTS.md"), prompt)?;

    Ok(PreparedEnv {
        home,
        xdg_config_home,
        xdg_data_home,
        xdg_cache_home,
        config_path,
        api_key: token,
    })
}

fn write_credentials_file(path: &Path, token: &str) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        let _ = std::fs::remove_file(path);
    }
    let escaped = serde_json::to_string(token).map_err(|e| e.to_string())?;
    let body = format!(
        "windsurf_api_key = {escaped}\napi_server_url = \"https://server.codeium.com\"\ndevin_webapp_host = \"app.devin.ai\"\ndevin_api_url = \"https://api.devin.ai\"\n"
    );
    write_text_if_changed(path, &body)?;
    crate::config::set_private(path);
    Ok(())
}

fn ensure_symlink(target: &Path, link: &Path) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(link) {
        if meta.file_type().is_symlink() && std::fs::read_link(link).ok().as_deref() == Some(target)
        {
            return Ok(());
        }
        if meta.is_dir() && !meta.file_type().is_symlink() {
            let _ = std::fs::remove_dir_all(link);
        } else {
            let _ = std::fs::remove_file(link);
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| format!("link {}: {e}", link.display()))?;
    Ok(())
}

fn write_if_changed(path: &Path, value: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_text_if_changed(path, &body)
}

fn write_text_if_changed(path: &Path, body: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(body) {
        std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

pub async fn auth_status() -> (bool, Option<String>) {
    let Some(bin) = binary() else {
        return (false, None);
    };
    let account = crate::accounts::active("devin");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let temp = if let Some(token) = account.as_deref().and_then(account_token) {
        let dir = crate::config::snippet_home().join("devin/probe");
        let devin_dir = dir.join("devin");
        let _ = std::fs::create_dir_all(&devin_dir);
        let _ = write_credentials_file(&devin_dir.join("credentials.toml"), &token);
        cmd.env("XDG_DATA_HOME", &dir);
        Some(dir)
    } else {
        None
    };
    let out = tokio::time::timeout(std::time::Duration::from_secs(6), cmd.output()).await;
    let _ = temp;
    let Ok(Ok(out)) = out else {
        return (false, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    parse_auth_status(&text)
}

pub fn parse_auth_status(text: &str) -> (bool, Option<String>) {
    let logged_in = text
        .lines()
        .any(|l| l.trim_start().starts_with("Logged in"));
    let failed = text.contains("Failed to fetch from server:");
    if !logged_in || failed {
        return (false, None);
    }
    let email = text
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("Email:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let via = text
        .lines()
        .find_map(|l| {
            l.trim_start()
                .strip_prefix("Logged in (via ")
                .and_then(|r| r.strip_suffix(")."))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (true, email.or(via))
}

pub async fn models_json() -> Result<(Option<String>, Value), String> {
    let bin = binary().ok_or("Devin CLI (devin) isn't installed or isn't on PATH")?;
    let account = crate::accounts::active("devin");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["models", "list", "--format", "json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if let Some(token) = account.as_deref().and_then(account_token) {
        let dir = crate::config::snippet_home().join("devin/probe");
        let devin_dir = dir.join("devin");
        let _ = std::fs::create_dir_all(&devin_dir);
        write_credentials_file(&devin_dir.join("credentials.toml"), &token)?;
        cmd.env("XDG_DATA_HOME", &dir);
        cmd.env("WINDSURF_API_KEY", token);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| "Devin CLI took too long to list models".to_string())?
        .map_err(|e| format!("couldn't run devin models list: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "devin listed no models — is it signed in?".to_string()
        } else {
            err
        });
    }
    let parsed: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parse devin models json: {e}"))?;
    Ok((account, parsed))
}

pub async fn models() -> Result<Vec<String>, String> {
    let (_, raw) = models_json().await?;
    let list = parse_model_ids(&raw);
    if list.is_empty() {
        return Err("devin listed no models — is it signed in?".to_string());
    }
    Ok(list)
}

pub fn parse_model_ids(raw: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let Some(families) = raw.get("families").and_then(Value::as_array) else {
        return ids;
    };
    for family in families {
        let slug = family
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if slug == "fusion" {
            continue;
        }
        if !slug.is_empty() && !ids.iter().any(|x| x == slug) {
            ids.push(slug.to_string());
        }
        if let Some(variants) = family.get("variants").and_then(Value::as_array) {
            for variant in variants {
                if let Some(uid) = variant
                    .get("model_uid")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    && !uid.is_empty()
                    && !uid.starts_with("MODEL_")
                    && !ids.iter().any(|x| x == uid)
                {
                    ids.push(uid.to_string());
                }
            }
        }
    }
    ids
}

fn sessions_db_path() -> Option<PathBuf> {
    sessions_db_path_in(&crate::config::snippet_home())
}

fn sessions_db_path_in(snippet_home: &Path) -> Option<PathBuf> {
    let db = snippet_home.join("devin/shared-cli/sessions.db");
    db.is_file().then_some(db)
}

/// Copy a Devin session in `sessions.db` under a fresh id so a forked Snippet
/// session continues from the real conversation without mutating the source.
pub fn fork_conversation(source: &str) -> Result<String, String> {
    let db = sessions_db_path().ok_or("Devin sessions.db does not exist")?;
    fork_conversation_in(&db, source)
}

pub fn fork_conversation_in(db_path: &Path, source: &str) -> Result<String, String> {
    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
    let copy = format!(
        "snippet-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let copied = tx
        .execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id, shell_last_seen_index, cogs_json, workspace_dirs, hidden, metadata)
             SELECT ?1, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id, shell_last_seen_index, cogs_json, workspace_dirs, hidden, metadata
             FROM sessions WHERE id = ?2",
            rusqlite::params![&copy, source],
        )
        .map_err(|e| e.to_string())?;
    if copied == 0 {
        return Err(format!("no devin session row for {source}"));
    }
    tx.execute(
        "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at, metadata)
         SELECT ?1, node_id, parent_node_id, chat_message, created_at, metadata
         FROM message_nodes WHERE session_id = ?2 ORDER BY row_id ASC",
        rusqlite::params![&copy, source],
    )
    .map_err(|e| e.to_string())?;
    let _ = tx.execute(
        "INSERT INTO tool_call_state (session_id, tool_call_id, tool_call_json, tool_call_update_json, source_node_ids)
         SELECT ?1, tool_call_id, tool_call_json, tool_call_update_json, source_node_ids
         FROM tool_call_state WHERE session_id = ?2",
        rusqlite::params![&copy, source],
    );
    tx.commit().map_err(|e| e.to_string())?;
    Ok(copy)
}

/// Whether a Devin ACP frame indicates account quota exhaustion, returning an
/// optional reset unix-timestamp hint when the error includes `retryAfterSeconds`.
pub fn quota_exhausted_hint(event: &Value) -> Option<i64> {
    let now = chrono::Utc::now().timestamp();
    if let Some(err) = event.get("error") {
        let code = err.get("code").and_then(Value::as_i64);
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
        let data = err.get("data");
        let kind = data
            .and_then(|d| d.get("cognition.ai/errorKind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let retry_secs = data
            .and_then(|d| d.get("cognition.ai/retryAfterSeconds"))
            .and_then(Value::as_i64)
            .filter(|s| *s > 0);
        let msg_lower = msg.to_ascii_lowercase();
        let is_quota = code == Some(-32011)
            || kind == "resource_exhausted"
            || msg_lower.contains("quota exhausted")
            || msg_lower.contains("usage limit reached")
            || msg_lower.contains("subscription limit reached")
            || msg_lower.contains("acu limit");
        if is_quota {
            return Some(retry_secs.map(|s| now + s).unwrap_or(0));
        }
    }
    if event.get("method").and_then(Value::as_str) == Some("_cognition.ai/agent_stopped") {
        let params = event.get("params").unwrap_or(&Value::Null);
        let cause = params.get("cause").and_then(Value::as_str).unwrap_or("");
        let msg = params
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            cause,
            "quota_exhausted" | "usage_limit_reached" | "resource_exhausted"
        ) || msg.contains("quota exhausted")
            || msg.contains("usage limit reached")
            || msg.contains("subscription limit reached")
        {
            return Some(0);
        }
    }
    None
}

struct Login {
    child: Child,
    stdin: Option<ChildStdin>,
    dir: PathBuf,
    label: String,
    lines: Arc<std::sync::Mutex<Vec<String>>>,
}

static LOGINS: LazyLock<tokio::sync::Mutex<HashMap<String, Login>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

pub fn extract_login_url(line: &str) -> Option<String> {
    let clean = strip_ansi_and_osc(line);
    for token in clean.split_whitespace() {
        let Some(pos) = token.find("https://") else {
            continue;
        };
        let candidate = &token[pos..];
        if let Ok(parsed) = reqwest::Url::parse(candidate)
            && parsed.scheme() == "https"
            && let Some(host) = parsed.host_str()
        {
            let host = host.to_ascii_lowercase();
            let allowed = host == "app.devin.ai"
                || host.ends_with(".devin.ai")
                || host.ends_with(".codeium.com")
                || host.ends_with(".windsurf.com");
            if allowed {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn strip_ansi_and_osc(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b']' => {
                    i += 1;
                    let mut closed = false;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            closed = true;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    if closed {
                        // Replace OSC framing with a space so OSC 8 target and visible label do not glue together.
                        out.push(b' ');
                    }
                }
                _ => {
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn valid_login_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub async fn begin_login(id: &str, label: &str) -> Result<String, String> {
    let bin = binary().ok_or("Devin CLI (devin) isn't installed or isn't on PATH")?;
    begin_login_with(&bin, &crate::config::snippet_home(), id, label).await
}

async fn begin_login_with(
    bin: &Path,
    snippet_home: &Path,
    id: &str,
    label: &str,
) -> Result<String, String> {
    if !valid_login_id(id) {
        return Err("invalid account id".to_string());
    }
    if let Some(mut old) = LOGINS.lock().await.remove(id) {
        let _ = old.child.kill().await;
        let _ = std::fs::remove_dir_all(&old.dir);
    }

    let dir = snippet_home.join("devin/login").join(id);
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    let config = dir.join("config");
    let data = dir.join("data");
    let cache = dir.join("cache");
    for sub in [&home, &config, &data, &cache] {
        std::fs::create_dir_all(sub).map_err(|e| format!("create {}: {e}", sub.display()))?;
    }

    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["auth", "login", "--force-manual-token-flow"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CACHE_HOME", &cache)
        .env_remove("WINDSURF_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start devin auth login: {e}"))?;
    let stdin = child.stdin.take().ok_or("devin login stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("devin login stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("devin login stderr unavailable")?;

    let collected = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stderr_lines = Arc::clone(&collected);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(mut buf) = stderr_lines.lock()
                && buf.len() < 64
            {
                buf.push(line);
            }
        }
    });

    let mut out_reader = BufReader::new(stdout).lines();
    let url_res = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Ok(Some(line)) = out_reader.next_line().await {
            if let Some(url) = extract_login_url(&line) {
                return Ok(url);
            }
            if let Ok(mut buf) = collected.lock()
                && buf.len() < 64
            {
                buf.push(line);
            }
        }
        Err("Devin login exited before printing a sign-in link".to_string())
    })
    .await;

    let url = match url_res {
        Ok(Ok(url)) => url,
        Ok(Err(msg)) => {
            let _ = child.kill().await;
            let detail = last_clean_line(&collected).unwrap_or(msg);
            let _ = std::fs::remove_dir_all(&dir);
            return Err(detail);
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = std::fs::remove_dir_all(&dir);
            return Err("timed out waiting for Devin sign-in link".to_string());
        }
    };

    let stdout_lines = Arc::clone(&collected);
    tokio::spawn(async move {
        while let Ok(Some(line)) = out_reader.next_line().await {
            if let Ok(mut buf) = stdout_lines.lock()
                && buf.len() < 64
            {
                buf.push(line);
            }
        }
    });

    if let Some(mut displaced) = LOGINS.lock().await.insert(
        id.to_string(),
        Login {
            child,
            stdin: Some(stdin),
            dir: dir.clone(),
            label: label.to_string(),
            lines: collected,
        },
    ) {
        let _ = displaced.child.kill().await;
        if displaced.dir != dir {
            let _ = std::fs::remove_dir_all(&displaced.dir);
        }
    }

    let reap_id = id.to_string();
    let reap_dir = dir;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15 * 60)).await;
        let mut guard = LOGINS.lock().await;
        if guard
            .get(&reap_id)
            .is_some_and(|entry| entry.dir == reap_dir)
            && let Some(mut stale) = guard.remove(&reap_id)
        {
            drop(guard);
            let _ = stale.child.kill().await;
            let _ = std::fs::remove_dir_all(&stale.dir);
        }
    });

    Ok(url)
}

fn last_clean_line(lines: &std::sync::Mutex<Vec<String>>) -> Option<String> {
    let guard = lines.lock().ok()?;
    guard
        .iter()
        .rev()
        .map(|l| strip_ansi_and_osc(l))
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
}

pub async fn complete_login(id: &str, code: &str) -> Result<(String, String), String> {
    let Some(mut login) = LOGINS.lock().await.remove(id) else {
        return Err("no sign-in in progress for this account; start again".to_string());
    };
    let res = finish_login(&mut login, code).await;
    let _ = std::fs::remove_dir_all(&login.dir);
    res
}

async fn finish_login(login: &mut Login, code: &str) -> Result<(String, String), String> {
    let mut stdin = login
        .stdin
        .take()
        .ok_or_else(|| "devin login stdin unavailable".to_string())?;
    let payload = format!("{}\n", code.trim());
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("couldn't send code to Devin login: {e}"))?;
    let _ = stdin.flush().await;
    drop(stdin);

    let status =
        match tokio::time::timeout(std::time::Duration::from_secs(60), login.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(format!("Devin login failed: {e}")),
            Err(_) => {
                let _ = login.child.kill().await;
                return Err("Devin login timed out waiting for verification".to_string());
            }
        };

    if !status.success() {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let msg =
            last_clean_line(&login.lines).unwrap_or_else(|| "Devin rejected the code".to_string());
        return Err(msg);
    }

    let cred_path = login.dir.join("data/devin/credentials.toml");
    let raw = std::fs::read_to_string(&cred_path)
        .map_err(|_| "Devin login finished without writing credentials.toml".to_string())?;
    let token = parse_credential_token(&raw)
        .ok_or_else(|| "Devin credentials.toml did not contain an API token".to_string())?;
    Ok((token, login.label.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_allows_snippet_mcp_tools_and_blocks_builtins() {
        let ok = decision(
            r#"{"tool_name":"mcp__snippet__bash","tool_input":{"command":"ls"}}"#,
            true,
        );
        assert_eq!(ok["decision"], "approve");
        let proxy_ok = decision(
            r#"{"tool_name":"mcp_call_tool","tool_input":{"server_name":"snippet","tool_name":"bash","arguments":{"command":"ls"}}}"#,
            true,
        );
        assert_eq!(proxy_ok["decision"], "approve");
        let proxy_other = decision(
            r#"{"tool_name":"mcp_call_tool","tool_input":{"server_name":"github","tool_name":"list_issues"}}"#,
            true,
        );
        assert_eq!(proxy_other["decision"], "block");
        let list = decision(
            r#"{"tool_name":"mcp_list_tools","tool_input":{"server_name":"snippet"}}"#,
            true,
        );
        assert_eq!(list["decision"], "approve");
        let skill_block = decision(
            r#"{"tool_name":"skill","tool_input":{"skill":"devin-cli"}}"#,
            true,
        );
        assert_eq!(skill_block["decision"], "block");
        let other_mcp = decision(r#"{"tool_name":"mcp__github__list_issues"}"#, true);
        assert_eq!(other_mcp["decision"], "block");
        let builtin = decision(
            r#"{"tool_name":"exec","tool_input":{"command":"rm -rf /"}}"#,
            true,
        );
        assert_eq!(builtin["decision"], "block");
        assert!(
            builtin["reason"]
                .as_str()
                .unwrap()
                .contains("mcp__snippet__bash")
        );
        let outside = decision(r#"{"tool_name":"exec"}"#, false);
        assert_eq!(outside["decision"], "approve");

        let cfg = build_config_json(Path::new("/usr/local/bin/snippet"), Some("org-123"));
        assert_eq!(cfg["version"], 1);
        assert_eq!(cfg["devin"]["org_id"], "org-123");
        assert_eq!(cfg["shell"]["setup_complete"], true);
        assert_eq!(cfg["respect_workspace_trust"], false);
        assert_eq!(cfg["disabled_tools"].as_array().unwrap().len(), 19);
    }

    #[test]
    fn credential_parser_accepts_json_toml_and_raw_tokens() {
        assert_eq!(
            parse_credential_token(r#"{"token":"devin-abc123"}"#).as_deref(),
            Some("devin-abc123")
        );
        assert_eq!(
            parse_credential_token("windsurf_api_key = \"devin-xyz\"\napi_server_url = \"https://server.codeium.com\"\n").as_deref(),
            Some("devin-xyz")
        );
        assert_eq!(
            parse_credential_token("devin-raw-key").as_deref(),
            Some("devin-raw-key")
        );
        assert_eq!(parse_credential_token("   "), None);
    }

    #[test]
    fn auth_status_parses_email_and_rejects_server_errors() {
        let good = "Logged in (via Devin).\n\nUser:\n  Email: dev@example.com\n";
        assert_eq!(
            parse_auth_status(good),
            (true, Some("dev@example.com".into()))
        );
        let bad = "Logged in (via Devin).\n\nUser / team info:\n  Failed to fetch from server: Authentication required: invalid api key\n";
        assert_eq!(parse_auth_status(bad), (false, None));
        assert_eq!(parse_auth_status("Not logged in."), (false, None));
    }

    #[test]
    fn quota_exhausted_detects_acp_32011_and_retry_hint() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "error": {
                "code": -32011,
                "message": "Quota exhausted.",
                "data": {
                    "cognition.ai/errorKind": "resource_exhausted",
                    "cognition.ai/retryAfterSeconds": 900
                }
            }
        });
        let hint = quota_exhausted_hint(&frame).expect("quota hint");
        assert!(hint >= chrono::Utc::now().timestamp() + 895);

        let auth_err = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "error": { "code": -32000, "message": "Please log in to use Devin." }
        });
        assert_eq!(quota_exhausted_hint(&auth_err), None);
    }

    #[test]
    fn fork_conversation_clones_sqlite_session_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, backend_type TEXT NOT NULL,
                model TEXT NOT NULL, agent_mode TEXT NOT NULL, created_at INTEGER NOT NULL,
                last_activity_at INTEGER NOT NULL, title TEXT, main_chain_id INTEGER,
                shell_last_seen_index INTEGER DEFAULT 0, cogs_json TEXT, workspace_dirs TEXT,
                hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT
             );
             CREATE TABLE message_nodes (
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                node_id INTEGER NOT NULL, parent_node_id INTEGER, chat_message TEXT NOT NULL,
                created_at INTEGER NOT NULL, metadata TEXT
             );
             CREATE TABLE tool_call_state (
                session_id TEXT NOT NULL, tool_call_id TEXT NOT NULL,
                tool_call_json TEXT, tool_call_update_json TEXT, source_node_ids TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id)
             VALUES ('orig-1', '/tmp', 'local', 'swe-2', 'bypass', 1, 2, 'hi', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at)
             VALUES ('orig-1', 0, NULL, '{\"role\":\"user\",\"content\":\"hi\"}', 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let forked = fork_conversation_in(&db, "orig-1").unwrap();
        assert!(forked.starts_with("snippet-"));
        let conn = rusqlite::Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_nodes WHERE session_id = ?1",
                [&forked],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn prepare_home_and_sessions_db_never_use_real_data_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let real_devin = dir.path().join("real-devin");
        let real_cli = real_devin.join("cli");
        std::fs::create_dir_all(&real_cli).unwrap();
        let real_db = real_cli.join("sessions.db");
        std::fs::write(&real_db, b"real-user-sessions").unwrap();
        std::fs::write(
            real_devin.join("credentials.toml"),
            "windsurf_api_key = \"devin-main\"\n",
        )
        .unwrap();

        let snippet_home = dir.path().join("snippet-home");
        let env = prepare_home_in(
            &snippet_home,
            Some(&real_devin),
            "sess-1",
            "http://127.0.0.1:9999/mcp",
            "prompt",
            None,
        )
        .unwrap();
        let cli_link = env.xdg_data_home.join("devin/cli");
        assert_eq!(
            std::fs::read_link(&cli_link).unwrap(),
            snippet_home.join("devin/shared-cli")
        );
        let cred_link = env.xdg_data_home.join("devin/credentials.toml");
        assert_eq!(
            std::fs::read_link(&cred_link).unwrap(),
            real_devin.join("credentials.toml")
        );
        assert_eq!(sessions_db_path_in(&snippet_home), None);

        let priv_db = snippet_home.join("devin/shared-cli/sessions.db");
        std::fs::write(&priv_db, b"snippet-private-sessions").unwrap();
        assert_eq!(
            sessions_db_path_in(&snippet_home).as_deref(),
            Some(priv_db.as_path())
        );
        assert_eq!(std::fs::read(&real_db).unwrap(), b"real-user-sessions");
    }

    #[test]
    fn extract_login_url_handles_plain_osc8_and_non_devin_lines() {
        let plain = "Visit https://app.devin.ai/auth/cli/continue?state=s1&code_challenge=c1&code_challenge_method=S256&cli_pkce_marker=1 to sign in, then copy the code and paste it below.";
        assert_eq!(
            extract_login_url(plain).as_deref(),
            Some(
                "https://app.devin.ai/auth/cli/continue?state=s1&code_challenge=c1&code_challenge_method=S256&cli_pkce_marker=1"
            )
        );

        let osc8 = "\x1b[1mVisit \x1b]8;;https://app.devin.ai/auth/cli/continue?state=s2&cli_pkce_marker=1\x1b\\https://app.devin.ai/auth/cli/continue?state=s2&cli_pkce_marker=1\x1b]8;;\x1b\\\x1b[0m to sign in";
        assert_eq!(
            extract_login_url(osc8).as_deref(),
            Some("https://app.devin.ai/auth/cli/continue?state=s2&cli_pkce_marker=1")
        );

        assert_eq!(
            extract_login_url("Visit https://auth.windsurf.com/cli?state=s3 now").as_deref(),
            Some("https://auth.windsurf.com/cli?state=s3")
        );
        assert_eq!(extract_login_url("No link on this line at all"), None);
        assert_eq!(
            extract_login_url("Visit https://evil.example.com/app.devin.ai?state=bad"),
            None
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_cli_begin_and_complete_login_round_trip_and_reject_bad_code() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let fake_bin = tmp.path().join("fake-devin");
        let script = r#"#!/bin/sh
printf 'Visit \033]8;;https://app.devin.ai/auth/cli/continue?state=teststate\033\\https://app.devin.ai/auth/cli/continue?state=teststate\033]8;;\033\\ to sign in.\n'
IFS= read -r code
if [ "$code" = "good-code" ]; then
  mkdir -p "$XDG_DATA_HOME/devin"
  printf 'windsurf_api_key = "fake-token-123"\n' > "$XDG_DATA_HOME/devin/credentials.toml"
  exit 0
fi
printf '\033[31mError: invalid authorization code\033[0m\n' >&2
exit 1
"#;
        std::fs::write(&fake_bin, script).unwrap();
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let snippet_home = tmp.path().join("snippet");
        let url = begin_login_with(&fake_bin, &snippet_home, "slotA1", "Work")
            .await
            .unwrap();
        assert_eq!(
            url,
            "https://app.devin.ai/auth/cli/continue?state=teststate"
        );
        assert!(snippet_home.join("devin/login/slotA1").is_dir());

        let (tok, label) = complete_login("slotA1", "  good-code  ").await.unwrap();
        assert_eq!(tok, "fake-token-123");
        assert_eq!(label, "Work");
        assert!(!snippet_home.join("devin/login/slotA1").exists());

        let missing = complete_login("slotA1", "good-code").await.unwrap_err();
        assert_eq!(
            missing,
            "no sign-in in progress for this account; start again"
        );

        begin_login_with(&fake_bin, &snippet_home, "slotB2", "")
            .await
            .unwrap();
        let err = complete_login("slotB2", "wrong").await.unwrap_err();
        assert_eq!(err, "Error: invalid authorization code");
        assert!(!snippet_home.join("devin/login/slotB2").exists());
    }
}
