use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_core::ThreadManager;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadIdleCause;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_protocol::user_input::UserInput;
use codex_tools::ToolPayload;

use crate::delivery::Courier;
use crate::delivery::context_fragment;
use crate::prompts;
use crate::service::SwarmRuntime;
use crate::service::SwarmService;
use crate::state::COORDINATOR;
use crate::state::CoordinatorIdle;
use crate::state::MemberIdle;
use crate::state::MessageKind;
use crate::state::Phase;
use crate::state::Role;
use crate::state::SwarmRun;
use crate::state::USER;
use crate::state::patch_paths;
use crate::tools::SwarmTool;
use crate::tools::SwarmToolKind;

/// Tools whose successful use means the calling thread edited files.
const FILE_EDIT_TOOLS: [&str; 1] = ["apply_patch"];
/// Messages shown by `/swarm board`.
const BOARD_VIEW_LIMIT: usize = 40;

/// Host configuration for the `/swarm` extension.
#[derive(Clone, Debug)]
pub struct SwarmExtensionConfig {
    pub enabled: bool,
    /// Directory for persisted run state and live board transcripts, usually
    /// `$CODEX_HOME/swarm`. `None` keeps runs in memory only.
    pub state_dir: Option<PathBuf>,
}

/// Per-thread facts captured when the host starts the thread.
#[derive(Clone, Debug)]
struct SwarmThreadInfo {
    thread_id: String,
    is_subagent: bool,
    enabled: bool,
    state_dir: Option<PathBuf>,
}

/// Paths of `apply_patch` calls that started but have not finished, by call id.
#[derive(Debug, Default)]
struct PendingEdits(Mutex<HashMap<String, Vec<String>>>);

struct SwarmExtension<C> {
    service: Arc<SwarmService>,
    courier: Arc<Courier>,
    config: Arc<dyn Fn(&C) -> SwarmExtensionConfig + Send + Sync>,
}

/// What the user asked for with a `/swarm` message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SwarmCommand {
    Start(String),
    Board,
    Status,
    Say(String),
    Stop,
    Usage,
}

/// Parses a user message that starts with `/swarm`.
pub(crate) fn parse_swarm_command(text: &str) -> Option<SwarmCommand> {
    let rest = text.trim_start().strip_prefix("/swarm")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let argument = rest.trim();
    let (first, remainder) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(first, rest)| (first, rest.trim()));
    Some(match first.to_ascii_lowercase().as_str() {
        "" => SwarmCommand::Usage,
        "board" if remainder.is_empty() => SwarmCommand::Board,
        "status" if remainder.is_empty() => SwarmCommand::Status,
        "stop" | "cancel" if remainder.is_empty() => SwarmCommand::Stop,
        "say" if !remainder.is_empty() => SwarmCommand::Say(remainder.to_string()),
        _ => SwarmCommand::Start(argument.to_string()),
    })
}

fn new_run_id(thread_id: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let prefix = thread_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>();
    format!("{prefix}{}", millis % 1_000_000_000)
}

