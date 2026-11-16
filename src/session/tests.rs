#[cfg(test)]
mod fork_tests {
    use crate::session::*;

    use crate::harness::{HarnessEvent, HarnessState, HarnessStatus};
    use crate::llm::HarnessMessage;

    fn sample_state() -> HarnessState {
        // Build via JSON so private migration fields stay internal.
        let mut s: HarnessState = serde_json::from_value(serde_json::json!({
            "version": 1,
            "status": "idle",
            "created_at": "t0",
            "updated_at": "t0",
            "workspace": "/tmp/ws",
            "title": "original title",
            "messages": [],
            "events": [],
            "iterations": 3,
            "total_tokens": 100,
            "prompt_tokens": 80,
            "completion_tokens": 20,
            "last_prompt_tokens": 50,
            "context_window": 128000
        }))
        .expect("sample state");
        s.messages = vec![
            HarnessMessage::User {
                content: "hi".into(),
            },
            HarnessMessage::Assistant {
                content: "hello".into(),
                tool_calls: Vec::new(),
            },
            HarnessMessage::User {
                content: "again".into(),
            },
        ];
        s.events = vec![
            HarnessEvent::UserInput { text: "hi".into() },
            HarnessEvent::AssistantText {
                text: "hello".into(),
                quiet: false,
            },
            HarnessEvent::UserInput {
                text: "again".into(),
            },
        ];
        s.checkpoints = vec![crate::harness::CheckpointRecord {
            id: "abc12345deadbeef".into(),
            label: "hi".into(),
            created_at: "t0".into(),
            event_index: 0,
            message_index: 0,
            compactions: 0,
        }];
        s
    }

    #[test]
    fn resolve_checkpoint_cut() {
        let s = sample_state();
        let p = resolve_fork_point(&s, Some("abc12345"), None).unwrap();
        assert_eq!(p.event_end, 0);
        assert_eq!(p.message_end, 0);
    }

    #[test]
    fn resolve_event_index_inclusive() {
        let s = sample_state();
        let p = resolve_fork_point(&s, None, Some(1)).unwrap();
        assert_eq!(p.event_end, 2); // keep through index 1
    }

    #[test]
    fn build_fork_truncates_and_idles() {
        let s = sample_state();
        let p = ForkPoint {
            event_end: 2,
            message_end: 2,
        };
        let f = build_forked_state(&s, p);
        assert_eq!(f.events.len(), 2);
        assert_eq!(f.messages.len(), 2);
        assert_eq!(f.status, HarnessStatus::Idle);
        assert!(f.lanes.is_empty());
        assert!(f.title.as_deref().unwrap_or("").starts_with("fork ·"));
        assert_eq!(f.total_tokens, 0);
    }

    #[test]
    fn snaps_orphan_tool_call_at_end() {
        let mut s = sample_state();
        s.events.push(HarnessEvent::ToolCall {
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        });
        s.messages.push(HarnessMessage::Assistant {
            content: String::new(),
            tool_calls: Vec::new(),
        });
        let last = s.events.len() - 1;
        let p = resolve_fork_point(&s, None, Some(last)).unwrap();
        // Exclusive end must not leave a trailing ToolCall.
        if p.event_end > 0 {
            assert!(!matches!(
                s.events[p.event_end - 1],
                HarnessEvent::ToolCall { .. }
            ));
        }
    }

