//! The `/swarm` run as a harness-owned state machine.
//!
//! A run has one coordinator (the thread that typed `/swarm`), a roster of named members
//! (workers and verifiers), and one append-only message board. Members address each other with
//! `@name`, `@group`, or `@all`. The harness, not the model, decides:
//! - who a message is delivered to, and so who is woken from parking;
//! - whether a roster is valid (unique names, non-overlapping worker files, verifier coverage);
//! - when a worker's work counts as verified (only an assigned verifier that did not edit files);
//! - when the run may finish (every active worker verified, violations acknowledged).
//!
//! Everything here is synchronous and side-effect free so it can be tested directly. Delivery
//! (waking threads) lives in the extension.

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Name the coordinator uses on the board and in mentions.
pub const COORDINATOR: &str = "coordinator";
/// Sender of messages the harness posts on its own behalf.
pub const HARNESS: &str = "harness";
/// Sender of messages the user posts with `/swarm say`.
pub const USER: &str = "user";
/// Mention that reaches every active member and the coordinator.
pub const ALL: &str = "all";
/// Built-in group of every active worker.
pub const WORKERS_GROUP: &str = "workers";
/// Built-in group of every active verifier.
pub const VERIFIERS_GROUP: &str = "verifiers";
const RESERVED_NAMES: [&str; 6] = [
    COORDINATOR,
    HARNESS,
    USER,
    ALL,
    WORKERS_GROUP,
    VERIFIERS_GROUP,
];

/// A verifier checks at most this many workers (Antigravity's 2-4 workers per verifier).
pub const MAX_WORKERS_PER_VERIFIER: usize = 4;
/// Failed verifications before a worker is blocked for the coordinator.
pub const MAX_FIX_ROUNDS: u32 = 3;
/// Times the harness tells the coordinator that a member parked while owing work.
pub const MAX_IDLE_NOTICES: u32 = 3;
/// Coordinator continuations without any state change before the run stalls for the user.
pub const MAX_COORDINATOR_NUDGES: u32 = 3;
/// Upper bound on roster size, to keep token use predictable.
pub const MAX_MEMBERS: usize = 16;
const MAX_NAME_LEN: usize = 24;
const MAX_BODY_LEN: usize = 8_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwarmError(pub String);

impl fmt::Display for SwarmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(message: impl Into<String>) -> Result<T, SwarmError> {
    Err(SwarmError(message.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The coordinator is reading the task and has not recorded a roster yet.
    Forming,
    /// Members are working, verifying, or parked.
    Running,
    /// The coordinator finished the run after every worker was verified.
    Finished,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Forming => "forming",
            Phase::Running => "running",
            Phase::Finished => "finished",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Worker,
    Verifier,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Worker => "worker",
            Role::Verifier => "verifier",
        }
    }
}

/// Whether a member is consuming tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// In the roster, not yet joined from a spawned thread.
    Invited,
    /// Running a turn.
    Active,
    /// Idle at zero token cost until a message mentions it.
    Parked,
    /// Removed by the coordinator. Receives nothing.
    Retired,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Invited => "invited",
            Activity::Active => "active",
            Activity::Parked => "parked",
            Activity::Retired => "retired",
        }
    }
}

/// Where a worker's assignment stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    InProgress,
    AwaitingVerification,
    Verified,
    /// Failed verification too often; waits for the coordinator to reopen it.
    Blocked,
}

impl Progress {
    pub fn as_str(self) -> &'static str {
        match self {
            Progress::InProgress => "in_progress",
            Progress::AwaitingVerification => "awaiting_verification",
            Progress::Verified => "verified",
            Progress::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Note,
    Question,
    Handoff,
    Report,
    Defect,
    Verdict,
    System,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Note => "note",
            MessageKind::Question => "question",
            MessageKind::Handoff => "handoff",
            MessageKind::Report => "report",
            MessageKind::Defect => "defect",
            MessageKind::Verdict => "verdict",
            MessageKind::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// One message on the board. `recipients` is resolved by the harness when the message is posted
/// and decides who is woken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardMessage {
    pub seq: u64,
    pub from: String,
    pub kind: MessageKind,
    pub recipients: Vec<String>,
    pub body: String,
}

/// A member as the coordinator describes it in `swarm_define_roster` or `swarm_add_members`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSpec {
    pub name: String,
    pub role: Role,
    pub assignment: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub verifies: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastVerdict {
    pub verifier: String,
    pub verdict: Verdict,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub role: Role,
    pub assignment: String,
    pub files: Vec<String>,
    pub groups: Vec<String>,
    /// Workers this verifier checks. Empty for workers.
    pub verifies: Vec<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub join_token: String,
    pub thread_id: Option<String>,
    pub activity: Activity,
    /// Workers only.
    pub progress: Option<Progress>,
    pub fix_rounds: u32,
    pub report: Option<String>,
    pub last_verdict: Option<LastVerdict>,
    /// Highest board sequence number handed to this member.
    pub delivered_seq: u64,
    /// Whether this member changed files with `apply_patch`.
    pub edited: bool,
    pub idle_notices: u32,
}

impl Member {
    fn is_active_member(&self) -> bool {
        self.activity != Activity::Retired
    }

