//! A `bash` that only reads, for sessions that must not change files (an
//! agent's inbox). Every segment of a command must start with an allow-listed
//! reader, and redirects other than to /dev/null are refused.

use super::*;
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;

pub struct ReadOnlyBashTool;

const READERS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "rg", "grep", "find", "tree", "stat", "file", "du", "df",
    "pwd", "echo", "printf", "date", "whoami", "uname", "which", "jq", "sort", "uniq", "cut", "tr",
    "sed", "basename", "dirname", "realpath", "readlink", "cd", "true", "column", "nl", "diff",
    "git", "snippet",
];

const GIT_READS: &[&str] = &[
    "log",
    "show",
    "diff",
    "status",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
    "describe",
    "shortlog",
];

/// Why a command is refused, or `Ok` when it only reads.
pub fn read_only_verdict(command: &str) -> Result<(), String> {
    if command.contains('`')
        || command.contains("$(")
        || command.contains("<(")
        || command.contains(">(")
    {
        return Err("command substitution isn't allowed in this read-only shell".into());
    }
    let stripped = command
        .replace("2>&1", " ")
        .replace("&>/dev/null", " ")
        .replace("2>/dev/null", " ")
        .replace(">/dev/null", " ");
    if stripped.contains('>') {
        return Err(
            "redirecting output to a file isn't allowed here; this session doesn't change files"
                .into(),
        );
    }
    for segment in stripped.split(['|', ';', '&', '\n']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .skip_while(|w| w.contains('=') && !w.starts_with('-'))
            .collect();
        let Some(first) = words.first() else { continue };
        let program = first.rsplit('/').next().unwrap_or(first);
        if !READERS.contains(&program) {
            return Err(format!("`{program}` isn't a read-only command here"));
        }
        let args = &words[1..];
        let refused = match program {
            "git" => git_refused(args),
            "sed" => sed_refused(args),
            "sort" => args.iter().any(|a| {
                a.starts_with("--output")
                    || a.starts_with("--compress-program")
                    || short_cluster(a, 'o')
            }),
            "tree" => args
                .iter()
                .any(|a| a.starts_with("--output") || short_cluster(a, 'o')),
            "rg" => args
                .iter()
                .any(|a| a.starts_with("--pre") || a.starts_with("--hostname-bin")),
            "file" => args.iter().any(|a| short_cluster(a, 'C')),
            "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() > 1,
            "snippet" => args.first() != Some(&"history"),
            "find" => args.iter().any(|a| {
                matches!(
                    *a,
                    "-delete"
                        | "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-fprint"
                        | "-fprint0"
                        | "-fprintf"
                        | "-fls"
                )
            }),
            _ => false,
        };
        if refused {
            return Err(format!(
                "`{}` changes things; this shell only reads",
                words.join(" ")
            ));
        }
    }
    Ok(())
}

fn short_cluster(arg: &str, flag: char) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag)
}

fn git_refused(args: &[&str]) -> bool {
    let mut it = args.iter();
    let mut sub = None;
    while let Some(a) = it.next() {
        match *a {
            "-C" => {
                it.next();
            }
            a if a.starts_with("-c")
                || a.starts_with("--config-env")
                || a.starts_with("--exec-path")
                || a.starts_with("--git-dir")
                || a.starts_with("--work-tree") =>
            {
                return true;
            }
            a if a.starts_with('-') => {}
            a => {
                sub = Some(a);
                break;
            }
        }
    }
    let Some(sub) = sub else { return true };
    !GIT_READS.contains(&sub)
        || args.iter().any(|a| {
            a.starts_with("--output")
                || a.starts_with("--open-files-in-pager")
                || a.starts_with("--ext-diff")
                || (sub == "grep" && a.starts_with("-O"))
        })
}