    #[test]
    fn a_fork_is_written_to_the_store() {
        // The bug this guards: fork wrote only a file, so the branch got no store
        // row — invisible in the list and unopenable, since reads are store-only.
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let s = sample_state();
        let p = ForkPoint {
            event_end: 2,
            message_end: 2,
        };
        let dir = std::env::temp_dir().join(format!("snippet-fork-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        let source = dir.join("conversations").join("source.json");

        let fork = write_forked_conversation_in(&store, &source, &s, p).unwrap();

        let row = store
            .get_session_row(&fork.id)
            .unwrap()
            .expect("the branch must have a store row");
        assert_eq!(row.title.as_deref(), Some(fork.title.as_str()));
        assert!(row.last_active.is_some(), "a new branch sorts to the top");
        // The truncated transcript, not the whole conversation.
        assert_eq!(store.conversation_message_count(&fork.id).unwrap(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
#[cfg(test)]
mod create_blank_tests {
    use crate::session::*;
    use crate::store::Store;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_folder(stamp: u128, suffix: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("snippet-mc-{suffix}-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn create_blank_session_writes_a_store_row() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = temp_folder(stamp, "blank");
        let store = Store::open_in_memory().unwrap();

        let info =
            create_blank_session_in(&store, &folder, "Odd request", true, WorkspaceMode::Folder)
                .unwrap();
        assert_eq!(info.title, "Odd request");
        assert_eq!(info.status, "idle");
        assert_eq!(
            info.folder,
            folder.canonicalize().unwrap().display().to_string()
        );

        // No state file: the row IS the session.
        assert!(!state_path_for_id(&info.id).unwrap().exists());
        let row = store
            .get_session_row(&info.id)
            .unwrap()
            .expect("row exists");
        assert_eq!(row.title.as_deref(), Some("Odd request"));
        assert_eq!(row.status, "idle");
        assert!(row.last_active.is_some(), "a new chat sorts to the top");

        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_second_default_session_in_the_same_folder_is_refused() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = temp_folder(stamp, "dup");
        let store = Store::open_in_memory().unwrap();

        create_blank_session_in(&store, &folder, "first", false, WorkspaceMode::Folder).unwrap();
        let second =
            create_blank_session_in(&store, &folder, "second", false, WorkspaceMode::Folder);
        assert!(second.is_err(), "uniqueness is the store's question now");

        let _ = fs::remove_dir_all(&folder);
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(stamp: u128, suffix: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!("snippet-wt-{suffix}-{stamp}"));
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q"]);
        git_ok(&repo, &["config", "user.email", "snippet@test"]);
        git_ok(&repo, &["config", "user.name", "snippet"]);
        fs::write(repo.join("README"), "hi\n").unwrap();
        git_ok(&repo, &["add", "README"]);
        git_ok(&repo, &["commit", "-qm", "init"]);
        repo.canonicalize().unwrap()
    }

    fn drop_worktree(repo: &Path, workspace: &Path) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(workspace)
            .status();
        if workspace.exists() {
            let _ = fs::remove_dir_all(workspace);
        }
    }

    #[test]
    fn new_session_in_git_repo_uses_isolated_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "repo");
        let workspace = prepare_new_session_workspace(&repo);
        let root = crate::config::worktrees_root();
        assert_ne!(workspace, repo);
        assert!(workspace.starts_with(&root));
        assert!(workspace.join(".git").is_file());
        assert!(workspace.join("README").exists());
        let branch = git_stdout(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert!(
            branch.starts_with("snippet/"),
            "session worktree should be on snippet/{{id}}, got {branch}"
        );
        assert_ne!(branch, "HEAD", "must not be detached");
        drop_worktree(&repo, &workspace);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_git_folder_stays_put() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("snippet-wt-plain-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        let got = prepare_new_session_workspace(&folder);
        assert_eq!(got, folder);
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn parallel_sessions_get_unique_worktrees() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "parallel");
        let a = prepare_new_session_workspace(&repo);
        let b = prepare_new_session_workspace(&repo);
        let root = crate::config::worktrees_root();
        assert_ne!(a, b);
        assert!(a.starts_with(&root) && b.starts_with(&root));
        assert!(a.join("README").exists() && b.join("README").exists());
        drop_worktree(&repo, &a);
        drop_worktree(&repo, &b);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_worktree_blank_session_is_isolated_and_cleaned_up() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "mc-blank");
        let store = Store::open_in_memory().unwrap();
        let info =
            create_blank_session_in(&store, &repo, "from mc", false, WorkspaceMode::Worktree)
                .unwrap();
        let root = crate::config::worktrees_root();
        let folder = PathBuf::from(&info.folder);
        assert_ne!(folder, repo);
        assert!(
            folder.starts_with(&root),
            "expected isolated worktree, got {}",
            folder.display()
        );
        assert!(folder.join(".git").is_file());
        let path = state_path_for_id(&info.id).expect("created session is resolvable");
        remove_session_files_with(Some(&store), &path);
        assert!(!folder.exists(), "isolated worktree should be gone");
        assert!(store.get_session_row(&info.id).unwrap().is_none());
        assert!(repo.exists(), "original clone must stay");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn folder_mode_works_in_the_repo_itself() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "folder-mode");
        let store = Store::open_in_memory().unwrap();
        let info =
            create_blank_session_in(&store, &repo, "here", false, WorkspaceMode::Folder).unwrap();
        assert_eq!(PathBuf::from(&info.folder), repo);
        assert_eq!(info.origin_folder, None, "the repo is not a worktree");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_worktree_session_knows_its_origin_and_branch() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "origin");
        fs::create_dir_all(repo.join("app")).unwrap();
        fs::write(repo.join("app/main.rs"), "fn main() {}\n").unwrap();
        git_ok(&repo, &["add", "app"]);
        git_ok(&repo, &["commit", "-qm", "app"]);

        let workspace = session_workspace(&repo.join("app"), WorkspaceMode::Worktree);
        assert!(workspace.ends_with("app"));
        let origin = worktree_origin(&workspace).expect("a linked worktree");
        assert_eq!(origin.folder, repo.join("app"));
        let branch = origin.branch.expect("a branch");
        assert!(branch.starts_with("snippet/"), "got {branch}");

        let (main, linked) = repo_worktrees(&repo).expect("a repo");
        assert_eq!(main, repo);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].branch.as_deref(), Some(branch.as_str()));

        drop_worktree(&repo, workspace.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_shared_worktree_outlives_one_of_its_sessions() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "shared");
        let workspace = session_workspace(&repo, WorkspaceMode::Worktree);
        let store = Store::open_in_memory().unwrap();
        let first = create_blank_session_in(&store, &workspace, "one", true, WorkspaceMode::Folder)
            .unwrap();
        let second =
            create_blank_session_in(&store, &workspace, "two", true, WorkspaceMode::Folder)
                .unwrap();

        remove_session_files_with(Some(&store), &state_path_for_id(&first.id).unwrap());
        assert!(workspace.exists(), "the other session still works here");
        remove_session_files_with(Some(&store), &state_path_for_id(&second.id).unwrap());
        assert!(
            !workspace.exists(),
            "the last session takes the worktree with it"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_persons_own_worktree_is_never_deleted() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "own");
        let mine = repo.with_file_name(format!("snippet-wt-mine-{stamp}"));
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                mine.to_str().unwrap(),
            ],
        );
        let store = Store::open_in_memory().unwrap();
        let info =
            create_blank_session_in(&store, &mine, "mine", false, WorkspaceMode::Folder).unwrap();
        remove_session_files_with(Some(&store), &state_path_for_id(&info.id).unwrap());
        assert!(mine.exists(), "a worktree snippet did not make stays");
        drop_worktree(&repo, &mine);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn deleting_a_session_drops_its_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "drop");
        let workspace = prepare_new_session_workspace(&repo);
        assert!(workspace.exists());
        let store = Store::open_in_memory().unwrap();
        let info =
            create_blank_session_in(&store, &workspace, "wt-drop", false, WorkspaceMode::Folder)
                .unwrap();
        let path = state_path_for_id(&info.id).expect("created session is resolvable");
        remove_session_files_with(Some(&store), &path);
        assert!(!workspace.exists(), "isolated worktree should be gone");
        assert!(repo.exists(), "original clone must stay");
        let _ = fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