    fn in_group(&self, group: &str) -> bool {
        match group {
            WORKERS_GROUP => self.role == Role::Worker,
            VERIFIERS_GROUP => self.role == Role::Verifier,
            _ => self.groups.iter().any(|g| g == group),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub agent: String,
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmRun {
    pub id: String,
    pub coordinator_thread_id: String,
    pub task: String,
    pub phase: Phase,
    pub members: Vec<Member>,
    pub board: Vec<BoardMessage>,
    pub coordinator_delivered_seq: u64,
    pub violations: Vec<Violation>,
    /// Sub-agent threads the coordinator started since the roster was recorded.
    pub spawned_threads: u32,
    pub revision: u64,
    pub nudges_without_progress: u32,
    pub last_nudge_revision: u64,
    /// The coordinator stopped being nudged; a user message resets this.
    pub stalled: bool,
    pub final_summary: Option<String>,
}

/// Result of posting a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub seq: u64,
    pub recipients: Vec<String>,
}

/// What to do when a member's turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberIdle {
    /// New messages are waiting; deliver them in a new turn.
    Wake,
    /// The member parked while owing work; the harness posted a notice to these recipients.
    Notified(Vec<String>),
    /// Nothing to do: the member stays parked at zero token cost.
    Park,
}

/// What to do when the coordinator's turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinatorIdle {
    /// Messages mention the coordinator; deliver them.
    Wake,
    /// Continue the coordinator with this instruction.
    Nudge(String),
    /// Members are working; the coordinator parks until mentioned.
    Park,
    /// Too many continuations without progress; wait for the user.
    Stalled,
    /// The run is finished.
    Done,
}

impl SwarmRun {
    pub fn new(id: String, coordinator_thread_id: String, task: String) -> Self {
        Self {
            id,
            coordinator_thread_id,
            task,
            phase: Phase::Forming,
            members: Vec::new(),
            board: Vec::new(),
            coordinator_delivered_seq: 0,
            violations: Vec::new(),
            spawned_threads: 0,
            revision: 0,
            nudges_without_progress: 0,
            last_nudge_revision: 0,
            stalled: false,
            final_summary: None,
        }
    }

    fn bump(&mut self) {
        self.revision += 1;
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.name == name)
    }

