# `/teamwork`

`/teamwork <goal>` runs a long, multi-step goal with a coordinated team of subagents: scoping, milestone execution, and independent review gates. The workflow is enforced by the harness (the `codex-teamwork-extension` crate), not only by prompt instructions.

```text
/teamwork migrate the REST service from Express to Fastify with full test coverage
/teamwork status
/teamwork stop
```

The harness recognizes any user message that starts with `/teamwork`, so the command works from the TUI, `codex exec`, and app-server clients.

## Architecture

The current (root) agent becomes the **lead**. The harness keeps the run as a state machine and persists it to `$CODEX_HOME/teamwork/<thread-id>.json`, so an unfinished run survives a restart.

```text
scoping ──record_brief──▶ planning ──record_plan──▶ executing(m) ──submit_for_review──▶ gating(m)
                                                        ▲                                  │
                                                        └──── any gate FAIL (≤ 3 rounds) ◀─┤
                                                                                           │ all PASS
                                     complete ◀── PASS ── final_audit ◀── last milestone ──┘
                                                            │   ▲
                                                       FAIL ▼   │ submit_for_review("final")
                                                           final_fix
```

The lead advances the run only through harness tools:

| Tool | Who can call it | What the harness enforces |
|---|---|---|
| `teamwork_record_brief` | lead | Every acceptance criterion needs a verification method. |
| `teamwork_record_plan` | lead | Milestones and tracks need unique ids. **Tracks in a milestone cannot own overlapping files or directories.** The model and reasoning effort for each track are recorded. |
| `teamwork_submit_for_review` | lead | Only the current milestone can be submitted. The harness opens Critic, Challenger, and Auditor tickets, each with a ready-made `spawn_message`. |
| `teamwork_submit_verdict` | spawned descendants only | The lead can never record a verdict. A thread that edited files (via `apply_patch`) cannot act as a gate. One agent cannot fill two gates in the same round. `fail` requires findings; `pass` requires evidence. |
| `teamwork_block` | lead | Stops automatic continuation until the user replies. |
| `teamwork_status` | lead | Returns the authoritative state and the spawn messages for open tickets. |

Other guarantees:

- **The run cannot finish early.** Only a passing Success Auditor verdict moves the run to `complete`.
- **Bounded fix loops.** After three failed review rounds for a milestone, or for the final audit, the run blocks for the user. A reply from the user resumes it with a fresh fix budget.
- **Automatic continuation.** Whenever the lead's turn ends while the run is in `planning` through `final_fix`, the harness starts a continuation turn that carries the current state and the next required action.
- **No-progress breaker.** Three consecutive continuations without any state change block the run for the user.
- **Robust to compaction.** Every user message during an active run gets the authoritative state attached, as does every continuation.

## Model and reasoning effort

You do not pick models for the team. The lead chooses the `model` and `reasoning_effort` for every subagent from the models offered to it (the `spawn_agent` tool description or `<model_catalog>`), and records its choices in the plan:

- **Mechanical edits and searches:** a fast model with low effort.
- **Well-specified implementation:** the default model with medium effort.
- **Design, retries after a failed gate, and every gate:** the strongest model with high effort, or `xhigh` when offered. Each gate ticket carries this recommendation.

The lead's own model and effort are whatever you selected with `/model`.

## Prompt format

Harness-owned prompts follow the structure recommended in OpenAI's GPT-5 prompting guides:

- **Lead instructions:** Markdown sections (`Role and Objective`, `Phases`, `Rules`, `Final Output`).
- **Gate briefs:** XML-style blocks (`<role>`, `<objective>`, `<context>`, `<instructions>`, `<verdict>`, `<output_format>`).
- **Worker briefs:** the lead uses the same blocks.

The templates are in [`codex-rs/ext/teamwork/templates`](../codex-rs/ext/teamwork/templates).

## Limitations

- File ownership is checked when the plan is recorded. The harness does not intercept edits at runtime, so an agent that edits files outside its track is caught only by the gates.
- An edit made through a shell command (rather than `apply_patch`) does not disqualify an agent from serving as a gate.
- Subagents must be enabled (`multi_agent`, on by default, or `multi_agent_v2`). Without them, the run blocks at its first gate.