mod resolve_session_path_tests {
    use crate::session::*;

    use std::fs;

    #[test]
    fn a_workspace_directory_is_the_default_session() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws-1");
        fs::create_dir_all(&ws).unwrap();

        let bare = resolve_session_path(root.path(), "ws-1").expect("resolves");
        assert_eq!(bare, ws, "got {}", bare.display());
    }

    #[test]
    fn the_full_id_resolves_to_itself() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws-2");
        fs::create_dir_all(&ws).unwrap();

        let resolved = resolve_session_path(root.path(), "ws-2").expect("resolves");
        assert!(resolved.ends_with("ws-2"), "got {}", resolved.display());
    }

    /// A saved conversation is not a directory, so the completion must not touch
    /// it. Regression guard: completing every id would rewrite this path.
    #[test]
    fn a_saved_conversation_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let conv = root.path().join("ws-3/conversations");
        fs::create_dir_all(&conv).unwrap();
        let file = conv.join("abc.json");
        fs::write(&file, b"{}").unwrap();

        let resolved =
            resolve_session_path(root.path(), "ws-3/conversations/abc.json").expect("resolves");
        assert!(
            resolved.ends_with("ws-3/conversations/abc.json"),
            "got {}",
            resolved.display()
        );
    }

    #[test]
    fn traversal_is_refused() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_session_path(root.path(), "../escape").is_none());
        assert!(resolve_session_path(root.path(), "/abs/path").is_none());
    }
}

