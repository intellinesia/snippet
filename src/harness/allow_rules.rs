//! Session allow-rules: the "always allow this in this session" grant on a
//! Manual-mode approval, and the matcher that lets a later call skip the prompt.
//!
//! A bash rule is keyed by the programs a command runs (`git push`, `rm`).
//! Anything the matcher cannot read with confidence (command substitution,
//! `eval`, `sudo`, `sh -c`, wrappers that run another program) offers no rule
//! and is never auto-approved, so it always asks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A rule the user can grant from an approval request, or has granted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllowRule {
    pub id: String,
    pub label: String,
}

const MAX_KEYS: usize = 3;
const MAX_REASON_CHARS: usize = 240;

/// Programs whose first non-flag argument is part of the key (`git push`).
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "flutter",
    "dart",
    "gh",
    "docker",
    "kubectl",
    "go",
    "pip",
    "uv",
    "make",
    "systemctl",
];

/// Segments running only these are ignored when keying a command.
const IGNORED_PROGRAMS: &[&str] = &["cd", "pwd", "echo", "true", "printf"];

/// Programs that run another program or escalate: the real command is hidden
/// from the key, so no rule may cover them.
const REFUSED_PROGRAMS: &[&str] = &[
    "eval", "sudo", "doas", "su", "exec", "xargs", "env", "nohup", "command", "builtin", "source",
    ".", "time", "nice", "timeout", "watch", "parallel", "setsid", "chroot", "runuser",
];

/// A shell runs whatever its script, stdin or heredoc says, so no rule covers one.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Flags that make a covered program run another program (`git push
/// --receive-pack=...`, `cargo --config target.x.runner=...`).
const REFUSED_FLAG_PREFIXES: &[&str] = &[
    "--config",
    "--exec",
    "--upload-pack",
    "--receive-pack",
    "--ssh",
    "--rcfile",
    "--init-file",
];

/// A redirect target a rule may write: a fd, /dev/null, or a plain file
/// under the working directory or /tmp. Home, absolute paths, `..`,
/// variables and dot-paths (`.git/hooks`, `.bashrc`) are refused.
fn harmless_target(word: &str) -> bool {
    if word == "/dev/null" || word.chars().all(|ch| ch.is_ascii_digit() || ch == '-') {
        return true;
    }
    let rel = word.strip_prefix("/tmp/").unwrap_or(word);
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains(['~', '$', '*', '?', '['])
        && rel.split('/').all(|part| !part.starts_with('.'))
}

/// Environment a prefix may set that changes which code a program runs.
fn risky_assignment(word: &str) -> bool {
    let name = word.split_once('=').map_or(word, |(n, _)| n);
    matches!(
        name,
        "PATH"
            | "BASH_ENV"
            | "ENV"
            | "PAGER"
            | "EDITOR"
            | "VISUAL"
            | "SHELL"
            | "PROMPT_COMMAND"
            | "IFS"
            | "RUSTC"
            | "RUSTC_WRAPPER"
            | "CARGO_BUILD_RUSTC_WRAPPER"
            | "NODE_OPTIONS"
            | "PYTHONPATH"
            | "PYTHONSTARTUP"
            | "PERL5OPT"
            | "RUBYOPT"
    ) || ["LD_", "DYLD_", "GIT_", "SSH_", "CARGO_TARGET_"]
        .iter()
        .any(|p| name.starts_with(p))
        || name.ends_with("_COMMAND")
        || name.ends_with("_EDITOR")
        || name.ends_with("_PAGER")
}

/// find options that run or delete beyond what the key `find` suggests.
const REFUSED_WORDS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir", "-delete"];

/// The headline for an approval card: bash's `label`, else a `title` argument.
pub fn title(tool_name: &str, args: &Value) -> Option<String> {
    let key = if tool_name == "bash" {
        "label"
    } else {
        "title"
    };
    trimmed(args.get(key))
}

/// The call's own `reason` argument, trimmed and capped.
pub fn reason(args: &Value) -> Option<String> {
    let text = trimmed(args.get("reason"))?;
    if text.chars().count() <= MAX_REASON_CHARS {
        return Some(text);
    }
    let mut clipped: String = text.chars().take(MAX_REASON_CHARS - 1).collect();
    clipped.push('…');
    Some(clipped)
}