fn edit_input(payload: &ToolPayload) -> Option<String> {
    match payload {
        ToolPayload::Custom { input } => Some(input.clone()),
        ToolPayload::Function { arguments } => serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| {
                value
                    .get("input")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .or_else(|| Some(arguments.clone())),
        ToolPayload::ToolSearch { .. } => None,
    }
}

impl<C> SwarmExtension<C> {
    fn coordinator_runtime(&self, info: &SwarmThreadInfo) -> Option<Arc<SwarmRuntime>> {
        if info.is_subagent || !info.enabled {
            return None;
        }
        self.service
            .runtime(&info.thread_id)
            .filter(|runtime| runtime.with_run(|run| run.is_some()))
    }

    /// The runtime and board name of the agent in this thread, if it takes part in a swarm.
    fn participant(&self, info: &SwarmThreadInfo) -> Option<(Arc<SwarmRuntime>, String)> {
        if !info.enabled {
            return None;
        }
        if let Some(runtime) = self.coordinator_runtime(info) {
            return Some((runtime, COORDINATOR.to_string()));
        }
        if info.is_subagent {
            return self.service.membership(&info.thread_id);
        }
        None
    }

    async fn on_coordinator_idle(&self, runtime: Arc<SwarmRuntime>) {
        let decision = runtime.with_run(|run| run.as_mut().map(SwarmRun::on_coordinator_idle));
        match decision {
            Some(CoordinatorIdle::Wake) => {
                runtime.save().await;
                self.courier
                    .deliver(&runtime, &[COORDINATOR.to_string()])
                    .await;
            }
            Some(CoordinatorIdle::Nudge(instruction)) => {
                runtime.save().await;
                let Some(prompt) = runtime.with_run(|run| {
                    run.as_ref()
                        .map(|run| prompts::nudge_prompt(run, &instruction))
                }) else {
                    return;
                };
                self.courier
                    .wake(runtime.coordinator_thread_id(), prompt)
                    .await;
            }
            Some(CoordinatorIdle::Stalled) => {
                tracing::info!("swarm coordinator stalled without progress");
                runtime.save().await;
            }
            Some(CoordinatorIdle::Park | CoordinatorIdle::Done) | None => {
                runtime.save().await;
            }
        }
    }

    async fn on_member_idle(&self, runtime: Arc<SwarmRuntime>, name: String) {
        let decision = runtime.with_run(|run| run.as_mut().map(|run| run.on_member_idle(&name)));
        runtime.save().await;
        match decision {
            Some(MemberIdle::Wake) => {
                self.courier.deliver(&runtime, &[name]).await;
            }
            Some(MemberIdle::Notified(recipients)) => {
                self.courier.deliver(&runtime, &recipients).await;
            }
            Some(MemberIdle::Park) | None => {}
        }
    }

    async fn handle_command(
        &self,
        info: &SwarmThreadInfo,
        command: Option<SwarmCommand>,
    ) -> Option<String> {
        let runtime = self.coordinator_runtime(info);
        let prompt = match command? {
            SwarmCommand::Usage => prompts::usage_prompt(),
            SwarmCommand::Start(task) => {
                let runtime = self
                    .service
                    .runtime_or_insert(&info.thread_id, info.state_dir.clone());
                let replaced = runtime.with_run(|run| {
                    let replaced = run.as_ref().is_some_and(|run| run.phase != Phase::Finished);
                    *run = Some(SwarmRun::new(
                        new_run_id(&info.thread_id),
                        info.thread_id.clone(),
                        task.clone(),
                    ));
                    replaced
                });
                runtime.save().await;
                let mut prompt =
                    prompts::coordinator_prompt(&task, runtime.board_path().as_deref());
                if replaced {
                    prompt.insert_str(
                        0,
                        "The previous unfinished /swarm run in this thread was replaced by a new one. Close its member agents if they are still open.\n\n",
                    );
                }
                prompt
            }
            SwarmCommand::Board => match runtime.as_ref().and_then(|runtime| {
                runtime.snapshot().map(|run| {
                    (
                        prompts::board_markdown(&run, Some(BOARD_VIEW_LIMIT)),
                        runtime.board_path(),
                    )
                })
            }) {
                Some((board, path)) => {
                    let mut prompt = format!(
                        "The user asked to see the /swarm board. Show the board below to the user as is, without commentary beyond one line, and do not take any other action.\n\n<swarm_board>\n{}\n</swarm_board>",
                        prompts::escape_xml(&board)
                    );
                    if let Some(path) = path {
                        let _ = write!(
                            prompt,
                            "\n\nAfter the board, tell the user they can follow the full live board in `{}`.",
                            path.display()
                        );
                    }
                    prompt
                }
                None => no_swarm_prompt("board"),
            },
            SwarmCommand::Status => match runtime.as_ref().and_then(|runtime| runtime.snapshot()) {
                Some(run) => format!(
                    "The user asked for the /swarm status. Summarize it for them from this authoritative state, without taking any other action:\n\n<swarm_state>\n{}\n</swarm_state>",
                    prompts::escape_xml(&prompts::status_markdown(&run))
                ),
                None => no_swarm_prompt("status"),
            },
            SwarmCommand::Say(message) => {
                let Some(runtime) = runtime else {
                    return Some(no_swarm_prompt("say"));
                };
                let posted = runtime.with_run(|run| {
                    let run = run.as_mut()?;
                    run.on_user_input();
                    Some(
                        run.post(USER, &[], MessageKind::Note, &message)
                            .inspect(|posted| {
                                // The coordinator reads the message in this turn.
                                if posted.recipients.iter().any(|r| r == COORDINATOR) {
                                    run.mark_delivered(COORDINATOR, posted.seq);
                                }
                            }),
                    )
                });
                match posted {
                    Some(Ok(posted)) => {
                        runtime.save().await;
                        let others = posted
                            .recipients
                            .iter()
                            .filter(|r| r.as_str() != COORDINATOR)
                            .cloned()
                            .collect::<Vec<_>>();
                        let woken = self.courier.deliver(&runtime, &others).await;
                        let for_coordinator = posted.recipients.iter().any(|r| r == COORDINATOR);
                        format!(
                            "The user posted this message to the /swarm board as @user (message #{}):\n\n<user_post>\n{}\n</user_post>\n\nDelivered to: {}. Woken now: {}. {}",
                            posted.seq,
                            prompts::escape_xml(&message),
                            if posted.recipients.is_empty() {
                                "nobody (no @mentions)".to_string()
                            } else {
                                posted.recipients.join(", ")
                            },
                            if woken.is_empty() {
                                "none".to_string()
                            } else {
                                woken.join(", ")
                            },
                            if for_coordinator {
                                "The message mentions you, so act on it."
                            } else {
                                "Confirm the delivery to the user in one line and end your turn unless the message needs your action."
                            }
                        )
                    }
                    Some(Err(error)) => format!(
                        "The user tried to post to the /swarm board with `/swarm say`, but the harness rejected it: {}. Tell the user.",
                        error.0
                    ),
                    None => no_swarm_prompt("say"),
                }
            }
            SwarmCommand::Stop => {
                let Some(runtime) = runtime else {
                    return Some(no_swarm_prompt("stop"));
                };
                runtime.with_run(|run| *run = None);
                runtime.save().await;
                "The user stopped the /swarm run. Close every member agent that is still open with close_agent, confirm that the swarm is stopped, and summarize what was completed.".to_string()
            }
        };
        Some(prompt)
    }
}

fn no_swarm_prompt(subcommand: &str) -> String {
    format!(
        "The user typed `/swarm {subcommand}`, but no swarm exists in this thread. Tell them to start one with `/swarm <task>`."
    )
}

impl<C> ThreadLifecycleContributor<C> for SwarmExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let config = (self.config)(input.config);
            let thread_id = input.thread_store.level_id().to_string();
            let parent_thread_id = input
                .session_source
                .parent_thread_id()
                .map(|parent| parent.to_string());
            self.service
                .register_thread(&thread_id, parent_thread_id.as_deref());
            let is_subagent = input.session_source.is_non_root_agent();
            input.thread_store.insert(SwarmThreadInfo {
                thread_id: thread_id.clone(),
                is_subagent,
                enabled: config.enabled,
                state_dir: config.state_dir.clone(),
            });
            if !config.enabled {
                return;
            }
            if is_subagent {
                // Count members spawned by a coordinator so it is not nudged to spawn them again
                // before they join.
                if let Some(parent) = parent_thread_id.as_deref()
                    && let Some(runtime) = self.service.runtime(parent)
                {
                    runtime.with_run(|run| {
                        if let Some(run) = run.as_mut() {
                            run.note_spawned_thread();
                        }
                    });
                }
                return;
            }
            let runtime = self
                .service
                .runtime_or_insert(&thread_id, config.state_dir.clone());
            if runtime
                .state_path()
                .as_ref()
                .is_some_and(|path| path.exists())
            {
                runtime.load().await;
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(info) = input.thread_store.get::<SwarmThreadInfo>() else {
                return;
            };
            let Some((runtime, name)) = self.participant(&info) else {
                return;
            };
            if input.cause != ThreadIdleCause::Completed {
                // An interrupted or failed agent parks without automatic follow-up.
                runtime.with_run(|run| {
                    if let Some(member) = run
                        .as_mut()
                        .and_then(|run| run.members.iter_mut().find(|m| m.name == name))
                        && member.activity == crate::state::Activity::Active
                    {
                        member.activity = crate::state::Activity::Parked;
                    }
                });
                runtime.save().await;
                return;
            }
            if name == COORDINATOR {
                self.on_coordinator_idle(runtime).await;
            } else {
                self.on_member_idle(runtime, name).await;
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(info) = input.thread_store.get::<SwarmThreadInfo>() else {
                return;
            };
            if let Some(runtime) = self.coordinator_runtime(&info) {
                runtime.save().await;
                self.service
                    .forget_coordinator(runtime.coordinator_thread_id());
            }
        })
    }
}