#[cfg(test)]
mod conversation_name_tests {
    use crate::session::conversation_name_from_id;

    /// A session under `conversations/` is a saved conversation; its name is the
    /// file stem.
    #[test]
    fn a_saved_conversation_is_named_by_its_file_stem() {
        assert_eq!(
            conversation_name_from_id("ws-1/conversations/41e5e18b-5478-4737-863f-750de39e025d"),
            "41e5e18b-5478-4737-863f-750de39e025d"
        );
        // The pre-canonical form (with `.json`) named the same conversation.
        assert_eq!(
            conversation_name_from_id("ws-1/conversations/41e5e18b.json"),
            "41e5e18b"
        );
    }

    /// Everything else is the workspace's one default session.
    #[test]
    fn a_root_session_is_the_default_conversation() {
        assert_eq!(
            conversation_name_from_id("snippet-service-61c2d836aee8dc5b"),
            "default"
        );
        assert_eq!(conversation_name_from_id("inbox-snippet"), "default");
        assert_eq!(conversation_name_from_id("mission-control"), "default");
    }
}

#[cfg(test)]
mod routable_target_tests {
    use crate::session::is_routable_target;

    /// An ordinary project session is the only thing work routes to.
    #[test]
    fn a_project_session_is_routable() {
        assert!(is_routable_target("snippet-service-61c2d836aee8dc5b"));
        assert!(is_routable_target(
            "wacht-480461c289235d72/conversations/e000736e-da39-4bd0-a307-52f52fc71241"
        ));
    }

    #[test]
    fn an_agent_inbox_is_not_routable() {
        assert!(!is_routable_target("inbox-snippet"));
        assert!(!is_routable_target("inbox-rust-pr-reviewer"));
    }

    /// Mission Control coordinates; routing work to itself is a loop.
    #[test]
    fn mission_control_is_not_routable() {
        assert!(!is_routable_target("mission-control"));
        assert!(!is_routable_target("mission-control/session.json"));
    }

    /// The two exclusions must not swallow a workspace that merely starts with
    /// the same letters — `inboxing-app-1234` is a real project folder.
    #[test]
    fn a_folder_named_like_an_inbox_stays_routable() {
        assert!(is_routable_target("inboxing-app-1234abcd"));
        assert!(is_routable_target("mission-control-ui-5678ef90"));
    }
}

#[cfg(test)]
mod working_agents_tests {
    use crate::coordination::{HandoffMode, Task, TaskStatus};
    use crate::session::session_task_contexts;
    use crate::store::Store;

    /// The roster row has a real FK to `agents(id)`, so the agent must exist.
    fn worker(id: &str) -> crate::coordination::types::Agent {
        crate::coordination::types::Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn dispatched(id: &str, session: &str, now: &str) -> Task {
        let mut task = Task::dispatched_to(
            id.into(),
            session.into(),
            format!("task {id}"),
            "scope".into(),
            vec![],
            HandoffMode::Resume,
            "agent",
            crate::mission_control::SESSION_ID,
            now.into(),
        );
        task.profile = None;
        task
    }

    /// A plain project session is nobody's — `sessions.agent_id` is NULL — yet an
    /// agent dispatched into it is exactly who the session list must name. This
    /// is the case the badge missed: it read the session's own binding, which
    /// dispatch never writes.
    #[test]
    fn a_dispatched_agent_is_reported_for_its_target_session() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        let map = session_task_contexts(&db).workers;
        assert_eq!(
            map.get("proj-1").map(String::as_str),
            Some("snippet"),
            "the dispatched agent must be reported for the target session"
        );
    }

    /// Finished work is not someone working. A terminal task must drop out, or
    /// every session an agent ever touched would keep claiming a worker.
    #[test]
    fn a_finished_task_stops_reporting_a_worker() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();
        assert!(session_task_contexts(&db).workers.contains_key("proj-1"));

