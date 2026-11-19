//! Model-visible tools that drive a `/swarm` run.
//!
//! The coordinator gets tools to record and change the roster, post and read the board, reopen
//! work, and finish. A spawned member first sees only `swarm_join`; after joining it gets the
//! board tools plus `swarm_report_done` (workers) or `swarm_verify` (verifiers).

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

use crate::delivery::Courier;
use crate::prompts::member_spawn_message;
use crate::prompts::next_action;
use crate::prompts::status_markdown;
use crate::service::SwarmRuntime;
use crate::service::SwarmService;
use crate::state::Activity;
use crate::state::BoardMessage;
use crate::state::COORDINATOR;
use crate::state::Member;
use crate::state::MemberSpec;
use crate::state::MessageKind;
use crate::state::Posted;
use crate::state::SwarmError;
use crate::state::SwarmRun;
use crate::state::Verdict;

pub(crate) const STATUS: &str = "swarm_status";
pub(crate) const DEFINE_ROSTER: &str = "swarm_define_roster";
pub(crate) const ADD_MEMBERS: &str = "swarm_add_members";
pub(crate) const RETIRE_MEMBER: &str = "swarm_retire_member";
pub(crate) const REOPEN: &str = "swarm_reopen";
pub(crate) const FINISH: &str = "swarm_finish";
pub(crate) const ASK_USER: &str = "swarm_ask_user";
pub(crate) const POST: &str = "swarm_post";
pub(crate) const READ_BOARD: &str = "swarm_read_board";
pub(crate) const JOIN: &str = "swarm_join";
pub(crate) const REPORT_DONE: &str = "swarm_report_done";
pub(crate) const VERIFY: &str = "swarm_verify";

const DEFAULT_READ_LIMIT: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwarmToolKind {
    Status,
    DefineRoster,
    AddMembers,
    RetireMember,
    Reopen,
    Finish,
    AskUser,
    Post,
    ReadBoard,
    Join,
    ReportDone,
    Verify,
}

impl SwarmToolKind {
    pub(crate) const COORDINATOR_FORMING: [SwarmToolKind; 3] = [
        SwarmToolKind::Status,
        SwarmToolKind::DefineRoster,
        SwarmToolKind::AskUser,
    ];
    pub(crate) const COORDINATOR_RUNNING: [SwarmToolKind; 8] = [
        SwarmToolKind::Status,
        SwarmToolKind::Post,
        SwarmToolKind::ReadBoard,
        SwarmToolKind::AddMembers,
        SwarmToolKind::RetireMember,
        SwarmToolKind::Reopen,
        SwarmToolKind::Finish,
        SwarmToolKind::AskUser,
    ];
    pub(crate) const COORDINATOR_FINISHED: [SwarmToolKind; 2] =
        [SwarmToolKind::Status, SwarmToolKind::ReadBoard];
    pub(crate) const WORKER: [SwarmToolKind; 3] = [
        SwarmToolKind::Post,
        SwarmToolKind::ReadBoard,
        SwarmToolKind::ReportDone,
    ];
    pub(crate) const VERIFIER: [SwarmToolKind; 3] = [
        SwarmToolKind::Post,
        SwarmToolKind::ReadBoard,
        SwarmToolKind::Verify,
    ];
    pub(crate) const MEMBER_FINISHED: [SwarmToolKind; 1] = [SwarmToolKind::ReadBoard];
    pub(crate) const JOINING: [SwarmToolKind; 1] = [SwarmToolKind::Join];

    pub(crate) fn name(self) -> &'static str {
        match self {
            SwarmToolKind::Status => STATUS,
            SwarmToolKind::DefineRoster => DEFINE_ROSTER,
            SwarmToolKind::AddMembers => ADD_MEMBERS,
            SwarmToolKind::RetireMember => RETIRE_MEMBER,
            SwarmToolKind::Reopen => REOPEN,
            SwarmToolKind::Finish => FINISH,
            SwarmToolKind::AskUser => ASK_USER,
            SwarmToolKind::Post => POST,
            SwarmToolKind::ReadBoard => READ_BOARD,
            SwarmToolKind::Join => JOIN,
            SwarmToolKind::ReportDone => REPORT_DONE,
            SwarmToolKind::Verify => VERIFY,
        }
    }
}

pub(crate) struct SwarmTool {
    kind: SwarmToolKind,
    service: Arc<SwarmService>,
    courier: Arc<Courier>,
    /// Thread that owns this tool instance.
    thread_id: String,
}

fn model_error(error: SwarmError) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.0)
}