fn sed_refused(args: &[&str]) -> bool {
    let mut scripts: Vec<String> = Vec::new();
    let mut quoted = String::new();
    let mut in_quote: Option<char> = None;
    for a in args {
        if a.starts_with("--in-place") || a.starts_with("--file") || a.starts_with("--follow") {
            return true;
        }
        if short_cluster(a, 'i') || short_cluster(a, 'f') {
            return true;
        }
    }
    let mut take_next = false;
    let mut have_script = false;
    for a in args {
        if let Some(q) = in_quote {
            quoted.push(' ');
            quoted.push_str(a.trim_end_matches(q));
            if a.ends_with(q) {
                scripts.push(std::mem::take(&mut quoted));
                in_quote = None;
            }
            continue;
        }
        let is_script = take_next || (!have_script && !a.starts_with('-'));
        if !is_script {
            take_next = *a == "-e" || *a == "--expression";
            if let Some(rest) = a.strip_prefix("--expression=") {
                scripts.push(rest.trim_matches(['\'', '"']).to_string());
                have_script = true;
            }
            continue;
        }
        take_next = false;
        have_script = true;
        match a.chars().next() {
            Some(q @ ('\'' | '"')) => {
                let body = &a[1..];
                if body.ends_with(q) && !body.is_empty() {
                    scripts.push(body[..body.len() - 1].to_string());
                } else {
                    quoted = body.to_string();
                    in_quote = Some(q);
                }
            }
            _ => scripts.push(a.to_string()),
        }
    }
    in_quote.is_some() || scripts.iter().any(|s| !sed_script_is_safe(s))
}

fn sed_script_is_safe(script: &str) -> bool {
    let c: Vec<char> = script.chars().collect();
    let mut i = 0;
    let skip_delimited = |i: &mut usize, delim: char| -> bool {
        while *i < c.len() {
            match c[*i] {
                '\\' => *i += 2,
                x if x == delim => {
                    *i += 1;
                    return true;
                }
                _ => *i += 1,
            }
        }
        false
    };
    while i < c.len() {
        match c[i] {
            ' ' | '\t' | '\n' | ';' | '{' | '}' | '!' | ',' | '$' | '~' | '+' | '0'..='9' => i += 1,
            '/' => {
                i += 1;
                if !skip_delimited(&mut i, '/') {
                    return false;
                }
                while i < c.len() && matches!(c[i], 'I' | 'M') {
                    i += 1;
                }
            }
            '\\' => {
                let Some(&d) = c.get(i + 1) else { return false };
                i += 2;
                if !skip_delimited(&mut i, d) {
                    return false;
                }
            }
            's' => {
                let Some(&d) = c.get(i + 1) else { return false };
                i += 2;
                if !skip_delimited(&mut i, d) || !skip_delimited(&mut i, d) {
                    return false;
                }
                while i < c.len() && !matches!(c[i], ';' | '}' | '\n' | ' ') {
                    if matches!(c[i], 'w' | 'W' | 'e') {
                        return false;
                    }
                    i += 1;
                }
            }
            'y' => {
                let Some(&d) = c.get(i + 1) else { return false };
                i += 2;
                if !skip_delimited(&mut i, d) || !skip_delimited(&mut i, d) {
                    return false;
                }
            }
            'p' | 'P' | 'd' | 'D' | 'n' | 'N' | 'g' | 'G' | 'h' | 'H' | 'x' | 'l' | 'q' | 'Q'
            | '=' => i += 1,
            'a' | 'i' | 'c' | '#' | 'b' | 't' | 'T' | ':' => {
                while i < c.len()
                    && c[i] != '\n'
                    && !(matches!(c[i], ';' | '}') && !matches!(c[i - 1], '\\'))
                {
                    i += 1;
                }
                if i < c.len() && c[i] == '\n' {
                    i += 1;
                }
            }
            _ => return false,
        }
    }
    true
}

#[async_trait]
impl Tool for ReadOnlyBashTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "bash".to_string(),
            description: "Read-only shell for looking things up: ls, cat, head, rg, find, jq, and git log/show/diff/status. Anything that writes, moves, deletes, installs or builds is refused; work that changes files goes to Mission Control.".to_string(),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string"},
                    "label": {"type": "string", "description": "a few words on what it's for"},
                    "max_lines": {"type": "integer", "minimum": 1}
                }),
                &["command", "label"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Err(why) = read_only_verdict(command) {
            return Ok(ToolResult::error(why));
        }
        // Secret approvals raised from a desk have no chat to land in; secret work belongs in a task.
        let session = ctx
            .durable_session_id()
            .or(ctx.lane_origin().map(|(session, _)| session));
        if session.is_some_and(crate::session::is_inbox_session_id)
            && !crate::vault::Vault::load()
                .referenced_names(command)
                .is_empty()
        {
            return Ok(ToolResult::error(
                "Desks can't use vault secrets. File the work as a task so a work session runs it.",
            ));
        }
        let mut forwarded = arguments.clone();
        if let Some(map) = forwarded.as_object_mut() {
            map.remove("background");
        }
        BashTool.execute(ctx, forwarded).await
    }
}