        for terminal in [TaskStatus::Done, TaskStatus::Failed, TaskStatus::Cancelled] {
            db.update_task_in("t1", "2026-01-02T00:00:00Z", |t| {
                t.status = terminal.clone()
            })
            .unwrap();
            assert!(
                !session_task_contexts(&db).workers.contains_key("proj-1"),
                "a {terminal:?} task must not report a worker"
            );
        }
    }

    /// The task row matches its session key.
    #[test]
    fn a_target_matches_its_session() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        assert_eq!(
            session_task_contexts(&db)
                .workers
                .get("proj-1")
                .map(String::as_str),
            Some("snippet"),
            "a target must key to the canonical session id"
        );
    }

    /// A task with no target cannot be delivered, so it must not claim a worker.
    #[test]
    fn a_targetless_task_reports_nothing() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        assert!(session_task_contexts(&db).workers.is_empty());
    }
}

#[cfg(test)]
mod inbox_tool_tests {
    /// An inbox files work through Mission Control; it never writes or delegates.
    #[test]
    fn an_inbox_cannot_write_delegate_or_report() {
        let tools = crate::session::coordination_tool_registry(None);
        for name in [
            "change_files",
            "write_file",
            "edit_file",
            "delegate_task",
            "report_mission_task",
        ] {
            assert!(!tools.contains(name), "{name} must not reach an inbox");
        }
        assert!(tools.contains("bash"));
        assert!(tools.contains("send_agent_message"));
    }
}

#[cfg(test)]
mod session_row_d2_tests {
    use super::super::*;
    use crate::conversations::SessionExtras;
    use crate::harness::{HarnessEvent, HarnessState, PlanStatus, PlanStep};
    use crate::lanes::{LaneRecord, LaneStatus};
    use crate::store::Store;

    fn step(text: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            step: text.to_string(),
            status,
        }
    }

    fn lane(
        id: &str,
        title: &str,
        status: LaneStatus,
        agent: Option<&str>,
        activity: Option<&str>,
    ) -> LaneRecord {
        LaneRecord {
            id: id.to_string(),
            title: title.to_string(),
            status,
            handoff: None,
            summary: None,
            report: None,
            structured: None,
            error: None,
            started_at: "2026-11-16T00:00:00Z".to_string(),
            finished_at: None,
            activity: activity.map(str::to_string),
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only: false,
            agent: agent.map(str::to_string),
            profile: None,
            transcript: None,
            paths: Vec::new(),
        }
    }

    #[test]
    fn compute_session_plan_tracks_when_current_step_started() {
        let e_old = (
            vec![
                step("Step 1", PlanStatus::InProgress),
                step("Step 2", PlanStatus::Pending),
            ],
            Some(100),
        );
        let e_mid = (
            vec![
                step("Step 1", PlanStatus::Done),
                step("Step 2", PlanStatus::InProgress),
            ],
            Some(200),
        );
        let e_new = (
            vec![
                step("Step 1", PlanStatus::Done),
                step("Step 2", PlanStatus::InProgress),
                step("Step 3", PlanStatus::Pending),
            ],
            Some(300),
        );
        // Descending order (latest first).
        let plan = compute_session_plan(&[e_new, e_mid, e_old]).unwrap();
        assert_eq!(plan.done, 1);
        assert_eq!(plan.total, 3);
        assert_eq!(plan.current.as_deref(), Some("Step 2"));
        assert_eq!(plan.step_started_at, Some(200));

        assert_eq!(compute_session_plan(&[]), None);
        assert_eq!(compute_session_plan(&[(Vec::new(), Some(100))]), None);
    }

    #[test]
    fn all_device_sessions_with_populates_plan_lanes_and_next_wake_at() {
        let store = Store::open_in_memory().unwrap();
        let mut state = HarnessState::blank("/tmp/proj", Some("Build D2".into()));
        state.lanes = vec![
            lane(
                "l1",
                "First",
                LaneStatus::Running,
                Some("reviewer"),
                Some("Reading diffs"),
            ),
            lane("l2", "Done", LaneStatus::Completed, None, Some("Finished")),
            lane(
                "l3",
                "Second",
                LaneStatus::Running,
                None,
                Some("Running tests"),
            ),
            lane("l4", "Third", LaneStatus::Running, Some("coder"), None),
            lane("l5", "Fourth", LaneStatus::Running, None, Some("Extra")),
        ];
        store
            .import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        store
            .append_conversation_events(
                "s1",
                &[HarnessEvent::PlanUpdated {
                    steps: vec![
                        step("A", PlanStatus::Done),
                        step("B", PlanStatus::InProgress),
                        step("C", PlanStatus::Pending),
                    ],
                    explanation: None,
                }],
                "2026-11-16T01:00:00Z",
            )
            .unwrap();
        store
            .append_conversation_events(
                "s1",
                &[HarnessEvent::PlanUpdated {
                    steps: vec![
                        step("A", PlanStatus::Done),
                        step("B", PlanStatus::InProgress),
                        step("C", PlanStatus::Pending),
                        step("D", PlanStatus::Pending),
                    ],
                    explanation: None,
                }],
                "2026-11-16T01:05:00Z",
            )
            .unwrap();
        store
            .schedule_followup(
                "s1",
                None,
                "2026-11-16T03:00:00Z",
                "later",
                "2026-11-16T01:00:00Z",
            )
            .unwrap();
        store
            .schedule_followup(
                "s1",
                None,
                "2026-11-16T02:00:00Z",
                "sooner",
                "2026-11-16T01:00:00Z",
            )
            .unwrap();
        let delivered = store
            .schedule_followup(
                "s1",
                None,
                "2026-11-16T01:30:00Z",
                "old",
                "2026-11-16T01:00:00Z",
            )
            .unwrap();
        store
            .mark_followup_delivered(&delivered.id, "2026-11-16T01:31:00Z", None)
            .unwrap();

        let empty = HarnessState::blank("/tmp/proj", Some("Empty".into()));
        store
            .import_session("s2", "wk1", &empty, &SessionExtras::default())
            .unwrap();

        let sessions = all_device_sessions_with(Some(&store));
        let s1 = sessions.iter().find(|s| s.id == "s1").unwrap();
        let plan = s1.plan.as_ref().unwrap();
        assert_eq!(plan.done, 1);
        assert_eq!(plan.total, 4);
        assert_eq!(plan.current.as_deref(), Some("B"));
        assert_eq!(
            plan.step_started_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-16T01:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );

        let lanes = s1.lanes.as_ref().unwrap();
        assert_eq!(lanes.running, 4);
        assert_eq!(lanes.items.len(), 3);
        assert_eq!(lanes.items[0].id, "l1");
        assert_eq!(lanes.items[0].agent.as_deref(), Some("reviewer"));
        assert_eq!(lanes.items[0].activity.as_deref(), Some("Reading diffs"));
        assert_eq!(lanes.items[1].id, "l3");
        assert_eq!(lanes.items[2].id, "l4");

        assert_eq!(
            s1.next_wake_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-16T02:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );

        let s2 = sessions.iter().find(|s| s.id == "s2").unwrap();
        assert!(s2.plan.is_none());
        assert!(s2.lanes.is_none());
        assert!(s2.next_wake_at.is_none());
        assert!(s2.pr.is_none());
    }
}