impl SwarmTool {
    pub(crate) fn new(
        kind: SwarmToolKind,
        service: Arc<SwarmService>,
        courier: Arc<Courier>,
        thread_id: String,
    ) -> Self {
        Self {
            kind,
            service,
            courier,
            thread_id,
        }
    }

    /// The runtime and board name of the caller: the coordinator of its own run, or a member.
    fn caller(&self) -> Result<(Arc<SwarmRuntime>, String), FunctionCallError> {
        if let Some(runtime) = self
            .service
            .runtime(&self.thread_id)
            .filter(|runtime| runtime.with_run(|run| run.is_some()))
        {
            return Ok((runtime, COORDINATOR.to_string()));
        }
        self.service.membership(&self.thread_id).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "this agent is not part of an active /swarm run".to_string(),
            )
        })
    }

    fn coordinator(&self) -> Result<Arc<SwarmRuntime>, FunctionCallError> {
        match self.caller()? {
            (runtime, name) if name == COORDINATOR => Ok(runtime),
            _ => Err(FunctionCallError::RespondToModel(
                "only the swarm coordinator can use this tool".to_string(),
            )),
        }
    }

    /// Applies `update` to the run, saves it, and delivers the resulting post.
    async fn mutate<T>(
        &self,
        runtime: &Arc<SwarmRuntime>,
        update: impl FnOnce(&mut SwarmRun) -> Result<T, SwarmError>,
    ) -> Result<(T, SwarmRun), FunctionCallError> {
        let result = runtime.with_run(|run| {
            let run = run
                .as_mut()
                .ok_or_else(|| SwarmError("the swarm is no longer active".to_string()))?;
            let value = update(run)?;
            Ok::<_, SwarmError>((value, run.clone()))
        });
        let result = result.map_err(model_error)?;
        runtime.save().await;
        Ok(result)
    }

    async fn deliver(&self, runtime: &Arc<SwarmRuntime>, posted: &Posted) -> Vec<String> {
        self.courier.deliver(runtime, &posted.recipients).await
    }

    async fn handle_arguments(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        match self.kind {
            SwarmToolKind::Status => {
                let runtime = self.coordinator()?;
                let run = runtime
                    .snapshot()
                    .ok_or_else(|| FunctionCallError::RespondToModel("no active swarm".into()))?;
                Ok(coordinator_response(
                    &run,
                    "Current /swarm state.",
                    &run.members,
                ))
            }
            SwarmToolKind::DefineRoster => {
                let args: RosterArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let (added, run) = self
                    .mutate(&runtime, |run| run.define_roster(args.members))
                    .await?;
                Ok(coordinator_response(
                    &run,
                    "Roster recorded and the swarm is running. Spawn one agent per member with spawn_agent, without parent history, passing its spawn_message unchanged and its model and reasoning effort. Then end your turn to park.",
                    &added,
                ))
            }
            SwarmToolKind::AddMembers => {
                let args: RosterArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let (added, run) = self
                    .mutate(&runtime, |run| run.add_members(args.members))
                    .await?;
                Ok(coordinator_response(
                    &run,
                    "Members added. Spawn each one with its spawn_message unchanged.",
                    &added,
                ))
            }
            SwarmToolKind::RetireMember => {
                let args: RetireArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let name = args
                    .name
                    .trim()
                    .trim_start_matches('@')
                    .to_ascii_lowercase();
                let ((), run) = self
                    .mutate(&runtime, |run| run.retire(&name, &args.reason))
                    .await?;
                Ok(coordinator_response(
                    &run,
                    "Member retired. Close its agent with close_agent if it is still open.",
                    &[],
                ))
            }
            SwarmToolKind::Reopen => {
                let args: ReopenArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let (posted, run) = self
                    .mutate(&runtime, |run| run.reopen(&args.worker, &args.instructions))
                    .await?;
                let woken = self.deliver(&runtime, &posted).await;
                let mut response = coordinator_response(&run, "Assignment reopened.", &[]);
                response["woken"] = json!(woken);
                Ok(response)
            }
            SwarmToolKind::Finish => {
                let args: FinishArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let ((), run) = self
                    .mutate(&runtime, |run| {
                        run.finish(&args.summary, args.acknowledge_violations.as_deref())
                    })
                    .await?;
                Ok(coordinator_response(
                    &run,
                    "The swarm is finished. Close any member agents that are still open, then give the user the final report.",
                    &[],
                ))
            }
            SwarmToolKind::AskUser => {
                let args: AskUserArgs = parse_arguments(arguments)?;
                let runtime = self.coordinator()?;
                let ((), _run) = self
                    .mutate(&runtime, |run| run.wait_for_user(&args.question))
                    .await?;
                Ok(json!({
                    "message": "The harness will not resume you until the user replies. Ask the user your question and end the turn. Members keep working and the board keeps collecting messages.",
                }))
            }
            SwarmToolKind::Post => {
                let args: PostArgs = parse_arguments(arguments)?;
                let (runtime, name) = self.caller()?;
                let kind = args.kind.unwrap_or(MessageKind::Note);
                let (posted, run) = self
                    .mutate(&runtime, |run| {
                        run.post(&name, &args.to, kind, &args.message)
                    })
                    .await?;
                let woken = self.deliver(&runtime, &posted).await;
                let note = if posted.recipients.is_empty() {
                    "No one was mentioned, so no one was woken. The message is on the board for anyone who reads it."
                } else {
                    "Recipients that were busy get the message when their current turn ends."
                };
                Ok(json!({
                    "posted": true,
                    "seq": posted.seq,
                    "recipients": posted.recipients,
                    "woken": woken,
                    "note": note,
                    "unread_for_you": run.pending_for(&name).len(),
                }))
            }
            SwarmToolKind::ReadBoard => {
                let args: ReadBoardArgs = parse_arguments(arguments)?;
                let (runtime, name) = self.caller()?;
                let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, 200);
                let (messages, run) = self
                    .mutate(&runtime, |run| {
                        let since = args.since.unwrap_or(0);
                        let mut messages = run
                            .board
                            .iter()
                            .filter(|message| message.seq > since)
                            .filter(|message| {
                                !args.mentions_only.unwrap_or(false)
                                    || message.recipients.iter().any(|r| r == &name)
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        let skip = messages.len().saturating_sub(limit);
                        messages.drain(..skip);
                        let latest = run.latest_seq();
                        run.mark_delivered(&name, latest);
                        Ok(messages)
                    })
                    .await?;
                Ok(json!({
                    "you": name,
                    "phase": run.phase.as_str(),
                    "latest_seq": run.latest_seq(),
                    "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
                    "roster": status_markdown(&run),
                    "valid_mentions": run.valid_mentions(),
                }))
            }
            SwarmToolKind::Join => self.join(arguments).await,
            SwarmToolKind::ReportDone => {
                let args: ReportDoneArgs = parse_arguments(arguments)?;
                let (runtime, name) = self.caller()?;
                let (posted, _run) = self
                    .mutate(&runtime, |run| run.report_done(&name, &args.summary))
                    .await?;
                let woken = self.deliver(&runtime, &posted).await;
                Ok(json!({
                    "reported": true,
                    "verifiers": posted.recipients,
                    "woken": woken,
                    "instructions": "Your report went to your verifier. End your turn to park; the harness wakes you if the verifier finds defects or someone mentions you.",
                }))
            }
            SwarmToolKind::Verify => {
                let args: VerifyArgs = parse_arguments(arguments)?;
                let (runtime, name) = self.caller()?;
                let (posted, run) = self
                    .mutate(&runtime, |run| {
                        run.verify(
                            &name,
                            &args.worker,
                            args.verdict,
                            &args.findings,
                            &args.evidence,
                        )
                    })
                    .await?;
                let woken = self.deliver(&runtime, &posted).await;
                Ok(json!({
                    "recorded": true,
                    "notified": posted.recipients,
                    "woken": woken,
                    "unread_for_you": run.pending_for(&name).len(),
                    "instructions": "Verdict recorded. End your turn to park unless another worker you verify is waiting.",
                }))
            }
        }
    }

    async fn join(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        let args: JoinArgs = parse_arguments(arguments)?;
        let token = args.join_token.trim();
        let runtime = self
            .service
            .runtime_for_token(&self.thread_id, token)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "this join token does not belong to a /swarm run coordinated by an ancestor of this agent".to_string(),
                )
            })?;
        let thread_id = self.thread_id.clone();
        let ((member, pending), run) = self
            .mutate(&runtime, |run| {
                let member = run.join(token, &thread_id)?;
                let pending = run.pending_for(&member.name);
                if let Some(last) = pending.last() {
                    run.mark_delivered(&member.name, last.seq);
                }
                Ok((member, pending))
            })
            .await?;
        Ok(json!({
            "joined": true,
            "you": member.name,
            "role": member.role.as_str(),
            "files": member.files,
            "verifies": member.verifies,
            "messages_for_you": pending.iter().map(message_json).collect::<Vec<_>>(),
            "valid_mentions": run.valid_mentions(),
            "instructions": "You joined the swarm. The board tools are available from your next step. Start on your assignment.",
        }))
    }
}

