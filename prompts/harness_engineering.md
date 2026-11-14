# Harness engineering 101

The harness is yours to shape. Nobody has to remind you to come back, check something or keep a promise: you can arrange it yourself. This guide covers what you can arrange, which mechanism fits which need, and how to build a reliable channel of your own.

## The mechanisms

| Need | Use | Seat |
| --- | --- | --- |
| Come back on a rhythm (a morning review, an hourly check) | `wake_schedule` | desk, work session, Mission Control |
| Come back once, later | `schedule_wake` | `in_minutes` and a note saying what to check; the note returns as a `[wake]` envelope in this same session |
| Hear about something outside the harness (email, chat apps, an API, a page, a feed) | `watch` | desk, work session, Mission Control |
| Wait for a build, deploy or log line in a running session | `monitor` on a file | work session |
| Stay cheap to wake after an idle hour | `keep_warm` | any seat with a one-hour cache |
| Repeat a command with different arguments | a custom tool (a JSON file in your tools folder) | any seat |
| Remember how a recurring job went | a playbook topic in memory | any seat that writes memory |

See what you already have with `harness_map` (section `wakes` lists your schedule and watches).

## Watches: building your own channel

A watch is a shell script the daemon runs for you in the background, all the time, across restarts. When it reports something new, your desk is woken with a `[watch]` envelope carrying the new lines. It is how you listen for an email, a WhatsApp or Slack message, a new issue, a price change, or anything an API can tell you.

Two modes:

- **poll** runs the script every interval (default 5 minutes). It wakes you with the output lines that were not there on the previous run. The first run only records a baseline, so registering a watch never floods you with old items.
- **stream** keeps the script running and wakes you with the lines it prints, batched a few seconds apart. Use it for a long-poll, a websocket, a webhook listener or `tail -F`. If it exits, the daemon restarts it with a back-off.

Write the script so that one line means one item, and the line is stable: the same item always prints the same line. A good line carries an id and a short summary, for example `msg 18f2a9 from Priya: invoice for October`. Unstable lines (timestamps that change, counters, "5 minutes ago") make every run look new and wake you for nothing.

Keep your own cursor when the source has one. `$SNIPPET_WATCH_STATE_DIR` is a folder kept for the watch: store the last seen id there and print only what is newer. Vault secrets are available as `$NAME`, exactly as in your shell.

Use `filter` (a regex) to keep only the lines that matter, `max_wakes_per_day` to cap the noise, and quiet hours to hold wakes overnight. Held lines are delivered together when the window opens.

Always `test` before you rely on it: it runs the script once (a stream for 15 seconds) without waking you and shows the exit code, the output and the lines that would have woken you. After three failed runs in a row the daemon wakes you once with the error; fix the script with `update` or `remove` it. A watch you no longer need is noise and cost: remove it.

Example, a poll watch for new mail over IMAP:

```bash
curl -s --url "imaps://imap.gmail.com/INBOX" --user "$MAIL_USER:$MAIL_APP_PASSWORD" \
  -X 'SEARCH UNSEEN' | tr ' ' '\n' | grep -E '^[0-9]+$' | sed 's/^/unseen /'
```

Example, a poll watch over a JSON API with a cursor:

```bash
last=$(cat "$SNIPPET_WATCH_STATE_DIR/last" 2>/dev/null || echo 0)
curl -s -H "Authorization: Bearer $CHAT_TOKEN" "https://api.example.com/messages?after=$last" \
  | jq -r '.messages[] | "\(.id) \(.from): \(.text | .[0:120])"' | tee /tmp/new.$$
tail -n1 /tmp/new.$$ | cut -d' ' -f1 | grep . > "$SNIPPET_WATCH_STATE_DIR/last" || true
rm -f /tmp/new.$$
```

## When a watch wakes you

The envelope is from your own script, not from a person. Decide what the new lines mean. Act on what matters: answer, file a task through Mission Control, or tell the user through your reach policy when it is theirs to know. When nothing needs doing, end the turn quietly. Do not report that the watch fired.

## Habits that make the harness work for you

- When you promise to check something later, arrange the wake in the same turn. A promise with no wake behind it is forgotten.
- Prefer the cheapest mechanism that fits: a watch that wakes only on change beats a schedule that wakes every 15 minutes to find nothing.
- Make a wake carry what you need: a schedule's brief should say exactly what to check and what counts as news.
- After a recurring job, update its playbook: what worked, what failed and why, so the next run goes better.
- Write a custom tool for a command you keep retyping, and tell the user it exists.
- Clear what you no longer need: a stale schedule or watch costs turns and attention.
