# `/swarm`

`/swarm <task>` splits a task among named agents that work in parallel and coordinate on a shared message board. Workers build, verifiers check their work adversarially, and the current conversation becomes the coordinator. The design follows Google Antigravity's `/swarm` command. The harness (the `codex-swarm-extension` crate) enforces the workflow, not only the prompt.

```text
/swarm add OAuth login with backend, frontend, and end-to-end tests
/swarm board
/swarm status
/swarm say @api use port 8080 for the callback
/swarm stop
```

The harness recognizes any user message that starts with `/swarm`, so the command works from the TUI, `codex exec`, and app-server clients.

## Compared with Antigravity's `/swarm`

| Antigravity `/swarm` | Codex `/swarm` |
|---|---|
| The current conversation becomes the coordinator | Same. The root thread that types `/swarm` is the coordinator. |
| Named workers and verifiers (about one verifier for every 2-4 workers) | `swarm_define_roster` records named `worker` and `verifier` members. The harness requires at least one verifier and lets a verifier cover at most four workers. |
| A shared, append-only message board | A harness-owned board. Agents post with `swarm_post` and read with `swarm_read_board`. |
| `@name`, `@group`, and `@all` addressing | `@name`, custom groups, `@workers`, `@verifiers`, `@coordinator`, and `@all`. The harness resolves mentions and rejects unknown names. |
| Idle agents park at zero token cost and resume when mentioned | An agent whose turn ends parks. The harness starts a new turn only when a message mentions it, and hands it exactly the messages addressed to it. |
| Adversarial verifiers report defects to the worker | A verifier's `fail` goes back to the worker and wakes it. A `pass` notifies the coordinator. |
| Per-agent model pinning | The coordinator records `model` and `reasoning_effort` per member and spawns it with them. |
| Workers in isolated worktrees | Workers share the checkout but own non-overlapping files. The harness checks this when the roster is recorded and tracks `apply_patch` edits at runtime. |
| `/swarm board` opens the live board | `/swarm board` shows the latest board messages, and the full board is kept live in `$CODEX_HOME/swarm/<thread-id>.board.md`. |
| `/agents` lists the workers | The members are ordinary subagents, so `/agents` lists them as well. |

## Lifecycle

```text
          /swarm <task>
               │
               ▼
 forming ── swarm_define_roster ──▶ running ── swarm_finish (every worker verified) ──▶ finished
                                      │
         coordinator spawns members with their spawn_message, then parks
                                      │
   worker ── swarm_report_done ──▶ verifier ── swarm_verify ──┬── pass ──▶ @coordinator
     ▲                                                         │
     └──────────────── fail: findings wake the worker ◀────────┘
                       (3 failures: blocked until swarm_reopen)
```

1. **Forming.** The coordinator inspects the repository and calls `swarm_define_roster`. The harness validates names, file ownership, and verifier coverage, then returns a `spawn_message` per member with a join token.
2. **Spawning.** The coordinator spawns one subagent per member with `spawn_agent`, passing the `spawn_message` unchanged and the member's model and effort. Each member first calls `swarm_join` with its token, which binds its thread to its name.
3. **Running.** Members work and coordinate on the board. When their turn ends they park. A worker reports with `swarm_report_done`, which is delivered to its verifiers. A verifier records `swarm_verify`.
4. **Finishing.** `swarm_finish` succeeds only when a verifier has passed every active worker, and only after the coordinator acknowledges any recorded file ownership violations.

## Tools

| Tool | Who | What the harness enforces |
|---|---|---|
| `swarm_define_roster`, `swarm_add_members` | coordinator | Unique lowercase names, which cannot be reserved words. Workers own at least one path, and their paths do not overlap. Verifiers own nothing. Each worker is covered by a verifier, and a verifier covers at most 4 workers. Workers without a verifier are assigned automatically. At most 16 members. |
| `swarm_retire_member` | coordinator | A retired member receives no messages and does not count toward finishing. Its workers are reassigned to another verifier. Retiring is rejected when no verifier would be left for them. |
| `swarm_reopen` | coordinator | Sends a blocked or verified worker back to work with new instructions, and resets its fix budget. |
| `swarm_finish` | coordinator | Requires every active worker verified, and `acknowledge_violations` when violations exist. |
| `swarm_ask_user` | coordinator | Stops automatic resumption until the user replies. |
| `swarm_status` | coordinator | Returns the authoritative roster, progress, verdicts, violations, and the spawn messages of members that have not joined. |
| `swarm_post`, `swarm_read_board` | coordinator and members | Resolves mentions, delivers each message to its recipients, and wakes the parked ones. |
| `swarm_join` | spawned members not yet joined | The token must come from a swarm whose coordinator is an ancestor of the calling agent. Each token can be used once. |
| `swarm_report_done` | workers | Only while the work is in progress. The report is delivered to the worker's verifiers. |
| `swarm_verify` | verifiers | Only an assigned verifier can record a verdict, only on a worker awaiting verification, and only if the verifier never edited files. A `fail` needs findings and a `pass` needs evidence. The third failure blocks the worker for the coordinator. |

## Parking and waking

- When a member's turn ends and messages mention it, the harness starts a new turn with them. Otherwise the member parks.
- If a member parks while it still owes work (a worker that has not reported, or a verifier with a worker waiting), the harness tells `@coordinator`. It stops after three such notices per member.
- The coordinator parks while members work. It is woken when mentioned: a question, a verdict, a blocked worker, a parked member that owes work, or a violation.
- When the coordinator's turn ends with nothing to wake it for, the harness resumes it in three cases: members have not joined, every worker is verified, or every member is parked with work outstanding. After three resumptions without any state change, the run stalls until the user writes again.
- `/swarm say <message>` posts to the board as `@user` and wakes the members it mentions.

## File ownership

Workers share the repository checkout. Instead of separate worktrees, the swarm relies on ownership:

- When a roster is recorded or extended, the harness rejects overlapping worker paths.
- At runtime, the harness reads the paths of every successful `apply_patch`. Edits outside a worker's paths, any edit by a verifier, and coordinator edits to an unverified worker's files are recorded as violations and posted to `@coordinator`.
- A verifier that edited files can no longer record verdicts.

## State and the live board

Run state is persisted to `$CODEX_HOME/swarm/<thread-id>.json`, so an unfinished swarm survives a restart. A Markdown transcript of the roster and board is rewritten to `$CODEX_HOME/swarm/<thread-id>.board.md` on every change. Follow it from another terminal:

```bash
tail -f ~/.codex/swarm/*.board.md
```

## Limitations

- Edits made through shell commands, rather than `apply_patch`, are not attributed to an agent. Verifiers that edit through the shell are not disqualified.
- Members share one checkout rather than isolated worktrees, so a build or test run can observe another worker's in-progress changes.
- A member that is busy when a message arrives receives it when its current turn ends. Tool results report `unread_for_you` in the meantime.
- `/swarm board`, `/swarm status`, `/swarm say`, and `/swarm stop` are typed to the coordinator, so they run between the coordinator's turns. The coordinator is parked most of the time.
- Subagents must be enabled (`multi_agent`, on by default, or `multi_agent_v2`).