fn message_json(message: &BoardMessage) -> Value {
    json!({
        "seq": message.seq,
        "from": message.from,
        "kind": message.kind.as_str(),
        "to": message.recipients,
        "body": message.body,
    })
}

fn member_json(run: &SwarmRun, member: &Member) -> Value {
    let mut value = json!({
        "name": member.name,
        "role": member.role.as_str(),
        "activity": member.activity.as_str(),
        "files": member.files,
        "verifies": member.verifies,
        "model": member.model,
        "reasoning_effort": member.reasoning_effort,
    });
    if let Some(progress) = member.progress {
        value["progress"] = json!(progress.as_str());
    }
    if member.activity == Activity::Invited {
        value["spawn_message"] = json!(member_spawn_message(run, member));
    }
    value
}

/// Response for coordinator tools. `members` lists members to show in detail, with spawn
/// messages for those still waiting to join.
fn coordinator_response(run: &SwarmRun, message: &str, members: &[Member]) -> Value {
    let invited = run
        .members
        .iter()
        .filter(|member| member.activity == Activity::Invited)
        .filter(|member| !members.iter().any(|shown| shown.name == member.name))
        .cloned()
        .collect::<Vec<_>>();
    let detailed = members
        .iter()
        .chain(invited.iter())
        .map(|member| member_json(run, member))
        .collect::<Vec<_>>();
    json!({
        "message": message,
        "phase": run.phase.as_str(),
        "revision": run.revision,
        "next_action": next_action(run),
        "members": detailed,
        "status": status_markdown(run),
        "unread_for_you": run.pending_for(COORDINATOR).len(),
    })
}

