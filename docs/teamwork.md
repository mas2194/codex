# `/teamwork`

`/teamwork <goal>` runs a long, multi-step goal with a coordinated team of subagents. The command is modeled on the agent-team workflow found in other agentic IDEs: a scoping step, milestone execution, and independent review gates.

```text
/teamwork migrate the REST service from Express to Fastify with full test coverage
```

## How it works

The current (root) agent becomes the **lead**. It is the only agent that talks to you, and it owns the plan and the final answer.

1. **Scoping.** The lead reads only as much of the repository as it needs. It writes down the goals, requirements, acceptance criteria, and how each criterion will be verified, in `.codex/teamwork/<slug>/BRIEF.md`. It asks you up to three questions only when an ambiguity would risk wasted or destructive work.
2. **Plan.** The lead splits the work into milestones. Each milestone is split into tracks that touch non-overlapping files. The plan, including each subagent's role, model, and reasoning effort, is kept in `.codex/teamwork/<slug>/PLAN.md`.
3. **Execute.** For each milestone, the lead spawns one worker per track (in parallel when the tracks are independent), integrates the results, and runs the verification commands.
4. **Gates.** Fresh subagents that did not write the code review each milestone:
   - **Critic**: finds correctness bugs and violations of repository conventions.
   - **Challenger**: tries to break the result.
   - **Auditor**: checks the milestone against the acceptance criteria.

   A failing gate sends findings back for a fix. The loop runs at most three rounds, after which the lead reports the blocker to you.
5. **Success audit.** A final auditor re-runs the full verification suite against the brief before the lead reports success.

## Model and reasoning effort

You do not pick models for the team. The lead chooses the `model` and `reasoning_effort` for every subagent from the models offered to it (the `spawn_agent` tool description or `<model_catalog>`):

- **Mechanical edits and searches:** a fast model with low effort.
- **Well-specified implementation:** the default model with medium effort.
- **Design, gate reviews, and retries after a failed gate:** the strongest model with high effort, or `xhigh` when offered.

The lead's own model and effort are whatever you selected with `/model`.

## Prompt format

The lead writes each delegation message as a standalone brief. The brief uses the XML-style sections recommended in OpenAI's GPT-5 prompting guides: `<role>`, `<objective>`, `<context>`, `<scope>`, `<instructions>`, `<verification>`, and `<output_format>`. Every subagent returns a structured report with a status, the changes made, the verification run, its findings, and its assumptions. The full lead prompt is in [`codex-rs/tui/assets/prompt_for_teamwork_command.md`](../codex-rs/tui/assets/prompt_for_teamwork_command.md).

## Requirements

- Subagents must be enabled (the `multi_agent` feature, on by default, or `multi_agent_v2`). If they are disabled, the lead runs every phase itself and reviews its own work against each gate.
- Subagents inherit the session's sandbox and approval settings.
- `/teamwork` uses noticeably more tokens than a single-agent turn. Use it for large or multi-milestone work.
