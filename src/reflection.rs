use rusqlite::{OptionalExtension, params};

use crate::store::{Store, StoreError};

pub const DAILY_SECS: i64 = 24 * 3600;
pub const IDENTITY_BUDGET_BYTES: usize = 6 * 1024;
const MAX_LISTED: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionMark {
    pub last_board_id: i64,
    pub last_at: i64,
    pub identity_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardLine {
    pub id: i64,
    pub kind: String,
    pub summary: String,
}

pub fn ensure_schema(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_reflections (
             agent_id TEXT PRIMARY KEY NOT NULL,
             last_board_id INTEGER NOT NULL DEFAULT 0,
             last_at INTEGER NOT NULL DEFAULT 0,
             identity_commit TEXT
         );",
    )
}

impl Store {
    pub fn reflection_mark(&self, agent_id: &str) -> Result<Option<ReflectionMark>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT last_board_id, last_at, identity_commit FROM agent_reflections WHERE agent_id = ?1",
                params![agent_id],
                |row| {
                    Ok(ReflectionMark {
                        last_board_id: row.get(0)?,
                        last_at: row.get(1)?,
                        identity_commit: row.get(2)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn set_reflection_mark(
        &self,
        agent_id: &str,
        mark: &ReflectionMark,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_reflections (agent_id, last_board_id, last_at, identity_commit)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_id) DO UPDATE SET last_board_id = excluded.last_board_id,
                    last_at = excluded.last_at, identity_commit = excluded.identity_commit",
                params![
                    agent_id,
                    mark.last_board_id,
                    mark.last_at,
                    mark.identity_commit
                ],
            )?;
            Ok(())
        })
    }

    pub fn board_since(&self, agent_id: &str, after_id: i64) -> Result<Vec<BoardLine>, StoreError> {
        self.with_connection(|conn| {
            conn.prepare(
                "SELECT id, kind, summary FROM agent_board WHERE agent_id = ?1 AND id > ?2 ORDER BY id",
            )?
            .query_map(params![agent_id, after_id], |row| {
                Ok(BoardLine {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    summary: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()
        })
    }
}

pub fn is_job_end(line: &BoardLine) -> bool {
    line.kind == "reported"
        || (line.summary.starts_with("lane ")
            && [" completed:", " failed:"]
                .iter()
                .any(|w| line.summary.contains(w)))
}

pub fn due(lines: &[BoardLine], last_at: i64, now: i64) -> bool {
    !lines.is_empty() && (lines.iter().any(is_job_end) || now - last_at >= DAILY_SECS)
}

pub fn reflect_envelope(home: &std::path::Path, lines: &[BoardLine]) -> String {
    let jobs = lines.iter().filter(|l| is_job_end(l)).count();
    let mut listed = String::new();
    for line in lines.iter().rev().take(MAX_LISTED).rev() {
        let flat: String = line
            .summary
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect();
        listed.push_str(&format!("  #{} {}  {flat}\n", line.id, line.kind));
    }
    if lines.len() > MAX_LISTED {
        listed.push_str(&format!(
            "  and {} earlier (read.board)\n",
            lines.len() - MAX_LISTED
        ));
    }
    let home = home.display();
    format!(
        "[reflect]\nSince you last reflected: {} board entries, {jobs} finished jobs.\n{listed}[/reflect]\n\
         Look back at this work (read.board, read.session for detail) and keep what will make the next job go better: \
         facts and lessons in your memory; a procedure worth repeating as a skill at {home}/skills/<name>/SKILL.md, \
         which every agent can use at once; how you run recurring work in a playbook in your agent memory ({home}/memory/playbooks/<job>.md); and only when your role \
         itself should change, {home}/identity.md (under 6 KB; the user is told and can revert). Commit what you change \
         with git in the repo it lives in, the message saying why. If nothing was learned, change nothing. Then stop.",
        lines.len()
    )
}

pub fn identity_notice(agent_id: &str, commit: &str, subject: &str, size: usize) -> String {
    let over = if size > IDENTITY_BUDGET_BYTES {
        format!(
            " It is {:.1} KB, over the 6 KB budget.",
            size as f64 / 1024.0
        )
    } else {
        String::new()
    };
    format!(
        "I updated my identity ({commit}: {subject}).{over} `snippet agent revert {agent_id}` undoes it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: i64, kind: &str, summary: &str) -> BoardLine {
        BoardLine {
            id,
            kind: kind.into(),
            summary: summary.into(),
        }
    }

    #[test]
    fn due_after_a_job_end_or_a_day_with_new_entries() {
        let progress = vec![line(1, "progress", "step two done")];
        assert!(!due(&progress, 100, 200));
        assert!(due(&progress, 100, 100 + DAILY_SECS));
        assert!(!due(&[], 0, DAILY_SECS * 10));
        assert!(due(&[line(2, "reported", "t1 — done: x")], 100, 101));
        assert!(due(
            &[line(3, "progress", "lane l-1 completed: fix — ok")],
            100,
            101
        ));
        assert!(!due(
            &[line(4, "progress", "lane l-1 started: fix")],
            100,
            101
        ));
    }
}