impl<C> TurnInputContributor for SwarmExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        _extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(info) = thread_store.get::<SwarmThreadInfo>() else {
                return Vec::new();
            };
            if info.is_subagent || !info.enabled {
                return Vec::new();
            }
            let text = input
                .user_input
                .iter()
                .filter_map(|item| match item {
                    UserInput::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                return Vec::new();
            }
            let prompt = match parse_swarm_command(&text) {
                Some(command) => self.handle_command(&info, Some(command)).await,
                None => {
                    let Some(runtime) = self.coordinator_runtime(&info) else {
                        return Vec::new();
                    };
                    let reminder = runtime.with_run(|run| {
                        let run = run.as_mut()?;
                        if run.phase == Phase::Finished {
                            return None;
                        }
                        run.on_user_input();
                        Some(prompts::user_turn_reminder(run))
                    });
                    if reminder.is_some() {
                        runtime.save().await;
                    }
                    reminder
                }
            };
            prompt
                .map(|prompt| {
                    vec![Box::new(context_fragment(prompt))
                        as Box<dyn ContextualUserFragment + Send>]
                })
                .unwrap_or_default()
        })
    }
}

impl<C> ToolContributor for SwarmExtension<C>
where
    C: Send + Sync + 'static,
{
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(info) = thread_store.get::<SwarmThreadInfo>() else {
            return Vec::new();
        };
        if !info.enabled {
            return Vec::new();
        }
        let kinds: &[SwarmToolKind] = if let Some(runtime) = self.coordinator_runtime(&info) {
            match runtime.with_run(|run| run.as_ref().map(|run| run.phase)) {
                Some(Phase::Forming) => &SwarmToolKind::COORDINATOR_FORMING,
                Some(Phase::Running) => &SwarmToolKind::COORDINATOR_RUNNING,
                Some(Phase::Finished) => &SwarmToolKind::COORDINATOR_FINISHED,
                None => &[],
            }
        } else if info.is_subagent {
            if let Some((runtime, name)) = self.service.membership(&info.thread_id) {
                let role = runtime.with_run(|run| {
                    let run = run.as_ref()?;
                    Some((run.phase, run.member(&name)?.role))
                });
                match role {
                    Some((Phase::Running, Role::Worker)) => &SwarmToolKind::WORKER,
                    Some((Phase::Running, Role::Verifier)) => &SwarmToolKind::VERIFIER,
                    Some(_) => &SwarmToolKind::MEMBER_FINISHED,
                    None => &[],
                }
            } else if self.service.has_joinable_run(&info.thread_id) {
                &SwarmToolKind::JOINING
            } else {
                &[]
            }
        } else {
            &[]
        };
        kinds
            .iter()
            .map(|kind| {
                Arc::new(SwarmTool::new(
                    *kind,
                    Arc::clone(&self.service),
                    Arc::clone(&self.courier),
                    info.thread_id.clone(),
                )) as Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>
            })
            .collect()
    }
}

