-- One-off repair of rows orphaned by finished tasks and stuck lane sessions.
-- Run on a copy first:  cp ~/.snippet/snippet.db /tmp/repair.db && sqlite3 /tmp/repair.db < scripts/repair-roster-2026-11-11.sql
-- Then against the live DB with the daemon stopped. Idempotent.
BEGIN;

UPDATE session_leases
SET released_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), release_reason = 'repair'
WHERE released_at IS NULL
  AND task_id IN (SELECT id FROM tasks WHERE status IN ('done', 'failed', 'cancelled'));

UPDATE task_agents
SET status = CASE
      WHEN task_id IN (SELECT id FROM tasks WHERE status = 'done') AND status IN ('active', 'offered') THEN 'done'
      ELSE 'withdrawn' END
WHERE removed_at IS NULL
  AND status IN ('offered', 'pending', 'waiting', 'active')
  AND task_id IN (SELECT id FROM tasks WHERE status IN ('done', 'failed', 'cancelled'));

UPDATE sessions
SET status = 'completed',
    state_json = CASE WHEN json_valid(state_json) THEN json_set(state_json, '$.status', 'completed') ELSE state_json END
WHERE status = 'running'
  AND (id LIKE 'lanes/lane-%' OR id LIKE '%/lanes/lane-%')
  AND COALESCE(last_active, 0) < CAST(strftime('%s', 'now') AS INTEGER) - 6 * 3600;

COMMIT;