#[cfg(test)]
mod tests {
    use super::read_only_verdict;

    #[tokio::test]
    async fn desk_shell_refuses_vault_secrets() {
        use crate::tools::Tool;
        // A vault with one secret, under a scratch HOME.
        let home = tempfile::tempdir().unwrap();
        let vault_dir = home.path().join(".snippet");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::write(
            vault_dir.join("vault.json"),
            r#"{"DESK_TEST_SECRET":"s3cr3t-value"}"#,
        )
        .unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let workspace = tempfile::tempdir().unwrap();
        let desk = crate::tools::ToolContext::new(workspace.path())
            .unwrap()
            .with_durable_session_id("inbox-desk-agent");
        let worker = crate::tools::ToolContext::new(workspace.path())
            .unwrap()
            .with_durable_session_id("work-session-1");
        let result = super::ReadOnlyBashTool
            .execute(
                &desk,
                serde_json::json!({"command": "echo $DESK_TEST_SECRET", "label": "t"}),
            )
            .await
            .unwrap();
        let message = result.value["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("Desks can't use vault secrets"),
            "{result:?}"
        );
        // A plain read still passes the vault check in a desk.
        let result = super::ReadOnlyBashTool
            .execute(
                &desk,
                serde_json::json!({"command": "ls -la", "label": "t"}),
            )
            .await
            .unwrap();
        assert_eq!(result.value["status"], "success", "{result:?}");
        // Work sessions are unchanged: the vault check only guards desks.
        drop(worker);
        if let Some(old) = old_home {
            unsafe { std::env::set_var("HOME", old) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
    }

    #[test]
    fn readers_pass_and_writers_are_refused() {
        for ok in [
            "ls -la src | head -20",
            "rg -n foo src 2>/dev/null; git log --oneline -5",
            "git -C /repo status --short && cat README.md",
            "sed -n 1,40p src/lib.rs",
            "LC_ALL=C sort file.txt | uniq -c",
            "sed -n '/foo/p' a.txt",
            "sed 's/a b/c/g' a.txt | head",
            "sed -n -e '2p' a.txt",
            "sort -u -k1,1 a.txt",
            "tree -L 2 src",
            "rg --no-heading -n foo src",
            "git -C /repo log --oneline -3",
            "git diff --stat HEAD~1",
            "snippet history search timeout",
        ] {
            assert!(read_only_verdict(ok).is_ok(), "{ok}");
        }
        for bad in [
            "echo hi > notes.md",
            "rm -rf target",
            "git commit -am x",
            "git push",
            "sed -i s/a/b/ f",
            "find . -name '*.tmp' -delete",
            "cat $(which ls)",
            "ls && cargo build",
            "python3 -c 'open(1)'",
            "cat a | tee b",
            "tee out.txt",
            "sed -n '1w out.txt' a",
            "sed -n '/x/W out.txt' a",
            "sed '1e id' a",
            "sed 's/a/b/w out.txt' a",
            "sed 's/a/b/e' a",
            "sed --in-place s/a/b/ a",
            "sed -ni s/a/b/p a",
            "sed -f script.sed a",
            "sort -o out.txt a",
            "sort a --output=out.txt",
            "tree -o out.txt .",
            "rg --pre ./x foo",
            "rg --pre-glob '*.x' foo",
            "git -c core.pager=x log",
            "git --exec-path=/tmp log",
            "git --git-dir=/tmp/x log",
            "git log --output=out.txt",
            "git branch -D x",
            "git config user.name x",
            "git grep -O foo",
            "uniq in out",
            "snippet send x",
            "find . -fprint0 out",
        ] {
            assert!(read_only_verdict(bad).is_err(), "{bad}");
        }
    }
}