fn parse_arguments<T: DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|error| FunctionCallError::RespondToModel(format!("invalid arguments: {error}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterArgs {
    members: Vec<MemberSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetireArgs {
    name: String,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReopenArgs {
    worker: String,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishArgs {
    summary: String,
    #[serde(default)]
    acknowledge_violations: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserArgs {
    question: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    message: String,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    kind: Option<MessageKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadBoardArgs {
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    mentions_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinArgs {
    join_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportDoneArgs {
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    worker: String,
    verdict: Verdict,
    #[serde(default)]
    findings: String,
    #[serde(default)]
    evidence: String,
}

impl<'call> ToolExecutor<ToolCall<'call>> for SwarmTool {
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

fn members_schema() -> JsonSchema {
    object(
        vec![(
            "members",
            JsonSchema::array(
                object(
                    vec![
                        (
                            "name",
                            string(
                                "Short lowercase name used for @mentions, for example `api` or `check-ui`.",
                            ),
                        ),
                        (
                            "role",
                            JsonSchema::string_enum(
                                vec![json!("worker"), json!("verifier")],
                                Some(
                                    "`worker` builds; `verifier` checks workers and never edits."
                                        .to_string(),
                                ),
                            ),
                        ),
                        (
                            "assignment",
                            string(
                                "Standalone description of the member's job; the member cannot see your conversation.",
                            ),
                        ),
                        (
                            "files",
                            string_list(
                                "Workers only: files or directories (ending in `/`) the worker exclusively owns.",
                            ),
                        ),
                        (
                            "groups",
                            string_list(
                                "Optional group names, so others can mention several members as `@group`.",
                            ),
                        ),
                        (
                            "verifies",
                            string_list(
                                "Verifiers only: names of the workers this verifier checks (at most 4). Omit to let the harness assign them.",
                            ),
                        ),
                        (
                            "model",
                            string(
                                "Model you chose for this member, from the spawn_agent model list. Omit to inherit.",
                            ),
                        ),
                        (
                            "reasoning_effort",
                            string("Reasoning effort you chose for this member. Omit to inherit."),
                        ),
                    ],
                    &["name", "role", "assignment"],
                ),
                Some("Members to add.".to_string()),
            ),
        )],
        &["members"],
    )
}

pub(crate) fn tool_spec(kind: SwarmToolKind) -> ToolSpec {
    match kind {
        SwarmToolKind::Status => function(
            STATUS,
            "Return the authoritative /swarm state: roster, activity (active, parked, invited, retired), work progress, verdicts, violations, the next action, and the spawn_message of every member that has not joined.",
            object(Vec::new(), &[]),
        ),
        SwarmToolKind::DefineRoster => function(
            DEFINE_ROSTER,
            "Record the swarm roster and start the swarm. Needs at least one worker and one verifier (one verifier for every 2-4 workers). Workers must own non-overlapping files. Returns a spawn_message for each member to pass unchanged to spawn_agent.",
            members_schema(),
        ),
        SwarmToolKind::AddMembers => function(
            ADD_MEMBERS,
            "Add members to the running swarm, with the same rules as swarm_define_roster. Returns a spawn_message for each new member.",
            members_schema(),
        ),
        SwarmToolKind::RetireMember => function(
            RETIRE_MEMBER,
            "Retire a member: it receives no further messages and no longer counts toward finishing. Workers it verified are reassigned to other verifiers.",
            object(
                vec![
                    ("name", string("Name of the member to retire.")),
                    ("reason", string("Why the member is retired.")),
                ],
                &["name"],
            ),
        ),
        SwarmToolKind::Reopen => function(
            REOPEN,
            "Send a worker back to work with new instructions: after it was blocked by failed verifications, or when its verified work needs more changes. Resets its fix budget and wakes it.",
            object(
                vec![
                    ("worker", string("Name of the worker.")),
                    ("instructions", string("What the worker must change.")),
                ],
                &["worker", "instructions"],
            ),
        ),
        SwarmToolKind::Finish => function(
            FINISH,
            "Finish the swarm. Rejected until a verifier has passed every active worker. If file ownership violations are on record, explain how each was resolved in acknowledge_violations.",
            object(
                vec![
                    ("summary", string("What the swarm delivered and the checks you ran with their results.")),
                    ("acknowledge_violations", string("How each recorded file ownership violation was resolved.")),
                ],
                &["summary"],
            ),
        ),
        SwarmToolKind::AskUser => function(
            ASK_USER,
            "Pause automatic resumption because you need a decision only the user can make. Then ask the user and end the turn. Do not use it because work is slow.",
            object(
                vec![("question", string("The decision you need from the user."))],
                &["question"],
            ),
        ),
        SwarmToolKind::Post => function(
            POST,
            "Post a message to the swarm board. Mention recipients in the text as @name, @group, @workers, @verifiers, @coordinator, or @all, or list them in `to`. Each recipient that is parked is woken with the message, so mention only agents that must act.",
            object(
                vec![
                    ("message", string("The message. Keep it short; point to files and lines instead of pasting content.")),
                    ("to", string_list("Optional recipients in addition to the @mentions in the message.")),
                    (
                        "kind",
                        JsonSchema::string_enum(
                            vec![json!("note"), json!("question"), json!("handoff"), json!("report"), json!("defect")],
                            Some("Message type. Defaults to `note`.".to_string()),
                        ),
                    ),
                ],
                &["message"],
            ),
        ),
        SwarmToolKind::ReadBoard => function(
            READ_BOARD,
            "Read the swarm board: recent messages, the roster, and valid mentions. Marks the messages for you as read.",
            object(
                vec![
                    ("since", JsonSchema::integer(Some("Only messages with a sequence number above this.".to_string()))),
                    ("limit", JsonSchema::integer(Some("Maximum number of most recent messages (default 30).".to_string()))),
                    ("mentions_only", JsonSchema::boolean(Some("Only messages that mention you.".to_string()))),
                ],
                &[],
            ),
        ),
        SwarmToolKind::Join => function(
            JOIN,
            "Join the /swarm run with the join_token from your task message. Call it first.",
            object(
                vec![("join_token", string("The join token from your task message."))],
                &["join_token"],
            ),
        ),
        SwarmToolKind::ReportDone => function(
            REPORT_DONE,
            "Report your assignment done and send it to your verifier. Only call it after your checks pass.",
            object(
                vec![(
                    "summary",
                    string("What changed, which files, and which checks you ran with their results."),
                )],
                &["summary"],
            ),
        ),
        SwarmToolKind::Verify => function(
            VERIFY,
            "Record your verdict on a worker you verify, after it reported done. `fail` sends your findings to the worker; `pass` tells the coordinator.",
            object(
                vec![
                    ("worker", string("Name of the worker.")),
                    (
                        "verdict",
                        JsonSchema::string_enum(
                            vec![json!("pass"), json!("fail")],
                            Some("`pass` only when the evidence proves the work meets its assignment.".to_string()),
                        ),
                    ),
                    ("findings", string("Concrete defects with file:line and how to reproduce them. Required for `fail`.")),
                    ("evidence", string("Commands and checks you ran and what they showed. Required for `pass`.")),
                ],
                &["worker", "verdict"],
            ),
        ),
    }
}