#[cfg(test)]
mod owner_and_came_back_tests {
    use crate::conversations::SessionRow;
    use crate::harness::HarnessEvent;
    use crate::session::*;
    use crate::store::Store;

    fn session_row(id: &str, status: &str, agent_id: Option<&str>) -> SessionRow {
        SessionRow {
            id: id.into(),
            workspace_key: "wk".into(),
            workspace: "/tmp/ws".into(),
            title: Some("Test".into()),
            status: status.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_active: Some(10),
            profile: None,
            role: if id == crate::mission_control::SESSION_ID {
                "mission_control".into()
            } else {
                "standard".into()
            },
            agent_id: agent_id.map(str::to_string),
            kind: if is_inbox_session_id(id) {
                "inbox".into()
            } else {
                "conversation".into()
            },
            settled_at: None,
            keep_awake: false,
            resume_mode: "ask".into(),
            last_changes: None,
            total_changes: None,
            user_seen_at: None,
            finished_at: None,
            result_summary: None,
            pending_handoff: None,
        }
    }

    #[test]
    fn owner_rule_covers_default_bound_seated_mc_and_desks() {
        let db = Store::open_in_memory().unwrap();
        db.create_conversation(&session_row("plain", "idle", None), &[], "[]")
            .unwrap();
        db.create_conversation(&session_row("spec", "idle", Some("rook")), &[], "[]")
            .unwrap();
        db.set_session_role("spec", "standard", Some("rook"))
            .unwrap();
        db.create_conversation(
            &session_row(crate::mission_control::SESSION_ID, "idle", None),
            &[],
            "[]",
        )
        .unwrap();
        db.set_session_role(crate::mission_control::SESSION_ID, "mission_control", None)
            .unwrap();
        db.create_conversation(&session_row("inbox-scout", "idle", None), &[], "[]")
            .unwrap();

        // Seated collaborator session with no stored agent_id inherits seated agent.
        db.create_conversation(&session_row("seat-1", "idle", None), &[], "[]")
            .unwrap();
        let task = crate::coordination::Task::dispatched_to(
            "t-seat".into(),
            "plain".into(),
            "Task".into(),
            "scope".into(),
            vec![],
            crate::coordination::HandoffMode::Resume,
            "human",
            "human",
            "2026-01-01T00:00:00Z".into(),
        );
        db.create_task(&task).unwrap();
        db.create_agent(&crate::coordination::types::Agent {
            id: "auditor".into(),
            display_name: "Auditor".into(),
            handle: "auditor".into(),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            capabilities: vec![],
        })
        .unwrap();
        db.add_task_agent("t-seat", "auditor", "reviewer", "2026-01-01T00:00:00Z")
            .unwrap();
        db.set_member_session("t-seat", "auditor", "seat-1")
            .unwrap();

        let by_id: std::collections::HashMap<String, SessionInfo> = all_device_sessions_in(&db)
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        assert_eq!(by_id["plain"].owner_agent_id, "snippet");
        assert_eq!(by_id["spec"].owner_agent_id, "rook");
        assert_eq!(
            by_id[crate::mission_control::SESSION_ID].owner_agent_id,
            "mission-control"
        );
        assert_eq!(by_id["inbox-scout"].owner_agent_id, "scout");
        assert_eq!(by_id["seat-1"].owner_agent_id, "auditor");
    }