fn trimmed(value: Option<&Value>) -> Option<String> {
    let text = value?
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// The rule a user may grant from this call's approval request. `custom` is
/// true for a custom tool; callers pass no call that uses vault secrets or is
/// a custom tool's first run (those always ask and never offer a rule).
pub fn offer(tool_name: &str, args: &Value, custom: bool) -> Option<AllowRule> {
    if custom {
        return Some(AllowRule {
            id: format!("tool:{tool_name}"),
            label: format!("{tool_name} runs"),
        });
    }
    match tool_name {
        "change_files" => Some(AllowRule {
            id: "change_files".into(),
            label: "file edits".into(),
        }),
        "bash" => {
            let keys = bash_keys(args.get("command")?.as_str()?)?;
            (!keys.is_empty()).then(|| AllowRule {
                id: format!("bash:{}", keys.join(",")),
                label: label_for_keys(&keys),
            })
        }
        _ => None,
    }
}

/// Whether the granted rules together cover this call. Same caller contract
/// as [`offer`]: never pass a vault or first-run call.
pub fn covers(rules: &[AllowRule], tool_name: &str, args: &Value, custom: bool) -> bool {
    covering(rules, tool_name, args, custom).is_some()
}

/// The label for a call the granted rules cover: the covering rule's label,
/// or the labels of every rule that contributed a key. None when not covered.
pub fn covering(
    rules: &[AllowRule],
    tool_name: &str,
    args: &Value,
    custom: bool,
) -> Option<String> {
    if custom {
        let id = format!("tool:{tool_name}");
        return rules.iter().find(|r| r.id == id).map(|r| r.label.clone());
    }
    match tool_name {
        "change_files" => rules
            .iter()
            .find(|r| r.id == "change_files")
            .map(|r| r.label.clone()),
        "bash" => {
            let keys = bash_keys(args.get("command")?.as_str()?)?;
            if keys.is_empty() {
                return None;
            }
            let keys_of = |r: &AllowRule| -> Vec<String> {
                r.id.strip_prefix("bash:")
                    .map(|k| k.split(',').map(str::to_string).collect())
                    .unwrap_or_default()
            };
            if let Some(single) = rules
                .iter()
                .find(|r| keys.iter().all(|k| keys_of(r).contains(k)))
            {
                return Some(single.label.clone());
            }
            let mut labels: Vec<String> = Vec::new();
            for key in &keys {
                let rule = rules.iter().find(|r| keys_of(r).contains(key))?;
                if !labels.contains(&rule.label) {
                    labels.push(rule.label.clone());
                }
            }
            Some(join_names(&labels))
        }
        _ => None,
    }
}

/// Add a rule unless one with the same id is already granted.
pub fn grant(rules: &mut Vec<AllowRule>, rule: AllowRule) {
    if !rules.iter().any(|r| r.id == rule.id) {
        rules.push(rule);
    }
}

/// The user-facing name of a rule's keys, rendered as-is by the apps.
pub fn label_for_keys(keys: &[String]) -> String {
    let mut names: Vec<String> = Vec::new();
    for key in keys {
        let name = friendly(key)
            .map(str::to_string)
            .unwrap_or_else(|| key.clone());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    join_names(&names)
}

fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn friendly(key: &str) -> Option<&'static str> {
    Some(match key {
        "git push" => "pushes",
        "git commit" => "commits",
        "git add" => "staging",
        "cargo test" | "flutter test" | "npm test" => "test runs",
        "cargo build" | "flutter build" => "builds",
        "rm" => "deletions",
        _ => return None,
    })
}

/// The sorted distinct keys of a bash command, or None when it must not be
/// covered by a rule (refused constructs, unreadable syntax, too many keys).
pub fn bash_keys(command: &str) -> Option<Vec<String>> {
    if command.contains("$(") || command.contains('`') {
        return None;
    }
    let mut keys: Vec<String> = Vec::new();
    for segment in segments(command)? {
        if segment
            .iter()
            .take_while(|w| is_assignment(w) || matches!(w.as_str(), "!" | "{" | "}"))
            .any(|w| is_assignment(w) && risky_assignment(w))
        {
            return None;
        }
        let mut words = segment
            .iter()
            .map(String::as_str)
            .skip_while(|w| is_assignment(w) || matches!(*w, "!" | "{" | "}"))
            .peekable();
        let Some(program) = words.next() else {
            continue;
        };
        let rest: Vec<&str> = words.collect();
        if REFUSED_PROGRAMS.contains(&program)
            || program.starts_with('$')
            || SHELLS.contains(&program)
            || rest.iter().any(|w| {
                REFUSED_WORDS.contains(w)
                    || *w == "eval"
                    || *w == "sudo"
                    || REFUSED_FLAG_PREFIXES.iter().any(|p| w.starts_with(p))
                    || (program == "git" && *w == "-c")
            })
        {
            return None;
        }
        if IGNORED_PROGRAMS.contains(&program) {
            continue;
        }
        let key = if SUBCOMMAND_PROGRAMS.contains(&program) {
            match subcommand(program, &rest)? {
                Some(sub) => format!("{program} {sub}"),
                None => program.to_string(),
            }
        } else {
            program.to_string()
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.len() > MAX_KEYS {
        return None;
    }
    keys.sort();
    Some(keys)
}

/// The first non-flag argument. An unknown flag before it might take a value
/// (`kubectl -n prod delete`), which would make the key lie, so it refuses.
fn subcommand<'a>(program: &str, args: &[&'a str]) -> Option<Option<&'a str>> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !arg.starts_with('-') && !(program == "cargo" && arg.starts_with('+')) {
            return Some(Some(arg));
        }
        if arg.starts_with("--") && arg.contains('=') {
            continue;
        }
        let takes_value = match program {
            "git" => matches!(*arg, "-C" | "-c"),
            "make" => matches!(*arg, "-C" | "-f"),
            _ => false,
        };
        if takes_value {
            iter.next()?;
            continue;
        }
        let boolean = match program {
            "git" => matches!(*arg, "-P" | "--no-pager" | "--paginate" | "--bare"),
            "make" => arg.starts_with("-j") && arg.len() > 2 || matches!(*arg, "-s" | "-k" | "-B"),
            "cargo" => {
                arg.starts_with('+')
                    || matches!(
                        *arg,
                        "-q" | "-v"
                            | "-vv"
                            | "--quiet"
                            | "--verbose"
                            | "--frozen"
                            | "--locked"
                            | "--offline"
                    )
            }
            "flutter" | "dart" => matches!(*arg, "-v" | "--verbose"),
            _ => false,
        };
        if !boolean {
            return None;
        }
    }
    Some(None)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split a command into the words of each simple command, on `&&`, `||`,
