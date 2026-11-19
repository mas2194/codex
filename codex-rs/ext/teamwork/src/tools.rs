//! Model-visible tools that drive a `/teamwork` run.
//!
//! The lead thread gets tools to record the brief and plan, submit work for review, block for the
//! user, and read the status. Spawned descendant threads get only the verdict tool, so gate
//! verdicts always come from an agent other than the lead.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_tools::FunctionCallError;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;

use crate::prompts::gate_model_guidance;
use crate::prompts::gate_spawn_message;
use crate::prompts::next_action;
use crate::prompts::status_markdown;
use crate::service::TeamworkRuntime;
use crate::service::TeamworkService;
use crate::state::Brief;
use crate::state::GateTicket;
use crate::state::MilestonePlan;
use crate::state::TeamworkError;
use crate::state::TeamworkRun;
use crate::state::Verdict;
use crate::state::VerdictOutcome;

pub(crate) const STATUS_TOOL_NAME: &str = "teamwork_status";
pub(crate) const RECORD_BRIEF_TOOL_NAME: &str = "teamwork_record_brief";
pub(crate) const RECORD_PLAN_TOOL_NAME: &str = "teamwork_record_plan";
pub(crate) const SUBMIT_FOR_REVIEW_TOOL_NAME: &str = "teamwork_submit_for_review";
pub(crate) const BLOCK_TOOL_NAME: &str = "teamwork_block";
pub(crate) const SUBMIT_VERDICT_TOOL_NAME: &str = "teamwork_submit_verdict";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TeamworkToolKind {
    Status,
    RecordBrief,
    RecordPlan,
    SubmitForReview,
    Block,
    SubmitVerdict,
}

impl TeamworkToolKind {
    pub(crate) const LEAD_TOOLS: [TeamworkToolKind; 5] = [
        TeamworkToolKind::Status,
        TeamworkToolKind::RecordBrief,
        TeamworkToolKind::RecordPlan,
        TeamworkToolKind::SubmitForReview,
        TeamworkToolKind::Block,
    ];

    fn name(self) -> &'static str {
        match self {
            TeamworkToolKind::Status => STATUS_TOOL_NAME,
            TeamworkToolKind::RecordBrief => RECORD_BRIEF_TOOL_NAME,
            TeamworkToolKind::RecordPlan => RECORD_PLAN_TOOL_NAME,
            TeamworkToolKind::SubmitForReview => SUBMIT_FOR_REVIEW_TOOL_NAME,
            TeamworkToolKind::Block => BLOCK_TOOL_NAME,
            TeamworkToolKind::SubmitVerdict => SUBMIT_VERDICT_TOOL_NAME,
        }
    }
}

pub(crate) struct TeamworkTool {
    kind: TeamworkToolKind,
    service: Arc<TeamworkService>,
    /// Thread that owns this tool instance: the lead for lead tools, the reviewer for verdicts.
    thread_id: String,
}

impl TeamworkTool {
    pub(crate) fn new(
        kind: TeamworkToolKind,
        service: Arc<TeamworkService>,
        thread_id: String,
    ) -> Self {
        Self {
            kind,
            service,
            thread_id,
        }
    }