    #[test]
    fn came_back_set_by_finish_and_cleared_by_seen() {
        let db = Store::open_in_memory().unwrap();
        db.create_conversation(&session_row("s1", "idle", None), &[], "[]")
            .unwrap();
        db.append_conversation_events(
            "s1",
            &[HarnessEvent::AssistantText {
                text: format!("\n  {}\nsecond line", "x".repeat(250)),
                quiet: false,
            }],
            "t1",
        )
        .unwrap();

        let (tx, _) = tokio::sync::broadcast::channel(8);
        emit_device_event_with_store(
            serde_json::json!({ "kind": "idle", "session": "s1" }),
            Some(&db),
            &tx,
        );

        let list = all_device_sessions_in(&db);
        let info = list.iter().find(|s| s.id == "s1").unwrap();
        assert!(info.finished_at.is_some());
        assert_eq!(info.result_summary.as_deref().unwrap().chars().count(), 200);
        assert!(info.came_back);

        // Waiting or running never shows as came back.
        assert!(!compute_came_back(
            "s1",
            "waiting_for_input",
            info.finished_at,
            None
        ));
        assert!(!compute_came_back("s1", "running", info.finished_at, None));
        assert!(!compute_came_back(
            crate::mission_control::SESSION_ID,
            "idle",
            info.finished_at,
            None
        ));
        assert!(!compute_came_back(
            "inbox-rook",
            "idle",
            info.finished_at,
            None
        ));

        // Seen at or after finished_at clears came_back.
        db.set_session_user_seen("s1", info.finished_at.unwrap())
            .unwrap();
        let list = all_device_sessions_in(&db);
        let info = list.iter().find(|s| s.id == "s1").unwrap();
        assert!(!info.came_back);
    }
}

#[test]
fn session_goals_lists_only_ongoing_goals() {
    let store = crate::store::Store::open_in_memory().unwrap();
    let save = |id: &str, scalar: &str| {
        store
            .save_session_scalar(
                id,
                id,
                "/w",
                Some("t"),
                "idle",
                scalar,
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
    };
    save(
        "a",
        r#"{"goal":{"text":"ship","dir":"d","status":"active","autonomous_turns":3,"resume_at":0}}"#,
    );
    save(
        "p",
        r#"{"goal":{"text":"wait","status":"paused","resume_at":99}}"#,
    );
    save("c", r#"{"goal":{"text":"done","status":"complete"}}"#);
    save("x", r#"{"goal":{"text":"no","status":"cancelled"}}"#);
    save("n", r#"{"goal":null}"#);
    let goals = store.session_goals();
    let mut ids: Vec<_> = goals.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, ["a", "p"]);
    assert_eq!(goals["a"].autonomous_turns, 3);
    assert_eq!(goals["p"].resume_at, 99);
    let json = serde_json::to_value(&goals["a"]).unwrap();
    assert_eq!(json["status"], "active");
}
