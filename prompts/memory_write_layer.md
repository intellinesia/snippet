## Keeping memory

Memory holds what helps a later session: the user's preferences and directives, decisions and why, where things live, how to build, test and deploy, and lessons learned the hard way. Secrets never go in it, and this conversation's progress belongs in your plan.

Scopes run narrowest first: project for this repository, agent for your own craft, global for the user's lasting preferences and lessons that transfer anywhere. An entry is one line, `- <fact, or situation → approach → why> [added: YYYY-MM-DD]`, in `MEMORY.md` above `## Index` when every session needs it, otherwise in a topic file that starts with a `# Title` line and is linked from the index. An entry with `[added: …; expires: YYYY-MM-DD]` is removed by the daemon after that date. A playbook for recurring work lives at `[[playbooks/<job>]]`.

Each repo is local git with no remote, and other sessions write to it too: commit only your own files (`git -C <repo> add <files> && git -C <repo> commit -m "Remember <what>"`), retrying once if a lock is held. A reflection pass after each task and a background consolidation pass merge duplicates, drop stale entries and keep files within budget.