    fn lead_runtime(&self) -> Result<Arc<TeamworkRuntime>, FunctionCallError> {
        self.service
            .runtime(&self.thread_id)
            .filter(|runtime| runtime.with_run(|run| run.is_some()))
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "no /teamwork run is active in this thread; the user starts one with `/teamwork <goal>`"
                        .to_string(),
                )
            })
    }

    async fn mutate_lead_run<T>(
        &self,
        update: impl FnOnce(&mut TeamworkRun) -> Result<T, TeamworkError>,
    ) -> Result<(T, TeamworkRun), FunctionCallError> {
        let runtime = self.lead_runtime()?;
        let result = runtime.with_run(|run| {
            let run = run
                .as_mut()
                .ok_or_else(|| TeamworkError("no active run".to_string()))?;
            let value = update(run)?;
            Ok::<_, TeamworkError>((value, run.clone()))
        });
        let result = result.map_err(|error| FunctionCallError::RespondToModel(error.0))?;
        runtime.save().await;
        Ok(result)
    }

    async fn handle_arguments(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        match self.kind {
            TeamworkToolKind::Status => {
                let run = self
                    .lead_runtime()?
                    .snapshot()
                    .ok_or_else(|| FunctionCallError::RespondToModel("no active run".into()))?;
                Ok(run_response(&run, "Current /teamwork state.", Vec::new()))
            }
            TeamworkToolKind::RecordBrief => {
                let brief: Brief = parse_arguments(arguments)?;
                let ((), run) = self.mutate_lead_run(|run| run.record_brief(brief)).await?;
                Ok(run_response(
                    &run,
                    "Brief recorded. The run is now planning.",
                    Vec::new(),
                ))
            }
            TeamworkToolKind::RecordPlan => {
                let args: RecordPlanArgs = parse_arguments(arguments)?;
                let ((), run) = self
                    .mutate_lead_run(|run| run.record_plan(args.milestones))
                    .await?;
                Ok(run_response(
                    &run,
                    "Plan recorded. The first milestone is now executing.",
                    Vec::new(),
                ))
            }
            TeamworkToolKind::SubmitForReview => {
                let args: SubmitForReviewArgs = parse_arguments(arguments)?;
                let (tickets, run) = self
                    .mutate_lead_run(|run| run.submit_for_review(&args.milestone_id, args.summary))
                    .await?;
                Ok(run_response(
                    &run,
                    "Submitted for independent review. Spawn one fresh gate agent per ticket, without parent history, and pass its spawn_message unchanged.",
                    tickets,
                ))
            }
            TeamworkToolKind::Block => {
                let args: BlockArgs = parse_arguments(arguments)?;
                let ((), run) = self.mutate_lead_run(|run| run.block(args.reason)).await?;
                Ok(run_response(
                    &run,
                    "The run is blocked. Ask the user your question and end the turn; the run resumes when they reply.",
                    Vec::new(),
                ))
            }
            TeamworkToolKind::SubmitVerdict => self.submit_verdict(arguments).await,
        }
    }

    async fn submit_verdict(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        let args: SubmitVerdictArgs = parse_arguments(arguments)?;
        let runtime = self
            .service
            .runtime_for_ticket(&self.thread_id, &args.gate_token)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "gate token `{}` does not belong to a /teamwork run led by an ancestor of this agent",
                    args.gate_token
                ))
            })?;
        let reviewer_modified_files = self.service.is_editor(&self.thread_id);
        let outcome = runtime
            .with_run(|run| {
                let run = run
                    .as_mut()
                    .ok_or_else(|| TeamworkError("the run is no longer active".to_string()))?;
                run.record_verdict(
                    &args.gate_token,
                    &self.thread_id,
                    reviewer_modified_files,
                    args.verdict,
                    args.findings,
                    args.evidence,
                )
            })
            .map_err(|error| FunctionCallError::RespondToModel(error.0))?;
        runtime.save().await;
        let message = match outcome {
            VerdictOutcome::AwaitingOtherGates => {
                "Verdict recorded. Other gates are still reviewing.".to_string()
            }
            VerdictOutcome::MilestonePassed { next_milestone_id } => {
                format!("Verdict recorded. The milestone passed; `{next_milestone_id}` is next.")
            }
            VerdictOutcome::FinalAuditOpened { .. } => {
                "Verdict recorded. Every milestone passed; the final audit is open.".to_string()
            }
            VerdictOutcome::MilestoneNeedsFixes { fix_round } => {
                format!(
                    "Verdict recorded. The milestone returns to the lead for fix round {fix_round}."
                )
            }
            VerdictOutcome::FinalAuditFailed { fix_round } => {
                format!("Verdict recorded. The final audit failed; fix round {fix_round} begins.")
            }
            VerdictOutcome::Blocked { reason } => {
                format!("Verdict recorded. The run is blocked for the user: {reason}.")
            }
            VerdictOutcome::Complete => "Verdict recorded. The run is complete.".to_string(),
        };
        Ok(json!({
            "recorded": true,
            "message": message,
            "instructions": "Your review is done. Reply with your verdict report and stop."
        }))
    }
}