impl<C> ToolLifecycleContributor for SwarmExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let is_edit = input.tool_name.is_default_namespace()
                && FILE_EDIT_TOOLS.contains(&input.tool_name.name.as_str());
            if !is_edit {
                return;
            }
            let Some(info) = input.thread_store.get::<SwarmThreadInfo>() else {
                return;
            };
            if self.participant(&info).is_none() {
                return;
            }
            let paths = edit_input(input.payload)
                .map(|patch| patch_paths(&patch))
                .unwrap_or_default();
            let pending = input.thread_store.get_or_init(PendingEdits::default);
            pending
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(input.call_id.to_string(), paths);
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let is_edit = input.tool_name.is_default_namespace()
                && FILE_EDIT_TOOLS.contains(&input.tool_name.name.as_str());
            if !is_edit {
                return;
            }
            let paths = input
                .thread_store
                .get::<PendingEdits>()
                .and_then(|pending| {
                    pending
                        .0
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(input.call_id)
                });
            if !matches!(input.outcome, ToolCallOutcome::Completed { success: true }) {
                return;
            }
            let Some(info) = input.thread_store.get::<SwarmThreadInfo>() else {
                return;
            };
            let Some((runtime, name)) = self.participant(&info) else {
                return;
            };
            let paths = paths.unwrap_or_default();
            let posted =
                runtime.with_run(|run| run.as_mut().and_then(|run| run.record_edit(&name, &paths)));
            runtime.save().await;
            if let Some(posted) = posted {
                self.courier.deliver(&runtime, &posted.recipients).await;
            }
        })
    }
}

