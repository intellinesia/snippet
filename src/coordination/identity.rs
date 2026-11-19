//! Durable agent identity homes, independent from execution workspaces.
use std::path::{Path, PathBuf};

const MAX_IDENTITY_BYTES: usize = 64 * 1024;
const HOME_GITIGNORE: &str =
    "/*\n!/.gitignore\n!/identity.md\n!/playbook.md\n!/tools/\n!/skills/\n";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid agent id: {0}")]
    InvalidAgentId(String),
    #[error("identity file is too large ({0} bytes; maximum is {MAX_IDENTITY_BYTES})")]
    TooLarge(usize),
    #[error("identity home: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("identity git: {0}")]
    Git(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHome {
    root: PathBuf,
    agent_id: String,
}

impl AgentHome {
    pub fn new(root: impl AsRef<Path>, agent_id: &str) -> Result<Self, IdentityError> {
        validate_agent_id(agent_id)?;
        Ok(Self {
            root: root.as_ref().join(agent_id),
            agent_id: agent_id.to_string(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn identity_path(&self) -> PathBuf {
        self.root.join("identity.md")
    }

    pub fn tools_path(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// Where this agent's watch scripts and per-watch state dirs live.
    pub fn watches_path(&self) -> PathBuf {
        self.root.join("watches")
    }

    /// Create the home if absent: the tools directory and `identity.md`.
    ///
    /// Idempotent — an existing identity is never overwritten, so a researched
    /// identity survives every daemon boot.
    pub fn ensure_layout(&self, default_identity: &str) -> Result<(), IdentityError> {
        std::fs::create_dir_all(self.tools_path())?;
        if !self.identity_path().exists() {
            atomic_write(&self.identity_path(), default_identity.as_bytes())?;
        }
        self.ensure_repo()?;
        Ok(())
    }

    /// Replace the identity (a build or rebuild). The home is created if absent.
    pub fn write_identity(&self, identity: &str) -> Result<(), IdentityError> {
        self.write_identity_with_reason(identity, "Update identity")
    }

    pub fn write_identity_with_reason(
        &self,
        identity: &str,
        reason: &str,
    ) -> Result<(), IdentityError> {
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::TooLarge(identity.len()));
        }
        std::fs::create_dir_all(self.tools_path())?;
        self.ensure_repo()?;
        atomic_write(&self.identity_path(), identity.as_bytes())?;
        self.commit(reason);
        Ok(())
    }

    fn ensure_repo(&self) -> Result<(), IdentityError> {
        if self.root.join(".git").exists() {
            return Ok(());
        }
        match git_output(&self.root, &["init", "-q"]) {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "agent home git init failed"
                );
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, %error, "agent home git init unavailable");
                return Ok(());
            }
        }
        if let Err(error) = atomic_write(&self.root.join(".gitignore"), HOME_GITIGNORE.as_bytes()) {
            tracing::warn!(agent_id = %self.agent_id, %error, "agent home .gitignore write failed");
            return Ok(());
        }
        self.commit("Start agent home");
        Ok(())
    }

    pub fn commit(&self, reason: &str) {
        if !self.root.join(".git").exists() {
            return;
        }
        match git_output(&self.root, &["add", "-A"]) {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "agent home git add failed"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, %error, "agent home git add unavailable");
                return;
            }
        }
        match git_output(&self.root, &["status", "--porcelain"]) {
            Ok(out) if out.status.success() => {
                if out.stdout.iter().all(u8::is_ascii_whitespace) {
                    return;
                }
            }
            Ok(out) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "agent home git status failed"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, %error, "agent home git status unavailable");
                return;
            }
        }
        match git_output(&self.root, &["commit", "-q", "-m", reason]) {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "agent home git commit failed"
                );
            }
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, %error, "agent home git commit unavailable");
            }
        }
    }

    pub fn revert_last_identity_change(&self) -> Result<String, IdentityError> {
        if !self.root.join(".git").exists() {
            return Err(IdentityError::Git(format!(
                "no git history for agent `{}`",
                self.agent_id
            )));
        }
        let hash_out = run_git_checked(
            &self.root,
            &["log", "-n", "1", "--format=%H", "--", "identity.md"],
        )?;
        let hash = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();
        if hash.is_empty() {
            return Err(IdentityError::Git(format!(
                "no identity changes to revert for agent `{}`",
                self.agent_id
            )));
        }
        let parent_ref = format!("{hash}^");
        let has_parent = git_output(
            &self.root,
            &["rev-parse", "--verify", "--quiet", &parent_ref],
        )
        .map_err(|error| IdentityError::Git(error.to_string()))?
        .status
        .success();
        if !has_parent {
            return Err(IdentityError::Git(format!(
                "nothing to revert for agent `{}`: only the initial commit exists",
                self.agent_id
            )));
        }
        let meta_out = run_git_checked(&self.root, &["show", "-s", "--format=%h%n%s", &hash])?;
        let meta = String::from_utf8_lossy(&meta_out.stdout);
        let mut lines = meta.lines();
        let short_hash = lines.next().unwrap_or(&hash).trim().to_string();
        let subject = lines.next().unwrap_or("").trim().to_string();

        let parent_blob = format!("{hash}^:identity.md");
        let parent_exists = git_output(&self.root, &["cat-file", "-e", &parent_blob])
            .map_err(|error| IdentityError::Git(error.to_string()))?
            .status
            .success();
        if parent_exists {
            let prev = run_git_checked(&self.root, &["show", &parent_blob])?;
            atomic_write(&self.identity_path(), &prev.stdout)?;
        } else if self.identity_path().exists() {
            std::fs::remove_file(self.identity_path())?;
        }

        let commit_msg = format!("Revert identity change {short_hash}: {subject}");
        self.commit(&commit_msg);
        Ok(format!("Reverted identity change {short_hash}: {subject}"))
    }

    pub fn read_identity(&self) -> Result<String, IdentityError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(self.identity_path())?
            .take(MAX_IDENTITY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::TooLarge(bytes.len()));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn validate_agent_id(agent_id: &str) -> Result<(), IdentityError> {
    if agent_id.is_empty()
        || agent_id == "."
        || agent_id == ".."
        || agent_id.len() > 96
        || !agent_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(IdentityError::InvalidAgentId(agent_id.to_string()));
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=snippet",
            "-c",
            "user.email=snippet@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(std::process::Stdio::null())
        .output()
}