/// `;`, `|`, `&`, newlines and parentheses. Quotes and escapes are honoured,
/// redirection targets, comments and heredoc bodies are dropped. None for
/// syntax it cannot read (unbalanced quotes, process substitution).
fn segments(command: &str) -> Option<Vec<Vec<String>>> {
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut skip_word = false;
    let mut heredocs: Vec<String> = Vec::new();
    // None: no pending output target; Some(false): pending; Some(true): wrote a file.
    let mut writes: Option<bool> = None;
    let mut chars = command.chars().peekable();

    fn flush(
        out: &mut [Vec<String>],
        word: &mut String,
        in_word: &mut bool,
        skip: &mut bool,
        writes: &mut Option<bool>,
    ) {
        if *in_word {
            if *skip {
                *skip = false;
                // An output redirect to a real file writes beyond what the key says.
                if writes.take() == Some(false) && !harmless_target(word) {
                    *writes = Some(true);
                }
            } else if let Some(last) = out.last_mut() {
                last.push(std::mem::take(word));
            }
        }
        word.clear();
        *in_word = false;
    }
    fn separate(out: &mut Vec<Vec<String>>, skip: &mut bool) {
        *skip = false;
        if out.last().is_some_and(|s| !s.is_empty()) {
            out.push(Vec::new());
        }
    }

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => word.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        ch => word.push(ch),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(ch) => {
                    in_word = true;
                    word.push(ch);
                }
                None => {}
            },
            ' ' | '\t' | '\r' => flush(
                &mut out,
                &mut word,
                &mut in_word,
                &mut skip_word,
                &mut writes,
            ),
            '\n' => {
                flush(
                    &mut out,
                    &mut word,
                    &mut in_word,
                    &mut skip_word,
                    &mut writes,
                );
                separate(&mut out, &mut skip_word);
                for delimiter in std::mem::take(&mut heredocs) {
                    let mut line = String::new();
                    loop {
                        match chars.next() {
                            None => break,
                            Some('\n') => {
                                if line.trim() == delimiter {
                                    break;
                                }
                                line.clear();
                            }
                            Some(ch) => line.push(ch),
                        }
                    }
                }
            }
            ';' | '(' | ')' => {
                flush(
                    &mut out,
                    &mut word,
                    &mut in_word,
                    &mut skip_word,
                    &mut writes,
                );
                separate(&mut out, &mut skip_word);
            }
            '|' => {
                flush(
                    &mut out,
                    &mut word,
                    &mut in_word,
                    &mut skip_word,
                    &mut writes,
                );
                if matches!(chars.peek(), Some('|' | '&')) {
                    chars.next();
                }
                separate(&mut out, &mut skip_word);
            }
            '&' => {
                flush(
                    &mut out,
                    &mut word,
                    &mut in_word,
                    &mut skip_word,
                    &mut writes,
                );
                if chars.peek() == Some(&'>') {
                    chars.next();
                    if chars.peek() == Some(&'>') {
                        chars.next();
                    }
                    skip_word = true;
                    writes = Some(false);
                } else {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    separate(&mut out, &mut skip_word);
                }
            }
            '>' | '<' => {
                if in_word && word.chars().all(|ch| ch.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                flush(
                    &mut out,
                    &mut word,
                    &mut in_word,
                    &mut skip_word,
                    &mut writes,
                );
                if chars.peek() == Some(&'(') {
                    return None;
                }
                if c == '<' && chars.peek() == Some(&'<') {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        chars.next();
                        skip_word = true;
                        continue;
                    }
                    if chars.peek() == Some(&'-') {
                        chars.next();
                    }
                    while matches!(chars.peek(), Some(' ' | '\t')) {
                        chars.next();
                    }
                    let mut delimiter = String::new();
                    while let Some(&ch) = chars.peek() {
                        if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>') {
                            break;
                        }
                        chars.next();
                        if ch != '\'' && ch != '"' && ch != '\\' {
                            delimiter.push(ch);
                        }
                    }
                    if delimiter.is_empty() {
                        return None;
                    }
                    heredocs.push(delimiter);
                    continue;
                }
                while matches!(chars.peek(), Some('>' | '&' | '|')) {
                    chars.next();
                }
                skip_word = true;
                if c == '>' {
                    writes = Some(false);
                }
            }
            '#' if !in_word => {
                while chars.peek().is_some_and(|ch| *ch != '\n') {
                    chars.next();
                }
            }
            ch => {
                in_word = true;
                word.push(ch);
            }
        }
    }
    flush(
        &mut out,
        &mut word,
        &mut in_word,
        &mut skip_word,
        &mut writes,
    );
    if writes == Some(true) {
        return None;
    }
    out.retain(|s| !s.is_empty());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys(command: &str) -> Option<Vec<String>> {
        bash_keys(command)
    }

    fn k(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    fn bash(command: &str) -> Value {
        json!({"command": command, "label": "x"})
    }

    fn rule(command: &str) -> AllowRule {
        offer("bash", &bash(command), false).expect("rule offered")
    }

    #[test]
    fn splits_on_every_operator() {
        assert_eq!(keys("git push"), k(&["git push"]));
        assert_eq!(
            keys("git add . && git commit -m x"),
            k(&["git add", "git commit"])
        );
        assert_eq!(
            keys("cargo test || rm -rf target"),
            k(&["cargo test", "rm"])
        );
        assert_eq!(keys("ls; cat x"), k(&["cat", "ls"]));
        assert_eq!(keys("git log | head -5"), k(&["git log", "head"]));
        assert_eq!(keys("ls\nrm x"), k(&["ls", "rm"]));
        assert_eq!(keys("sleep 5 & rm x"), k(&["rm", "sleep"]));
        assert_eq!(keys("(cd sub && make test)"), k(&["make test"]));
        assert_eq!(keys("git status |& tee log"), k(&["git status", "tee"]));
    }

    #[test]
    fn quotes_and_escapes_do_not_split() {
        assert_eq!(keys("git commit -m 'a; rm -rf /'"), k(&["git commit"]));
        assert_eq!(keys("git commit -m \"a && b\""), k(&["git commit"]));
        assert_eq!(keys("grep a\\;b file"), k(&["grep"]));
        assert_eq!(keys("git commit -m 'unterminated"), None);
    }

    #[test]
    fn drops_env_prefixes_and_ignored_programs() {
        assert_eq!(keys("RUST_LOG=debug FOO=1 cargo test"), k(&["cargo test"]));
        assert_eq!(keys("cd /repo && git push"), k(&["git push"]));
        assert_eq!(keys("echo hi; pwd; true; printf x"), k(&[]));
        assert_eq!(keys("A=1"), k(&[]));
    }

    #[test]
    fn redirections_comments_and_heredocs() {
        assert_eq!(
            keys("cargo build 2>&1 | tail -20"),
            k(&["cargo build", "tail"])
        );
        assert_eq!(keys("make test > out.log 2> err.log"), k(&["make test"]));
        assert_eq!(keys("cargo test &> log"), k(&["cargo test"]));
        assert_eq!(keys("git push # rm -rf /"), k(&["git push"]));
        assert_eq!(
            keys("git commit -F - <<'EOF'\nrm everything\nmore text\nEOF\ngit push"),
            k(&["git commit", "git push"])
        );
        assert_eq!(keys("diff <(ls a) <(ls b)"), None);
        assert_eq!(keys("cat <<< \"rm x\""), k(&["cat"]));
    }

    #[test]
    fn subcommand_keys() {
        assert_eq!(keys("git -C /repo push origin main"), k(&["git push"]));
        assert_eq!(keys("git --no-pager log"), k(&["git log"]));
        assert_eq!(keys("cargo +nightly test"), k(&["cargo test"]));
        assert_eq!(keys("cargo -q build --release"), k(&["cargo build"]));
        assert_eq!(keys("make -j4 test"), k(&["make test"]));
        assert_eq!(keys("make"), k(&["make"]));
        assert_eq!(keys("rm -rf build"), k(&["rm"]));
        // An unknown flag before the subcommand might take a value.
        assert_eq!(keys("kubectl -n prod delete pod x"), None);
        assert_eq!(keys("docker --context=prod ps"), k(&["docker ps"]));
    }

    #[test]
    fn refusals() {
        for command in [
            "echo $(rm -rf x)",
            "echo `whoami`",
            "eval \"$CMD\"",
            "sudo rm -rf /",
            "git push && sudo reboot",
            "bash -c 'rm -rf x'",
            "sh -c 'git push'",
            "bash -lc 'ls'",
            "xargs rm < list",
            "env FOO=1 rm x",
            "nohup rm x",
            "$CMD arg",
            "find . -name '*.o' -delete",
            "find . -exec rm {} \\;",
            "ls; cat a; head b; tail c",
        ] {
            assert_eq!(keys(command), None, "{command}");
            assert_eq!(offer("bash", &bash(command), false), None, "{command}");
        }
        // Three keys is still fine; a shell never gets a rule.
        assert_eq!(keys("ls; cat a; head b"), k(&["cat", "head", "ls"]));
        assert_eq!(keys("bash script.sh"), None);
    }

    #[test]
    fn rule_ids_and_labels() {
        assert_eq!(
            rule("git push"),
            AllowRule {
                id: "bash:git push".into(),
                label: "pushes".into()
            }
        );
        assert_eq!(rule("git commit -m x").label, "commits");
        assert_eq!(rule("git add .").label, "staging");
        assert_eq!(rule("cargo test").label, "test runs");
        assert_eq!(rule("flutter build apk").label, "builds");
        assert_eq!(rule("rm -rf x").label, "deletions");
        assert_eq!(rule("make test").label, "make test");
        let multi = rule("git push && git add . && git commit -m x");
        assert_eq!(multi.id, "bash:git add,git commit,git push");
        assert_eq!(multi.label, "staging, commits and pushes");
        assert_eq!(rule("cargo test && flutter test").label, "test runs");
        assert_eq!(rule("git log | head").label, "git log and head");
        assert_eq!(offer("bash", &bash("echo hi"), false), None);
        assert_eq!(
            offer("change_files", &json!({"changes": []}), false),
            Some(AllowRule {
                id: "change_files".into(),
                label: "file edits".into()
            })
        );
        assert_eq!(
            offer("deploy", &json!({}), true),
            Some(AllowRule {
                id: "tool:deploy".into(),
                label: "deploy runs".into()
            })
        );
        assert_eq!(offer("web_fetch", &json!({}), false), None);
    }

    #[test]
    fn coverage_is_by_the_union_of_rules() {
        let mut rules = Vec::new();
        assert!(!covers(&rules, "bash", &bash("git push"), false));
        grant(&mut rules, rule("git push"));
        grant(&mut rules, rule("git push"));
        assert_eq!(rules.len(), 1);
        assert!(covers(&rules, "bash", &bash("git push origin main"), false));
        assert!(covers(&rules, "bash", &bash("cd repo && git push"), false));
        assert!(!covers(
            &rules,
            "bash",
            &bash("git push && rm -rf x"),
            false
        ));
        assert!(!covers(&rules, "bash", &bash("git push; sudo rm x"), false));
        assert!(!covers(&rules, "bash", &bash("echo hi"), false));
        grant(&mut rules, rule("rm x"));
        assert!(covers(&rules, "bash", &bash("git push && rm -rf x"), false));
        assert!(!covers(
            &rules,
            "bash",
            &bash("git push && rm $(cat list)"),
            false
        ));
        assert!(!covers(
            &rules,
            "bash",
            &bash("git push && git commit"),
            false
        ));
        assert_eq!(
            covering(&rules, "bash", &bash("git push"), false).as_deref(),
            Some("pushes")
        );
        assert_eq!(
            covering(&rules, "bash", &bash("rm x; git push"), false).as_deref(),
            Some("pushes and deletions")
        );
        // A multi-key rule grants each of its keys.
        let rules = vec![rule("git add . && git commit -m x")];
        assert!(covers(&rules, "bash", &bash("git commit --amend"), false));
        assert!(!covers(&rules, "change_files", &json!({}), false));
        let rules = vec![offer("change_files", &json!({}), false).unwrap()];
        assert!(covers(&rules, "change_files", &json!({}), false));
        assert!(!covers(&rules, "bash", &bash("ls"), false));
        let rules = vec![offer("deploy", &json!({}), true).unwrap()];
        assert!(covers(&rules, "deploy", &json!({}), true));
        assert!(!covers(&rules, "other", &json!({}), true));
    }

    #[test]
    fn title_and_reason() {
        let args = json!({"command": "git push", "label": "  Push the branch ", "reason": " So CI runs. "});
        assert_eq!(title("bash", &args).as_deref(), Some("Push the branch"));
        assert_eq!(reason(&args).as_deref(), Some("So CI runs."));
        assert_eq!(title("change_files", &args), None);
        assert_eq!(
            title("deploy", &json!({"title": "Deploy staging"})).as_deref(),
            Some("Deploy staging")
        );
        assert_eq!(reason(&json!({"reason": "   "})), None);
        let long = reason(&json!({"reason": "x".repeat(400)})).unwrap();
        assert_eq!(long.chars().count(), MAX_REASON_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn refuses_rule_stretching() {
        assert_eq!(keys("GIT_SSH_COMMAND='sh evil' git push"), None);
        assert_eq!(keys("LD_PRELOAD=/tmp/x.so git status"), None);
        assert_eq!(keys("PATH=.:$PATH git status"), None);
        assert_eq!(keys("RUST_LOG=debug cargo test"), k(&["cargo test"]));
        assert_eq!(keys("git -c core.sshCommand=evil push"), None);
        assert_eq!(keys("git push --receive-pack=evil origin"), None);
        assert_eq!(keys("cargo --config target.x.runner=evil test"), None);
        assert_eq!(keys("cargo test --config=x"), None);
        assert_eq!(keys("bash build.sh"), None);
        assert_eq!(keys("bash <<EOF\nrm -rf /\nEOF"), None);
        assert_eq!(keys("git status > ~/.bashrc"), None);
        assert_eq!(keys("git log > .git/hooks/pre-commit"), None);
        assert_eq!(keys("cargo test &> ../x"), None);
        assert_eq!(keys("cargo test > $HOME/x"), None);
        assert_eq!(keys("cargo test > /etc/x"), None);
        assert_eq!(keys("cargo test > target/log.txt 2>&1"), k(&["cargo test"]));
        assert_eq!(keys("cargo test &> /tmp/run/log"), k(&["cargo test"]));
        assert_eq!(keys("cargo test > /dev/null 2>&1"), k(&["cargo test"]));
        assert_eq!(keys("cargo test 2>/dev/null"), k(&["cargo test"]));
        assert_eq!(keys("grep x < input.txt"), k(&["grep"]));
    }

    #[test]
    fn serde_shape() {
        let rule = rule("git push");
        assert_eq!(
            serde_json::to_value(&rule).unwrap(),
            json!({"id": "bash:git push", "label": "pushes"})
        );
    }
}
