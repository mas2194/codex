use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_core::StartIfIdleSubmission;
use codex_core::ThreadManager;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
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
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_protocol::ThreadId;
use codex_protocol::user_input::UserInput;

use crate::prompts;
use crate::service::TeamworkRuntime;
use crate::service::TeamworkService;
use crate::state::ContinuationDecision;
use crate::state::Phase;
use crate::state::TeamworkRun;
use crate::tools::TeamworkTool;
use crate::tools::TeamworkToolKind;

const CONTEXT_SOURCE: &str = "teamwork";
const TURN_TRIGGER: &str = "teamwork";
/// Tools whose successful use means the calling thread edited files.
const FILE_EDIT_TOOLS: [&str; 1] = ["apply_patch"];

/// Host configuration for the `/teamwork` extension.
#[derive(Clone, Debug)]
pub struct TeamworkExtensionConfig {
    pub enabled: bool,
    /// Directory for persisted run state, usually `$CODEX_HOME/teamwork`. `None` keeps runs in
    /// memory only.
    pub state_dir: Option<PathBuf>,
}

/// Per-thread facts captured when the host starts the thread.
#[derive(Clone, Debug)]
struct TeamworkThreadInfo {
    thread_id: String,
    is_subagent: bool,
    enabled: bool,
    state_dir: Option<PathBuf>,
}

struct TeamworkExtension<C> {
    service: Arc<TeamworkService>,
    thread_manager: Weak<ThreadManager>,
    config: Arc<dyn Fn(&C) -> TeamworkExtensionConfig + Send + Sync>,
}

impl<C> TeamworkExtension<C> {
    fn lead_runtime(&self, thread_store: &ExtensionData) -> Option<Arc<TeamworkRuntime>> {
        let info = thread_store.get::<TeamworkThreadInfo>()?;
        if info.is_subagent || !info.enabled {
            return None;
        }
        self.service.runtime(&info.thread_id)
    }

    async fn continue_lead(&self, runtime: &TeamworkRuntime, prompt: String) {
        let Some(thread_manager) = self.thread_manager.upgrade() else {
            return;
        };
        let Ok(thread_id) = ThreadId::from_string(runtime.lead_thread_id()) else {
            return;
        };
        let Ok(thread) = thread_manager.get_thread(thread_id).await else {
            return;
        };
        let request = TurnInputRequest::new(TurnInput::ResponseItem(context_item(prompt)))
            .on_start(TurnStartOptions {
                turn_trigger: Some(TURN_TRIGGER.to_string()),
                ..Default::default()
            });
        match thread.start_turn_if_idle(request).await {
            Ok(StartIfIdleSubmission::Started { .. }) => {}
            Ok(StartIfIdleSubmission::NotSubmitted { reason }) => {
                tracing::debug!(?reason, "teamwork continuation was not submitted");
            }
            Err(error) => {
                tracing::debug!(%error, "teamwork continuation failed to submit");
            }
        }
    }
}

fn context_item(prompt: String) -> codex_protocol::models::ResponseItem {
    ContextualUserFragment::into(context_fragment(prompt))
}

fn context_fragment(prompt: String) -> InternalModelContextFragment {
    InternalModelContextFragment::new(InternalContextSource::from_static(CONTEXT_SOURCE), prompt)
}

/// What the user asked for with a `/teamwork` message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TeamworkCommand {
    Start(String),
    Status,
    Stop,
    Usage,
}

/// Parses a user message that starts with `/teamwork`.
pub(crate) fn parse_teamwork_command(text: &str) -> Option<TeamworkCommand> {
    let rest = text.trim_start().strip_prefix("/teamwork")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let argument = rest.trim();
    Some(match argument.to_ascii_lowercase().as_str() {
        "" => TeamworkCommand::Usage,
        "status" => TeamworkCommand::Status,
        "stop" | "cancel" => TeamworkCommand::Stop,
        _ => TeamworkCommand::Start(argument.to_string()),
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
    format!("{prefix}{millis}")
}

fn state_path(state_dir: Option<&PathBuf>, thread_id: &str) -> Option<PathBuf> {
    state_dir.map(|dir| dir.join(format!("{thread_id}.json")))
}

impl<C> ThreadLifecycleContributor<C> for TeamworkExtension<C>
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
            input.thread_store.insert(TeamworkThreadInfo {
                thread_id: thread_id.clone(),
                is_subagent,
                enabled: config.enabled,
                state_dir: config.state_dir.clone(),
            });
            if is_subagent || !config.enabled {
                return;
            }
            let path = state_path(config.state_dir.as_ref(), &thread_id);
            if path.as_ref().is_some_and(|path| path.exists()) {
                let runtime = self.service.runtime_or_insert(&thread_id, path);
                runtime.load().await;
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.cause != ThreadIdleCause::Completed {
                return;
            }
            let Some(runtime) = self.lead_runtime(input.thread_store) else {
                return;
            };
            let decision = runtime.with_run(|run| run.as_mut().map(TeamworkRun::on_idle));
            match decision {
                Some(ContinuationDecision::Continue) => {
                    runtime.save().await;
                    let Some(run) = runtime.snapshot() else {
                        return;
                    };
                    self.continue_lead(&runtime, prompts::continuation_prompt(&run))
                        .await;
                }
                Some(ContinuationDecision::BlockedNoProgress { reason }) => {
                    tracing::info!("teamwork run blocked without progress: {reason}");
                    runtime.save().await;
                }
                Some(ContinuationDecision::Skip) | None => {}
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = self.lead_runtime(input.thread_store) {
                runtime.save().await;
                self.service.forget_lead(runtime.lead_thread_id());
            }
        })
    }
}