    fn member_mut(&mut self, name: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|member| member.name == name)
    }

    pub fn member_by_thread(&self, thread_id: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|member| member.thread_id.as_deref() == Some(thread_id))
    }

    /// The board name of the agent running in `thread_id`, if it belongs to this run.
    pub fn agent_name(&self, thread_id: &str) -> Option<String> {
        if thread_id == self.coordinator_thread_id {
            return Some(COORDINATOR.to_string());
        }
        self.member_by_thread(thread_id)
            .map(|member| member.name.clone())
    }

    pub fn invited_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.activity == Activity::Invited)
            .count()
    }

    pub fn active_workers(&self) -> impl Iterator<Item = &Member> {
        self.members
            .iter()
            .filter(|member| member.role == Role::Worker && member.is_active_member())
    }

    fn verifiers_of(&self, worker: &str) -> Vec<String> {
        self.members
            .iter()
            .filter(|member| {
                member.role == Role::Verifier
                    && member.is_active_member()
                    && member.verifies.iter().any(|name| name == worker)
            })
            .map(|member| member.name.clone())
            .collect()
    }

    pub fn all_workers_verified(&self) -> bool {
        let mut workers = self.active_workers().peekable();
        workers.peek().is_some()
            && workers.all(|worker| worker.progress == Some(Progress::Verified))
    }

    // ----- Roster -------------------------------------------------------------------------

    /// Records the first roster and starts the run. Returns the new members.
    pub fn define_roster(&mut self, specs: Vec<MemberSpec>) -> Result<Vec<Member>, SwarmError> {
        if self.phase != Phase::Forming {
            return err(
                "the roster is already recorded; use `swarm_add_members` or `swarm_retire_member`",
            );
        }
        if !specs.iter().any(|spec| spec.role == Role::Worker) {
            return err("a swarm needs at least one worker");
        }
        if !specs.iter().any(|spec| spec.role == Role::Verifier) {
            return err(
                "a swarm needs at least one verifier; plan one verifier for every 2-4 workers",
            );
        }
        let added = self.add_specs(specs)?;
        self.phase = Phase::Running;
        let names = added
            .iter()
            .map(|member| format!("@{} ({})", member.name, member.role.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        self.post_internal(
            HARNESS,
            MessageKind::System,
            Vec::new(),
            format!("Swarm formed for: {}\nRoster: {names}", self.task),
        );
        Ok(added)
    }

    /// Adds members to a running swarm.
    pub fn add_members(&mut self, specs: Vec<MemberSpec>) -> Result<Vec<Member>, SwarmError> {
        if self.phase != Phase::Running {
            return err(format!(
                "members can only be added while the swarm is running (phase: {})",
                self.phase.as_str()
            ));
        }
        let added = self.add_specs(specs)?;
        let names = added
            .iter()
            .map(|member| format!("@{} ({})", member.name, member.role.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        self.post_internal(
            HARNESS,
            MessageKind::System,
            Vec::new(),
            format!("Joined the roster: {names}"),
        );
        Ok(added)
    }

    fn add_specs(&mut self, specs: Vec<MemberSpec>) -> Result<Vec<Member>, SwarmError> {
        if specs.is_empty() {
            return err("no members given");
        }
        if self.members.len() + specs.len() > MAX_MEMBERS {
            return err(format!(
                "a swarm holds at most {MAX_MEMBERS} members; keep the roster small"
            ));
        }
        let mut members = self.members.clone();
        let mut new_names = Vec::new();
        for spec in specs {
            let name = normalize_name(&spec.name)?;
            if members.iter().any(|member| member.name == name) {
                return err(format!("member name `{name}` is already taken"));
            }
            let mut groups = Vec::new();
            for group in &spec.groups {
                let group = normalize_name(group)?;
                if !groups.contains(&group) {
                    groups.push(group);
                }
            }
            if spec.assignment.trim().is_empty() {
                return err(format!("member `{name}` needs an assignment"));
            }
            let files = spec
                .files
                .iter()
                .map(|file| normalize_path(file))
                .filter(|file| !file.is_empty())
                .collect::<Vec<_>>();
            let verifies = spec
                .verifies
                .iter()
                .map(|worker| normalize_name(worker.trim_start_matches('@')))
                .collect::<Result<Vec<_>, _>>()?;
            match spec.role {
                Role::Worker => {
                    if files.is_empty() {
                        return err(format!(
                            "worker `{name}` must own at least one file or directory"
                        ));
                    }
                    if !verifies.is_empty() {
                        return err(format!("worker `{name}` cannot verify other members"));
                    }
                }
                Role::Verifier => {
                    if !files.is_empty() {
                        return err(format!(
                            "verifier `{name}` cannot own files; verifiers never edit"
                        ));
                    }
                }
            }
            let token = format!("swarm-{}-{name}-{}", self.id, self.revision + 1);
            new_names.push(name.clone());
            members.push(Member {
                name,
                role: spec.role,
                assignment: spec.assignment.trim().to_string(),
                files,
                groups,
                verifies,
                model: spec.model.filter(|model| !model.trim().is_empty()),
                reasoning_effort: spec
                    .reasoning_effort
                    .filter(|effort| !effort.trim().is_empty()),
                join_token: token,
                thread_id: None,
                activity: Activity::Invited,
                progress: (spec.role == Role::Worker).then_some(Progress::InProgress),
                fix_rounds: 0,
                report: None,
                last_verdict: None,
                delivered_seq: 0,
                edited: false,
                idle_notices: 0,
            });
        }

        // Group names must not collide with member names.
        let names = members
            .iter()
            .map(|member| member.name.as_str())
            .collect::<BTreeSet<_>>();
        for member in &members {
            if let Some(group) = member
                .groups
                .iter()
                .find(|group| names.contains(group.as_str()))
            {
                return err(format!(
                    "group `{group}` of `{}` has the same name as a member",
                    member.name
                ));
            }
        }

        check_file_ownership(&members)?;
        assign_coverage(&mut members)?;

        self.members = members;
        self.bump();
        Ok(self
            .members
            .iter()
            .filter(|member| new_names.contains(&member.name))
            .cloned()
            .collect())
    }

    pub fn retire(&mut self, name: &str, reason: &str) -> Result<(), SwarmError> {
        if self.phase != Phase::Running {
            return err("the swarm is not running");
        }
        let Some(member) = self.member(name) else {
            return err(format!("no member named `{name}`"));
        };
        if member.activity == Activity::Retired {
            return err(format!("`{name}` is already retired"));
        }
        let role = member.role;
        if role == Role::Verifier {
            // Make sure every worker stays covered after the verifier leaves.
            let mut members = self.members.clone();
            if let Some(member) = members.iter_mut().find(|member| member.name == name) {
                member.activity = Activity::Retired;
                member.verifies.clear();
            }
            assign_coverage(&mut members)
                .map_err(|error| SwarmError(format!("cannot retire `{name}`: {}", error.0)))?;
            self.members = members;
        } else {
            for member in &mut self.members {
                if member.name == name {
                    member.activity = Activity::Retired;
                }
                member.verifies.retain(|worker| worker != name);
            }
        }
        let reason = reason.trim();
        self.post_internal(
            COORDINATOR,
            MessageKind::System,
            Vec::new(),
            format!("Retired @{name}: {reason}"),
        );
        Ok(())
    }

    /// Binds a spawned thread to the member that owns `token`.
    pub fn join(&mut self, token: &str, thread_id: &str) -> Result<Member, SwarmError> {
        if self.phase == Phase::Finished {
            return err("the swarm already finished");
        }
        if self.member_by_thread(thread_id).is_some() {
            return err("this agent already joined the swarm");
        }
        let Some(member) = self.members.iter_mut().find(|m| m.join_token == token) else {
            return err("unknown join token");
        };
        match member.activity {
            Activity::Retired => return err(format!("`{}` was retired", member.name)),
            Activity::Invited => {}
            Activity::Active | Activity::Parked => {
                return err(format!(
                    "`{}` already joined from another agent",
                    member.name
                ));
            }
        }
        member.thread_id = Some(thread_id.to_string());
        member.activity = Activity::Active;
        let member = member.clone();
        self.bump();
        Ok(member)
    }

    pub fn note_spawned_thread(&mut self) {
        if self.phase == Phase::Running {
            self.spawned_threads += 1;
        }
    }

    // ----- Board --------------------------------------------------------------------------

    fn next_seq(&self) -> u64 {
        self.board.last().map_or(1, |message| message.seq + 1)
    }

    fn post_internal(
        &mut self,
        from: &str,
        kind: MessageKind,
        recipients: Vec<String>,
        body: String,
    ) -> Posted {
        let seq = self.next_seq();
        self.board.push(BoardMessage {
            seq,
            from: from.to_string(),
            kind,
            recipients: recipients.clone(),
            body,
        });
        self.bump();
        Posted { seq, recipients }
    }

    /// Posts a message from `from` (a member name, the coordinator, or the user). Recipients
    /// come from `@mentions` in the body plus the explicit `to` list.
    pub fn post(
        &mut self,
        from: &str,
        to: &[String],
        kind: MessageKind,
        body: &str,
    ) -> Result<Posted, SwarmError> {
        if self.phase == Phase::Finished {
            return err("the swarm already finished");
        }
        let body = body.trim();
        if body.is_empty() {
            return err("the message is empty");
        }
        if body.len() > MAX_BODY_LEN {
            return err(format!(
                "the message is longer than {MAX_BODY_LEN} bytes; summarize it and point to files instead"
            ));
        }
        if kind == MessageKind::System || kind == MessageKind::Verdict {
            return err("`system` and `verdict` messages are posted by the harness");
        }
        let mut mentions = extract_mentions(body);
        for target in to {
            let target = target.trim().trim_start_matches('@').to_ascii_lowercase();
            if !target.is_empty() && !mentions.contains(&target) {
                mentions.push(target);
            }
        }
        let recipients = self.resolve(from, &mentions)?;
        Ok(self.post_internal(from, kind, recipients, body.to_string()))
    }

    /// Expands mentions into recipient names, excluding the sender.
    pub fn resolve(&self, from: &str, mentions: &[String]) -> Result<Vec<String>, SwarmError> {
        let mut recipients = Vec::new();
        let push = |name: &str, recipients: &mut Vec<String>| {
            if name != from && !recipients.iter().any(|existing| existing == name) {
                recipients.push(name.to_string());
            }
        };
        for mention in mentions {
            match mention.as_str() {
                COORDINATOR => push(COORDINATOR, &mut recipients),
                ALL => {
                    push(COORDINATOR, &mut recipients);
                    for member in self.members.iter().filter(|m| m.is_active_member()) {
                        push(&member.name, &mut recipients);
                    }
                }
                name => {
                    if let Some(member) = self.member(name) {
                        if member.activity == Activity::Retired {
                            return err(format!("@{name} was retired and receives no messages"));
                        }
                        push(name, &mut recipients);
                        continue;
                    }
                    let group = self
                        .members
                        .iter()
                        .filter(|member| member.is_active_member() && member.in_group(name))
                        .map(|member| member.name.clone())
                        .collect::<Vec<_>>();
                    if group.is_empty() {
                        return err(format!(
                            "@{name} is not a member or group of this swarm; valid mentions: {}",
                            self.valid_mentions().join(", ")
                        ));
                    }
                    for member in group {
                        push(&member, &mut recipients);
                    }
                }
            }
        }
        Ok(recipients)
    }

    pub fn valid_mentions(&self) -> Vec<String> {
        let mut mentions = vec![
            format!("@{COORDINATOR}"),
            format!("@{ALL}"),
            format!("@{WORKERS_GROUP}"),
            format!("@{VERIFIERS_GROUP}"),
        ];
        let mut groups = BTreeSet::new();
        for member in self.members.iter().filter(|m| m.is_active_member()) {
            mentions.push(format!("@{}", member.name));
            groups.extend(member.groups.iter().cloned());
        }
        mentions.extend(groups.into_iter().map(|group| format!("@{group}")));
        mentions
    }

    fn delivered_seq(&self, name: &str) -> u64 {
        if name == COORDINATOR {
            self.coordinator_delivered_seq
        } else {
            self.member(name).map_or(0, |member| member.delivered_seq)
        }
    }

    /// Messages that mention `name` and were not delivered yet.
    pub fn pending_for(&self, name: &str) -> Vec<BoardMessage> {
        let delivered = self.delivered_seq(name);
        self.board
            .iter()
            .filter(|message| {
                message.seq > delivered && message.recipients.iter().any(|r| r == name)
            })
            .cloned()
            .collect()
    }

    pub fn mark_delivered(&mut self, name: &str, upto: u64) {
        if name == COORDINATOR {
            self.coordinator_delivered_seq = self.coordinator_delivered_seq.max(upto);
        } else if let Some(member) = self.member_mut(name) {
            member.delivered_seq = member.delivered_seq.max(upto);
        }
    }

    pub fn latest_seq(&self) -> u64 {
        self.board.last().map_or(0, |message| message.seq)
    }

    /// Marks a member as running a turn, for example after the harness woke it.
    pub fn mark_active(&mut self, name: &str) {
        if let Some(member) = self.member_mut(name)
            && member.activity == Activity::Parked
        {
            member.activity = Activity::Active;
        }
    }

    // ----- Work and verification ----------------------------------------------------------

    /// A worker reports its assignment done. Returns the verifiers that were notified.
    pub fn report_done(&mut self, worker: &str, summary: &str) -> Result<Posted, SwarmError> {
        self.require_running()?;
        let Some(member) = self.member(worker) else {
            return err(format!("no member named `{worker}`"));
        };
        if member.role != Role::Worker {
            return err("only workers report work as done");
        }
        match member.progress {
            Some(Progress::InProgress) => {}
            Some(Progress::AwaitingVerification) => {
                return err(
                    "your work is already waiting for verification; park until a verifier replies",
                );
            }
            Some(Progress::Verified) => {
                return err("your work is already verified");
            }
            Some(Progress::Blocked) | None => {
                return err("your assignment is blocked; wait for the coordinator to reopen it");
            }
        }
        if summary.trim().is_empty() {
            return err("the report needs a summary of changes and the checks you ran");
        }
        let verifiers = self.verifiers_of(worker);
        if verifiers.is_empty() {
            return err("no verifier covers you; ask @coordinator to add one");
        }
        if let Some(member) = self.member_mut(worker) {
            member.progress = Some(Progress::AwaitingVerification);
            member.report = Some(summary.trim().to_string());
            member.idle_notices = 0;
        }
        let mentions = verifiers
            .iter()
            .map(|name| format!("@{name}"))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(self.post_internal(
            worker,
            MessageKind::Report,
            verifiers,
            format!("{mentions} ready for verification.\n\n{}", summary.trim()),
        ))
    }

    /// A verifier records a verdict on a worker. Returns the post that announces it.
    pub fn verify(
        &mut self,
        verifier: &str,
        worker: &str,
        verdict: Verdict,
        findings: &str,
        evidence: &str,
    ) -> Result<Posted, SwarmError> {
        self.require_running()?;
        let worker = worker.trim().trim_start_matches('@').to_ascii_lowercase();
        let Some(reviewer) = self.member(verifier) else {
            return err(format!("no member named `{verifier}`"));
        };
        if reviewer.role != Role::Verifier {
            return err("only verifiers record verdicts");
        }
        if reviewer.edited {
            return err(
                "you edited files, so you cannot act as an independent verifier; tell @coordinator",
            );
        }
        if !reviewer.verifies.contains(&worker) {
            return err(format!(
                "you are not assigned to verify `{worker}`; you verify: {}",
                reviewer.verifies.join(", ")
            ));
        }
        let Some(target) = self.member(&worker) else {
            return err(format!("no member named `{worker}`"));
        };
        if target.progress != Some(Progress::AwaitingVerification) {
            return err(format!(
                "`{worker}` is not waiting for verification (status: {})",
                target.progress.map_or("none", Progress::as_str)
            ));
        }
        let findings = findings.trim();
        let evidence = evidence.trim();
        match verdict {
            Verdict::Fail if findings.is_empty() => {
                return err(
                    "a `fail` verdict needs concrete findings: file, defect, and how to reproduce it",
                );
            }
            Verdict::Pass if evidence.is_empty() => {
                return err(
                    "a `pass` verdict needs evidence: the commands and checks you ran and what they showed",
                );
            }
            _ => {}
        }
        if let Some(reviewer) = self.member_mut(verifier) {
            reviewer.idle_notices = 0;
        }
        let Some(target) = self.member_mut(&worker) else {
            return err(format!("no member named `{worker}`"));
        };
        let detail = match verdict {
            Verdict::Pass => evidence.to_string(),
            Verdict::Fail => findings.to_string(),
        };
        target.last_verdict = Some(LastVerdict {
            verifier: verifier.to_string(),
            verdict,
            detail: detail.clone(),
        });
        target.idle_notices = 0;
        match verdict {
            Verdict::Pass => {
                target.progress = Some(Progress::Verified);
                let all_verified = self.all_workers_verified();
                let mut body = format!("@{COORDINATOR} PASS for @{worker}.\n\nEvidence: {detail}");
                if all_verified {
                    body.push_str("\n\nEvery active worker is verified. The coordinator can finish the swarm.");
                }
                Ok(self.post_internal(
                    verifier,
                    MessageKind::Verdict,
                    vec![COORDINATOR.to_string()],
                    body,
                ))
            }
            Verdict::Fail => {
                target.fix_rounds += 1;
                let rounds = target.fix_rounds;
                if rounds >= MAX_FIX_ROUNDS {
                    target.progress = Some(Progress::Blocked);
                    Ok(self.post_internal(
                        verifier,
                        MessageKind::Verdict,
                        vec![COORDINATOR.to_string()],
                        format!(
                            "@{COORDINATOR} FAIL for @{worker} ({rounds} of {MAX_FIX_ROUNDS} rounds). The assignment is blocked until you reopen, reassign, or retire it.\n\nFindings: {detail}"
                        ),
                    ))
                } else {
                    target.progress = Some(Progress::InProgress);
                    Ok(self.post_internal(
                        verifier,
                        MessageKind::Defect,
                        vec![worker.clone()],
                        format!(
                            "@{worker} FAIL (round {rounds} of {MAX_FIX_ROUNDS}). Fix these findings, rerun your checks, and call `swarm_report_done` again.\n\nFindings: {detail}"
                        ),
                    ))
                }
            }
        }
    }

    /// The coordinator reopens a worker's assignment (after it was blocked, or to ask for more).
    pub fn reopen(&mut self, worker: &str, instructions: &str) -> Result<Posted, SwarmError> {
        self.require_running()?;
        let worker = worker.trim().trim_start_matches('@').to_ascii_lowercase();
        let Some(member) = self.member_mut(&worker) else {
            return err(format!("no member named `{worker}`"));
        };
        if member.role != Role::Worker || member.activity == Activity::Retired {
            return err(format!("`{worker}` is not an active worker"));
        }
        if instructions.trim().is_empty() {
            return err("tell the worker what to change");
        }
        member.progress = Some(Progress::InProgress);
        member.fix_rounds = 0;
        member.idle_notices = 0;
        Ok(self.post_internal(
            COORDINATOR,
            MessageKind::Handoff,
            vec![worker.clone()],
            format!(
                "@{worker} your assignment is reopened.\n\n{}",
                instructions.trim()
            ),
        ))
    }

    /// Records files an agent changed. Returns a post to deliver when an edit broke ownership.
    pub fn record_edit(&mut self, agent: &str, paths: &[String]) -> Option<Posted> {
        if self.phase != Phase::Running {
            return None;
        }
        let mut new_violations = Vec::new();
        if agent == COORDINATOR {
            for path in paths {
                if let Some(owner) = self.active_owner_of(path) {
                    new_violations.push(Violation {
                        agent: agent.to_string(),
                        path: path.clone(),
                        reason: format!(
                            "owned by worker `{owner}`, whose work is not verified yet"
                        ),
                    });
                }
            }
        } else {
            let member = self.member_mut(agent)?;
            member.edited = true;
            let role = member.role;
            let files = member.files.clone();
            for path in paths {
                let reason = match role {
                    Role::Verifier => Some("verifiers must not edit files".to_string()),
                    Role::Worker if !files.iter().any(|owned| path_within(path, owned)) => {
                        Some(format!("outside `{agent}`'s files ({})", files.join(", ")))
                    }
                    Role::Worker => None,
                };
                if let Some(reason) = reason {
                    new_violations.push(Violation {
                        agent: agent.to_string(),
                        path: path.clone(),
                        reason,
                    });
                }
            }
        }
        if new_violations.is_empty() {
            self.bump();
            return None;
        }
        let lines = new_violations
            .iter()
            .map(|violation| {
                format!(
                    "- `{}` edited `{}`: {}",
                    violation.agent, violation.path, violation.reason
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.violations.extend(new_violations);
        let mut recipients = vec![COORDINATOR.to_string()];
        recipients.retain(|recipient| recipient != agent);
        Some(self.post_internal(
            HARNESS,
            MessageKind::System,
            recipients,
            format!(
                "@{COORDINATOR} file ownership violation:\n{lines}\nReview the change, coordinate with the owner, and acknowledge it in `swarm_finish`."
            ),
        ))
    }

    fn active_owner_of(&self, path: &str) -> Option<String> {
        self.active_workers()
            .filter(|worker| worker.progress != Some(Progress::Verified))
            .find(|worker| worker.files.iter().any(|owned| path_within(path, owned)))
            .map(|worker| worker.name.clone())
    }

    fn require_running(&self) -> Result<(), SwarmError> {
        match self.phase {
            Phase::Running => Ok(()),
            Phase::Forming => err("the swarm has no roster yet"),
            Phase::Finished => err("the swarm already finished"),
        }
    }

    /// The coordinator finishes the run.
    pub fn finish(
        &mut self,
        summary: &str,
        acknowledge_violations: Option<&str>,
    ) -> Result<(), SwarmError> {
        self.require_running()?;
        let unverified = self
            .active_workers()
            .filter(|worker| worker.progress != Some(Progress::Verified))
            .map(|worker| {
                format!(
                    "{} ({})",
                    worker.name,
                    worker.progress.map_or("none", Progress::as_str)
                )
            })
            .collect::<Vec<_>>();
        if self.active_workers().next().is_none() {
            return err(
                "every worker is retired; add a worker or stop the swarm with `/swarm stop`",
            );
        }
        if !unverified.is_empty() {
            return err(format!(
                "the swarm cannot finish until a verifier passes every active worker; not verified: {}",
                unverified.join(", ")
            ));
        }
        if !self.violations.is_empty()
            && acknowledge_violations.is_none_or(|ack| ack.trim().is_empty())
        {
            return err(format!(
                "{} file ownership violation(s) are on record; explain how each was resolved in `acknowledge_violations`",
                self.violations.len()
            ));
        }
        if summary.trim().is_empty() {
            return err("the final summary is empty");
        }
        self.final_summary = Some(summary.trim().to_string());
        self.post_internal(
            COORDINATOR,
            MessageKind::System,
            Vec::new(),
            format!("Swarm finished.\n\n{}", summary.trim()),
        );
        self.phase = Phase::Finished;
        for member in &mut self.members {
            if member.activity != Activity::Retired {
                member.activity = Activity::Parked;
            }
        }
        Ok(())
    }

    // ----- Idle handling ------------------------------------------------------------------

    /// Decides what happens when a member's turn ends.
    pub fn on_member_idle(&mut self, name: &str) -> MemberIdle {
        if self.phase != Phase::Running {
            return MemberIdle::Park;
        }
        let Some(member) = self.member(name) else {
            return MemberIdle::Park;
        };
        if member.activity == Activity::Retired {
            return MemberIdle::Park;
        }
        if !self.pending_for(name).is_empty() {
            return MemberIdle::Wake;
        }
        let owed = match member.role {
            Role::Worker => (member.progress == Some(Progress::InProgress))
                .then(|| "parked without calling `swarm_report_done`".to_string()),
            Role::Verifier => {
                let waiting = member
                    .verifies
                    .iter()
                    .filter(|worker| {
                        self.member(worker).is_some_and(|worker| {
                            worker.progress == Some(Progress::AwaitingVerification)
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                (!waiting.is_empty()).then(|| {
                    format!(
                        "parked without a verdict for {}",
                        waiting
                            .iter()
                            .map(|worker| format!("@{worker}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            }
        };
        let notices = member.idle_notices;
        if let Some(member) = self.member_mut(name) {
            member.activity = Activity::Parked;
        }
        let Some(owed) = owed else {
            return MemberIdle::Park;
        };
        if notices >= MAX_IDLE_NOTICES {
            return MemberIdle::Park;
        }
        if let Some(member) = self.member_mut(name) {
            member.idle_notices += 1;
        }
        let mut body = format!("@{COORDINATOR} @{name} {owed}.");
        if notices + 1 == MAX_IDLE_NOTICES {
            body.push_str(" This is the last automatic notice for this member.");
        }
        let posted = self.post_internal(
            HARNESS,
            MessageKind::System,
            vec![COORDINATOR.to_string()],
            body,
        );
        MemberIdle::Notified(posted.recipients)
    }

    /// Decides what happens when the coordinator's turn ends.
    pub fn on_coordinator_idle(&mut self) -> CoordinatorIdle {
        if self.phase == Phase::Finished {
            return CoordinatorIdle::Done;
        }
        if !self.pending_for(COORDINATOR).is_empty() {
            return CoordinatorIdle::Wake;
        }
        let nudge = match self.phase {
            // Before the roster exists the coordinator may be asking the user a question, so the
            // harness waits for the user instead of pushing.
            Phase::Forming => None,
            Phase::Running if self.all_workers_verified() => Some(
                "Every active worker is verified. Run the final checks and call `swarm_finish`."
                    .to_string(),
            ),
            Phase::Running => {
                let invited = self.invited_count();
                let unbound = self.spawned_threads as usize
                    - self
                        .members
                        .iter()
                        .filter(|member| member.thread_id.is_some())
                        .count()
                        .min(self.spawned_threads as usize);
                let any_active = self
                    .members
                    .iter()
                    .any(|member| member.activity == Activity::Active);
                if invited > unbound {
                    let names = self
                        .members
                        .iter()
                        .filter(|member| member.activity == Activity::Invited)
                        .map(|member| member.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Some(format!(
                        "These members have not joined: {names}. Spawn each one with `spawn_agent`, passing its spawn_message from `swarm_status` unchanged."
                    ))
                } else if any_active || invited > 0 {
                    None
                } else {
                    Some(
                        "Every member is parked and no message is waiting, but the swarm is not finished. Read `swarm_status`, then unblock the work: message the right member, reopen a blocked worker, add a member, or retire one."
                            .to_string(),
                    )
                }
            }
            Phase::Finished => None,
        };
        let Some(nudge) = nudge else {
            return CoordinatorIdle::Park;
        };
        if self.stalled {
            return CoordinatorIdle::Stalled;
        }
        if self.revision == self.last_nudge_revision {
            self.nudges_without_progress += 1;
        } else {
            self.nudges_without_progress = 1;
        }
        self.last_nudge_revision = self.revision;
        if self.nudges_without_progress > MAX_COORDINATOR_NUDGES {
            self.stalled = true;
            return CoordinatorIdle::Stalled;
        }
        CoordinatorIdle::Nudge(nudge)
    }

    /// The coordinator needs a user decision: stop resuming it until the user replies.
    pub fn wait_for_user(&mut self, question: &str) -> Result<(), SwarmError> {
        if self.phase == Phase::Finished {
            return err("the swarm already finished");
        }
        if question.trim().is_empty() {
            return err("state the decision you need from the user");
        }
        self.stalled = true;
        self.bump();
        Ok(())
    }

    /// A user message reaches the coordinator; restart the no-progress breaker.
    pub fn on_user_input(&mut self) {
        self.stalled = false;
        self.nudges_without_progress = 0;
    }
}

/// Assigns uncovered active workers to the active verifier with the fewest assignments, then
/// checks that every verifier covers only active workers and at most the allowed number.
fn assign_coverage(members: &mut [Member]) -> Result<(), SwarmError> {
    let workers = members
        .iter()
        .filter(|member| member.role == Role::Worker && member.is_active_member())
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    for verifier in members
        .iter_mut()
        .filter(|member| member.role == Role::Verifier && member.is_active_member())
    {
        if let Some(unknown) = verifier
            .verifies
            .iter()
            .find(|name| !workers.contains(name))
        {
            return err(format!(
                "verifier `{}` lists `{unknown}`, which is not an active worker",
                verifier.name
            ));
        }
        verifier.verifies.dedup();
    }
    for worker in &workers {
        let covered = members.iter().any(|member| {
            member.role == Role::Verifier
                && member.is_active_member()
                && member.verifies.contains(worker)
        });
        if covered {
            continue;
        }
        let Some(verifier) = members
            .iter_mut()
            .filter(|member| {
                member.role == Role::Verifier
                    && member.is_active_member()
                    && member.verifies.len() < MAX_WORKERS_PER_VERIFIER
            })
            .min_by_key(|member| member.verifies.len())
        else {
            return err(format!(
                "no verifier has room for worker `{worker}`; add a verifier (one for every 2-{MAX_WORKERS_PER_VERIFIER} workers)"
            ));
        };
        verifier.verifies.push(worker.clone());
    }
    if let Some(verifier) = members
        .iter()
        .find(|member| member.verifies.len() > MAX_WORKERS_PER_VERIFIER)
    {
        return err(format!(
            "verifier `{}` covers {} workers; the limit is {MAX_WORKERS_PER_VERIFIER}",
            verifier.name,
            verifier.verifies.len()
        ));
    }
    Ok(())
}

fn check_file_ownership(members: &[Member]) -> Result<(), SwarmError> {
    let workers = members
        .iter()
        .filter(|member| member.role == Role::Worker && member.is_active_member())
        .collect::<Vec<_>>();
    for (index, left) in workers.iter().enumerate() {
        for right in &workers[index + 1..] {
            for a in &left.files {
                for b in &right.files {
                    if paths_overlap(a, b) {
                        return err(format!(
                            "workers must own non-overlapping files: `{}` ({a}) overlaps `{}` ({b})",
                            left.name, right.name
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Lowercases and validates a member or group name.
pub fn normalize_name(raw: &str) -> Result<String, SwarmError> {
    let name = raw.trim().trim_start_matches('@').to_ascii_lowercase();
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return err(format!(
            "`{raw}` is not a valid name: use 1-{MAX_NAME_LEN} lowercase letters, digits, `-` or `_`, starting with a letter"
        ));
    }
    if RESERVED_NAMES.contains(&name.as_str()) {
        return err(format!("`{name}` is reserved"));
    }
    Ok(name)
}

/// Mentions in `body`: `@` at a word boundary followed by a name.
pub fn extract_mentions(body: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let chars = body.char_indices().collect::<Vec<_>>();
    for (index, (offset, c)) in chars.iter().enumerate() {
        if *c != '@' {
            continue;
        }
        let at_boundary = index == 0 || {
            let previous = chars[index - 1].1;
            !(previous.is_alphanumeric() || previous == '_' || previous == '`' || previous == '.')
        };
        if !at_boundary {
            continue;
        }
        let name = body[offset + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
            .trim_end_matches(['-', '_'])
            .to_ascii_lowercase();
        if name.starts_with(|c: char| c.is_ascii_lowercase()) && !mentions.contains(&name) {
            mentions.push(name);
        }
    }
    mentions
}

/// Normalizes a repository path: trims, drops a leading `./`, and uses `/` separators.
pub fn normalize_path(raw: &str) -> String {
    let path = raw.trim().replace('\\', "/");
    let mut path = path.as_str();
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path.to_string()
}

fn as_dir(path: &str) -> String {
    format!("{}/", path.trim_end_matches('/'))
}

/// Whether two ownership entries can refer to the same file.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let a_trim = a.trim_end_matches('/');
    let b_trim = b.trim_end_matches('/');
    a_trim == b_trim || b.starts_with(&as_dir(a)) || a.starts_with(&as_dir(b))
}

/// Whether an edited `path` is within the ownership entry `owned`. Absolute paths match when
/// they end with the owned path at a directory boundary.
pub fn path_within(path: &str, owned: &str) -> bool {
    let path = normalize_path(path);
    let owned_trim = owned.trim_end_matches('/');
    let owned_dir = as_dir(owned);
    if path == owned_trim || path.starts_with(&owned_dir) {
        return true;
    }
    if path.starts_with('/') || path.contains(":/") {
        let suffix = format!("/{owned_trim}");
        return path.ends_with(&suffix) || path.contains(&format!("/{owned_dir}"));
    }
    false
}

/// Paths that an `apply_patch` input touches.
pub fn patch_paths(patch: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut paths = Vec::new();
    for line in patch.lines() {
        let line = line.trim_start();
        if let Some(path) = MARKERS.iter().find_map(|marker| line.strip_prefix(marker)) {
            let path = normalize_path(path);
            if !path.is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