/// Installs the `/swarm` harness: roster and board state, member and coordinator tools,
/// mention-driven waking of parked agents, file ownership tracking, and persistence.
pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    service: Arc<SwarmService>,
    thread_manager: Weak<ThreadManager>,
    config: impl Fn(&C) -> SwarmExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(SwarmExtension {
        service,
        courier: Arc::new(Courier::new(thread_manager)),
        config: Arc::new(config),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_input_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.tool_contributor(extension);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_swarm_commands() {
        assert_eq!(
            parse_swarm_command("/swarm build the login flow"),
            Some(SwarmCommand::Start("build the login flow".to_string()))
        );
        assert_eq!(parse_swarm_command("/swarm"), Some(SwarmCommand::Usage));
        assert_eq!(
            parse_swarm_command("/swarm board"),
            Some(SwarmCommand::Board)
        );
        assert_eq!(
            parse_swarm_command("/swarm STATUS"),
            Some(SwarmCommand::Status)
        );
        assert_eq!(parse_swarm_command("/swarm stop"), Some(SwarmCommand::Stop));
        assert_eq!(
            parse_swarm_command("/swarm say @api use port 8080"),
            Some(SwarmCommand::Say("@api use port 8080".to_string()))
        );
        // A task that starts with a subcommand word is still a task.
        assert_eq!(
            parse_swarm_command("/swarm board game engine in Rust"),
            Some(SwarmCommand::Start("board game engine in Rust".to_string()))
        );
        assert_eq!(
            parse_swarm_command("/swarm say"),
            Some(SwarmCommand::Start("say".to_string()))
        );
        assert_eq!(parse_swarm_command("/swarming"), None);
        assert_eq!(parse_swarm_command("please /swarm this"), None);
    }

    #[test]
    fn edit_input_reads_custom_and_function_payloads() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch";
        assert_eq!(
            edit_input(&ToolPayload::Custom {
                input: patch.to_string()
            })
            .map(|patch| patch_paths(&patch)),
            Some(vec!["src/a.rs".to_string()])
        );
        let arguments = serde_json::json!({ "input": patch }).to_string();
        assert_eq!(
            edit_input(&ToolPayload::Function { arguments }).map(|patch| patch_paths(&patch)),
            Some(vec!["src/a.rs".to_string()])
        );
    }
}