impl<C> TurnInputContributor for TeamworkExtension<C>
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
            let Some(info) = thread_store.get::<TeamworkThreadInfo>() else {
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

            let prompt = match parse_teamwork_command(&text) {
                Some(TeamworkCommand::Usage) => prompts::usage_prompt(),
                Some(TeamworkCommand::Start(goal)) => {
                    let runtime = self.service.runtime_or_insert(
                        &info.thread_id,
                        state_path(info.state_dir.as_ref(), &info.thread_id),
                    );
                    let replaced = runtime.with_run(|run| {
                        let replaced = run
                            .as_ref()
                            .is_some_and(|run| run.phase != Phase::Complete);
                        *run = Some(TeamworkRun::new(
                            new_run_id(&info.thread_id),
                            info.thread_id.clone(),
                            goal.clone(),
                        ));
                        replaced
                    });
                    runtime.save().await;
                    let mut prompt = prompts::lead_prompt(&goal);
                    if replaced {
                        prompt.insert_str(
                            0,
                            "The previous unfinished /teamwork run in this thread was replaced by a new one.\n\n",
                        );
                    }
                    prompt
                }
                Some(TeamworkCommand::Status) => match self.service.runtime(&info.thread_id).and_then(|runtime| runtime.snapshot()) {
                    Some(run) => format!(
                        "The user asked for the /teamwork status. Summarize it for them from this authoritative state:\n\n<teamwork_state>\n{}\n</teamwork_state>",
                        prompts::status_markdown(&run)
                    ),
                    None => "The user asked for the /teamwork status, but no run exists in this thread. Tell them how to start one with `/teamwork <goal>`.".to_string(),
                },
                Some(TeamworkCommand::Stop) => {
                    if let Some(runtime) = self.service.runtime(&info.thread_id) {
                        runtime.with_run(|run| *run = None);
                        runtime.save().await;
                    }
                    "The user stopped the /teamwork run. Close any sub-agents that are still running, confirm that the run is stopped, and summarize what was completed.".to_string()
                }
                None => {
                    let Some(runtime) = self.service.runtime(&info.thread_id) else {
                        return Vec::new();
                    };
                    let reminder = runtime.with_run(|run| {
                        let run = run.as_mut()?;
                        if run.phase == Phase::Complete {
                            return None;
                        }
                        let resumed = run.resume_after_user_input();
                        Some((resumed, prompts::user_turn_reminder(run, resumed)))
                    });
                    let Some((resumed, reminder)) = reminder else {
                        return Vec::new();
                    };
                    if resumed {
                        runtime.save().await;
                    }
                    reminder
                }
            };
            vec![Box::new(context_fragment(prompt)) as Box<dyn ContextualUserFragment + Send>]
        })
    }
}

impl<C> ToolContributor for TeamworkExtension<C>
where
    C: Send + Sync + 'static,
{
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(info) = thread_store.get::<TeamworkThreadInfo>() else {
            return Vec::new();
        };
        if !info.enabled {
            return Vec::new();
        }
        let kinds: &[TeamworkToolKind] = if info.is_subagent {
            if !self.service.has_open_tickets_for(&info.thread_id) {
                return Vec::new();
            }
            &[TeamworkToolKind::SubmitVerdict]
        } else {
            let has_run = self
                .service
                .runtime(&info.thread_id)
                .is_some_and(|runtime| runtime.with_run(|run| run.is_some()));
            if !has_run {
                return Vec::new();
            }
            &TeamworkToolKind::LEAD_TOOLS
        };
        kinds
            .iter()
            .map(|kind| {
                Arc::new(TeamworkTool::new(
                    *kind,
                    Arc::clone(&self.service),
                    info.thread_id.clone(),
                )) as Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>
            })
            .collect()
    }
}

impl<C> ToolLifecycleContributor for TeamworkExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let edited = input.tool_name.is_default_namespace()
                && FILE_EDIT_TOOLS.contains(&input.tool_name.name.as_str())
                && matches!(input.outcome, ToolCallOutcome::Completed { success: true });
            if !edited {
                return;
            }
            if let Some(info) = input.thread_store.get::<TeamworkThreadInfo>()
                && info.is_subagent
            {
                self.service.mark_editor(&info.thread_id);
            }
        })
    }
}

/// Installs the `/teamwork` harness: run state, lead and gate tools, automatic continuation, and
/// persistence.
pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    service: Arc<TeamworkService>,
    thread_manager: Weak<ThreadManager>,
    config: impl Fn(&C) -> TeamworkExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(TeamworkExtension {
        service,
        thread_manager,
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
    fn parses_teamwork_commands() {
        assert_eq!(
            parse_teamwork_command("/teamwork port the parser"),
            Some(TeamworkCommand::Start("port the parser".to_string()))
        );
        assert_eq!(
            parse_teamwork_command("  /teamwork\n multi\nline goal "),
            Some(TeamworkCommand::Start("multi\nline goal".to_string()))
        );
        assert_eq!(
            parse_teamwork_command("/teamwork"),
            Some(TeamworkCommand::Usage)
        );
        assert_eq!(
            parse_teamwork_command("/teamwork status"),
            Some(TeamworkCommand::Status)
        );
        assert_eq!(
            parse_teamwork_command("/teamwork STOP"),
            Some(TeamworkCommand::Stop)
        );
        assert_eq!(parse_teamwork_command("/teamworks are fun"), None);
        assert_eq!(parse_teamwork_command("please /teamwork this"), None);
    }

    #[test]
    fn run_ids_are_prefixed_with_the_thread() {
        let run_id = new_run_id("0193-abcd-ef01-2345");
        assert!(run_id.starts_with("0193abcd"));
    }
}