fn run_response(run: &TeamworkRun, message: &str, new_tickets: Vec<GateTicket>) -> Value {
    let tickets = if new_tickets.is_empty() {
        run.open_tickets().cloned().collect::<Vec<_>>()
    } else {
        new_tickets
    };
    let tickets = tickets
        .iter()
        .map(|ticket| {
            json!({
                "gate_token": ticket.token,
                "gate": ticket.kind.as_str(),
                "milestone_id": ticket.milestone_id,
                "round": ticket.round,
                "recommended_model": gate_model_guidance(ticket.kind),
                "spawn_message": gate_spawn_message(run, ticket),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "message": message,
        "phase": run.phase.as_str(),
        "revision": run.revision,
        "next_action": next_action(run),
        "open_gate_tickets": tickets,
        "status": status_markdown(run),
    })
}

fn parse_arguments<T: DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|error| FunctionCallError::RespondToModel(format!("invalid arguments: {error}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordPlanArgs {
    milestones: Vec<MilestonePlan>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitForReviewArgs {
    milestone_id: String,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockArgs {
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitVerdictArgs {
    gate_token: String,
    verdict: Verdict,
    #[serde(default)]
    findings: String,
    #[serde(default)]
    evidence: String,
}

impl<'call> ToolExecutor<ToolCall<'call>> for TeamworkTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.kind.name())
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(self.kind)
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> codex_tools::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = call.function_arguments()?;
            let arguments = if arguments.trim().is_empty() {
                "{}"
            } else {
                arguments
            };
            let output = self.handle_arguments(arguments).await?;
            Ok(Box::new(JsonToolOutput::new(output)) as Box<dyn codex_tools::ToolOutput>)
        })
    }
}

fn string(description: &str) -> JsonSchema {
    JsonSchema::string(Some(description.to_string()))
}

fn string_list(description: &str) -> JsonSchema {
    JsonSchema::array(JsonSchema::string(None), Some(description.to_string()))
}

fn object(properties: Vec<(&str, JsonSchema)>, required: &[&str]) -> JsonSchema {
    JsonSchema::object(
        properties
            .into_iter()
            .map(|(name, schema)| (name.to_string(), schema))
            .collect::<BTreeMap<_, _>>(),
        Some(required.iter().map(ToString::to_string).collect()),
        Some(false.into()),
    )
}

fn function(name: &str, description: &str, parameters: JsonSchema) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

pub(crate) fn tool_spec(kind: TeamworkToolKind) -> ToolSpec {
    match kind {
        TeamworkToolKind::Status => function(
            STATUS_TOOL_NAME,
            "Return the authoritative state of the active /teamwork run: phase, brief, milestones, gate verdicts, the next required action, and a ready-made spawn_message for every open gate ticket.",
            object(Vec::new(), &[]),
        ),
        TeamworkToolKind::RecordBrief => function(
            RECORD_BRIEF_TOOL_NAME,
            "Record the scoping brief for the active /teamwork run and move it to planning. Every acceptance criterion needs a concrete verification method. Calling it again during planning replaces the brief.",
            object(
                vec![
                    (
                        "objective",
                        string("What the run must deliver, stated as an outcome."),
                    ),
                    ("requirements", string_list("Explicit requirements.")),
                    (
                        "acceptance_criteria",
                        JsonSchema::array(
                            object(
                                vec![
                                    ("id", string("Short unique id, for example `ac1`.")),
                                    ("description", string("What must be true.")),
                                    (
                                        "verification",
                                        string(
                                            "How to verify it: a command, a test, or a concrete check.",
                                        ),
                                    ),
                                ],
                                &["id", "description", "verification"],
                            ),
                            Some("At least one verifiable acceptance criterion.".to_string()),
                        ),
                    ),
                    (
                        "out_of_scope",
                        string_list("Work that is explicitly excluded."),
                    ),
                    (
                        "assumptions",
                        string_list("Assumptions made instead of asking the user."),
                    ),
                ],
                &["objective", "acceptance_criteria"],
            ),
        ),
        TeamworkToolKind::RecordPlan => function(
            RECORD_PLAN_TOOL_NAME,
            "Record the milestone plan for the active /teamwork run and start executing the first milestone. Tracks in the same milestone must own non-overlapping files. Record the model and reasoning effort you chose for each track's worker.",
            object(
                vec![(
                    "milestones",
                    JsonSchema::array(
                        object(
                            vec![
                                (
                                    "id",
                                    string(
                                        "Short unique id, for example `m1`. `final` is reserved.",
                                    ),
                                ),
                                ("title", string("Milestone title.")),
                                ("deliverable", string("Concrete, verifiable deliverable.")),
                                (
                                    "tracks",
                                    JsonSchema::array(
                                        object(
                                            vec![
                                                (
                                                    "id",
                                                    string("Track id, unique in the milestone."),
                                                ),
                                                ("description", string("The work in this track.")),
                                                (
                                                    "files",
                                                    string_list(
                                                        "Files or directories (ending in `/`) this track exclusively owns.",
                                                    ),
                                                ),
                                                (
                                                    "role",
                                                    string(
                                                        "Agent role, for example `worker`. Defaults to `worker`.",
                                                    ),
                                                ),
                                                (
                                                    "model",
                                                    string(
                                                        "Model you chose for this track's worker, from the spawn_agent model list. Omit to inherit.",
                                                    ),
                                                ),
                                                (
                                                    "reasoning_effort",
                                                    string(
                                                        "Reasoning effort you chose for this track's worker. Omit to inherit.",
                                                    ),
                                                ),
                                            ],
                                            &["id", "description", "files"],
                                        ),
                                        Some("One or more tracks.".to_string()),
                                    ),
                                ),
                            ],
                            &["id", "title", "deliverable", "tracks"],
                        ),
                        Some("Milestones in execution order.".to_string()),
                    ),
                )],
                &["milestones"],
            ),
        ),
        TeamworkToolKind::SubmitForReview => function(
            SUBMIT_FOR_REVIEW_TOOL_NAME,
            "Submit the current milestone (or, after a failed final audit, milestone_id `final`) for independent review. Opens Critic, Challenger, and Auditor gate tickets (or a Success Auditor ticket) and returns a spawn_message for each one. Only call it after integrating the work and running its checks.",
            object(
                vec![
                    (
                        "milestone_id",
                        string("Id of the current milestone, or `final`."),
                    ),
                    (
                        "summary",
                        string(
                            "What changed, which files, and which checks you ran with their results.",
                        ),
                    ),
                ],
                &["milestone_id", "summary"],
            ),
        ),
        TeamworkToolKind::Block => function(
            BLOCK_TOOL_NAME,
            "Block the active /teamwork run because a decision only the user can make is needed. Automatic continuation stops until the user replies. Do not use it because the work is hard or slow.",
            object(
                vec![(
                    "reason",
                    string("The specific question or decision you need from the user."),
                )],
                &["reason"],
            ),
        ),
        TeamworkToolKind::SubmitVerdict => function(
            SUBMIT_VERDICT_TOOL_NAME,
            "Record your verdict as an independent /teamwork gate reviewer. Use the gate_token from your task. Call it exactly once, after your review is complete.",
            object(
                vec![
                    (
                        "gate_token",
                        string("The gate token from your task message."),
                    ),
                    (
                        "verdict",
                        JsonSchema::string_enum(
                            vec![json!("pass"), json!("fail")],
                            Some(
                                "`pass` only when the evidence proves the bar is met.".to_string(),
                            ),
                        ),
                    ),
                    (
                        "findings",
                        string(
                            "Concrete defects with file:line and a failure scenario. Required for `fail`.",
                        ),
                    ),
                    (
                        "evidence",
                        string(
                            "Commands and checks you ran and what they showed. Required for `pass`.",
                        ),
                    ),
                ],
                &["gate_token", "verdict"],
            ),
        ),
    }
}