fn run_git_checked(root: &Path, args: &[&str]) -> Result<std::process::Output, IdentityError> {
    let out = git_output(root, args).map_err(|error| IdentityError::Git(error.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(IdentityError::Git(if stderr.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or(""))
        } else {
            stderr
        }));
    }
    Ok(out)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_independent_agent_home_and_reads_identity() {
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "rust-reviewer").unwrap();
        home.ensure_layout("# Rust reviewer\n").unwrap();
        assert_eq!(home.read_identity().unwrap(), "# Rust reviewer\n");
        assert!(home.tools_path().is_dir());
    }

    #[test]
    fn rejects_path_traversal_and_unbounded_identity() {
        assert!(AgentHome::new("/tmp", "../escape").is_err());
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "worker").unwrap();
        home.ensure_layout("x").unwrap();
        std::fs::write(home.identity_path(), vec![b'x'; MAX_IDENTITY_BYTES + 1]).unwrap();
        assert!(matches!(
            home.read_identity(),
            Err(IdentityError::TooLarge(_))
        ));
    }

    #[test]
    fn identity_history_tracks_changes_reverts_and_ignores_memory() {
        if std::process::Command::new("git")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .map(|out| !out.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "historian").unwrap();
        std::fs::create_dir_all(home.root().join("memory")).unwrap();
        std::fs::write(home.root().join("memory").join("MEMORY.md"), "# mem\n").unwrap();

        home.write_identity("# First\n").unwrap();
        home.write_identity("# Second\n").unwrap();

        let reverted = home.revert_last_identity_change().unwrap();
        assert!(reverted.contains("Reverted identity change"));
        assert_eq!(home.read_identity().unwrap(), "# First\n");

        let tracked = git_output(home.root(), &["ls-files"]).unwrap();
        assert!(tracked.status.success());
        let tracked_files = String::from_utf8_lossy(&tracked.stdout);
        assert!(tracked_files.lines().any(|line| line == "identity.md"));
        assert!(
            !tracked_files
                .lines()
                .any(|line| line.starts_with("memory/"))
        );
    }
}
