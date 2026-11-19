use super::*;
use pretty_assertions::assert_eq;

fn worker(name: &str, files: &[&str]) -> MemberSpec {
    MemberSpec {
        name: name.to_string(),
        role: Role::Worker,
        assignment: format!("build {name}"),
        files: files.iter().map(ToString::to_string).collect(),
        groups: Vec::new(),
        verifies: Vec::new(),
        model: None,
        reasoning_effort: None,
    }
}

fn verifier(name: &str, verifies: &[&str]) -> MemberSpec {
    MemberSpec {
        name: name.to_string(),
        role: Role::Verifier,
        assignment: format!("verify as {name}"),
        files: Vec::new(),
        groups: Vec::new(),
        verifies: verifies.iter().map(ToString::to_string).collect(),
        model: None,
        reasoning_effort: None,
    }
}

fn new_run() -> SwarmRun {
    SwarmRun::new("r1".into(), "coord".into(), "ship login".into())
}

/// A running swarm with workers `api` and `ui` (group `frontend`) and verifier `check`, all
/// joined from threads `t-<name>`.
fn running() -> SwarmRun {
    let mut run = new_run();
    let mut ui = worker("ui", &["web/"]);
    ui.groups = vec!["frontend".into()];
    run.define_roster(vec![
        worker("api", &["server/"]),
        ui,
        verifier("check", &[]),
    ])
    .expect("roster");
    for name in ["api", "ui", "check"] {
        let token = run.member(name).expect("member").join_token.clone();
        run.join(&token, &format!("t-{name}")).expect("join");
    }
    run
}

#[test]
fn roster_requires_workers_and_verifiers() {
    let mut run = new_run();
    let error = run
        .define_roster(vec![worker("api", &["server/"])])
        .expect_err("no verifier");
    assert!(error.0.contains("at least one verifier"), "{error}");
    let error = run
        .define_roster(vec![verifier("check", &[])])
        .expect_err("no worker");
    assert!(error.0.contains("at least one worker"), "{error}");
    assert_eq!(run.phase, Phase::Forming);
}

#[test]
fn roster_rejects_overlapping_files_and_bad_names() {
    let mut run = new_run();
    let error = run
        .define_roster(vec![
            worker("api", &["src/"]),
            worker("ui", &["./src/ui.rs"]),
            verifier("check", &[]),
        ])
        .expect_err("overlap");
    assert!(error.0.contains("non-overlapping"), "{error}");

    let error = run
        .define_roster(vec![worker("all", &["a"]), verifier("check", &[])])
        .expect_err("reserved");
    assert!(error.0.contains("reserved"), "{error}");

    let error = run
        .define_roster(vec![worker("Bad Name", &["a"]), verifier("check", &[])])
        .expect_err("invalid");
    assert!(error.0.contains("not a valid name"), "{error}");

    let mut owning = verifier("check", &[]);
    owning.files = vec!["a".into()];
    let error = run
        .define_roster(vec![worker("api", &["b"]), owning])
        .expect_err("verifier files");
    assert!(error.0.contains("cannot own files"), "{error}");
}

#[test]
fn verifiers_cover_at_most_four_workers() {
    let mut run = new_run();
    let workers = (0..5)
        .map(|index| worker(&format!("w{index}"), &[&format!("f{index}")]))
        .collect::<Vec<_>>();
    let mut specs = workers.clone();
    specs.push(verifier("check", &[]));
    let error = run.define_roster(specs).expect_err("over capacity");
    assert!(error.0.contains("add a verifier"), "{error}");

    let mut specs = workers;
    specs.push(verifier("check-a", &[]));
    specs.push(verifier("check-b", &[]));
    run.define_roster(specs).expect("two verifiers");
    let a = run.member("check-a").expect("a").verifies.len();
    let b = run.member("check-b").expect("b").verifies.len();
    assert_eq!(a + b, 5);
    assert!(a <= MAX_WORKERS_PER_VERIFIER && b <= MAX_WORKERS_PER_VERIFIER);
    assert_eq!(run.phase, Phase::Running);
}

#[test]
fn explicit_coverage_must_name_workers() {
    let mut run = new_run();
    let error = run
        .define_roster(vec![worker("api", &["a"]), verifier("check", &["nobody"])])
        .expect_err("unknown");
    assert!(error.0.contains("not an active worker"), "{error}");
}

#[test]
fn join_binds_threads_once() {
    let mut run = new_run();
    run.define_roster(vec![worker("api", &["a"]), verifier("check", &[])])
        .expect("roster");
    let token = run.member("api").expect("api").join_token.clone();
    assert!(run.join("nope", "t1").is_err());
    let member = run.join(&token, "t1").expect("join");
    assert_eq!(member.name, "api");
    assert_eq!(run.agent_name("t1"), Some("api".to_string()));
    assert_eq!(run.agent_name("coord"), Some(COORDINATOR.to_string()));
    assert!(run.join(&token, "t2").is_err(), "token reuse");
}

#[test]
fn mentions_resolve_to_members_groups_and_all() {
    let mut run = running();
    let posted = run
        .post(
            "api",
            &[],
            MessageKind::Question,
            "@ui @frontend which route? cc @coordinator",
        )
        .expect("post");
    assert_eq!(
        posted.recipients,
        vec!["ui".to_string(), COORDINATOR.to_string()]
    );

    let posted = run
        .post("coordinator", &[], MessageKind::Note, "@all standup")
        .expect("post");
    assert_eq!(
        posted.recipients,
        vec!["api".to_string(), "ui".to_string(), "check".to_string()]
    );

    let posted = run
        .post("check", &["@workers".into()], MessageKind::Note, "heads up")
        .expect("post");
    assert_eq!(posted.recipients, vec!["api".to_string(), "ui".to_string()]);

    let error = run
        .post("api", &[], MessageKind::Note, "@nobody hi")
        .expect_err("unknown mention");
    assert!(error.0.contains("valid mentions"), "{error}");

    // Email addresses and code are not mentions.
    let posted = run
        .post(
            "api",
            &[],
            MessageKind::Note,
            "mail me@example.com about `@ui`",
        )
        .expect("post");
    assert!(posted.recipients.is_empty());

    assert!(run.post("api", &[], MessageKind::Verdict, "pass").is_err());
}

#[test]
fn pending_messages_track_delivery() {
    let mut run = running();
    run.post("api", &[], MessageKind::Question, "@ui one")
        .expect("post");
    let second = run
        .post("check", &[], MessageKind::Note, "@ui two")
        .expect("post");
    assert_eq!(run.pending_for("ui").len(), 2);
    run.mark_delivered("ui", second.seq);
    assert!(run.pending_for("ui").is_empty());
    assert!(
        run.pending_for("api").is_empty(),
        "senders do not receive their own posts"
    );
}

#[test]
fn verification_flow_reaches_finish() {
    let mut run = running();
    assert!(run.finish("done", None).is_err(), "nothing verified");

    let posted = run
        .report_done("api", "server done; cargo test ok")
        .expect("report");
    assert_eq!(posted.recipients, vec!["check".to_string()]);
    assert_eq!(
        run.member("api").and_then(|m| m.progress),
        Some(Progress::AwaitingVerification)
    );
    assert!(run.report_done("api", "again").is_err());

    // Workers cannot verify, and verifiers need evidence or findings.
    assert!(run.verify("ui", "api", Verdict::Pass, "", "ok").is_err());
    assert!(run.verify("check", "api", Verdict::Pass, "", "").is_err());
    assert!(run.verify("check", "api", Verdict::Fail, "", "").is_err());

    let posted = run
        .verify(
            "check",
            "api",
            Verdict::Fail,
            "server/a.rs:3 panics on empty input",
            "",
        )
        .expect("fail");
    assert_eq!(posted.recipients, vec!["api".to_string()]);
    assert_eq!(
        run.member("api").and_then(|m| m.progress),
        Some(Progress::InProgress)
    );

    run.report_done("api", "fixed").expect("report");
    let posted = run
        .verify("check", "@api", Verdict::Pass, "", "cargo test: 12 passed")
        .expect("pass");
    assert_eq!(posted.recipients, vec![COORDINATOR.to_string()]);

    run.report_done("ui", "ui done").expect("report");
    let posted = run
        .verify("check", "ui", Verdict::Pass, "", "npm test ok")
        .expect("pass");
    assert!(
        run.board
            .last()
            .expect("msg")
            .body
            .contains("Every active worker is verified")
    );
    assert_eq!(posted.recipients, vec![COORDINATOR.to_string()]);
    assert!(run.all_workers_verified());

    run.finish("shipped login", None).expect("finish");
    assert_eq!(run.phase, Phase::Finished);
    assert!(run.post("api", &[], MessageKind::Note, "late").is_err());
}

#[test]
fn repeated_failures_block_until_reopened() {
    let mut run = running();
    for _ in 0..MAX_FIX_ROUNDS {
        run.report_done("api", "try").expect("report");
        run.verify("check", "api", Verdict::Fail, "still broken", "")
            .expect("fail");
    }
    assert_eq!(
        run.member("api").and_then(|m| m.progress),
        Some(Progress::Blocked)
    );
    assert!(run.report_done("api", "try").is_err());
    let blocked_post = run.board.last().expect("msg");
    assert_eq!(blocked_post.recipients, vec![COORDINATOR.to_string()]);

    let posted = run.reopen("api", "use the other parser").expect("reopen");
    assert_eq!(posted.recipients, vec!["api".to_string()]);
    assert_eq!(run.member("api").map(|m| m.fix_rounds), Some(0));
    run.report_done("api", "try again")
        .expect("report after reopen");
}

#[test]
fn edits_are_checked_against_ownership() {
    let mut run = running();
    assert!(run.record_edit("api", &["server/main.rs".into()]).is_none());
    let posted = run
        .record_edit("api", &["web/index.ts".into()])
        .expect("violation");
    assert_eq!(posted.recipients, vec![COORDINATOR.to_string()]);
    assert_eq!(run.violations.len(), 1);

    // A verifier that edits is disqualified.
    run.record_edit("check", &["server/main.rs".into()])
        .expect("verifier violation");
    run.report_done("api", "done").expect("report");
    let error = run
        .verify("check", "api", Verdict::Pass, "", "ok")
        .expect_err("disqualified");
    assert!(error.0.contains("independent verifier"), "{error}");

    // The coordinator must not edit files of unverified workers.
    assert!(
        run.record_edit(COORDINATOR, &["web/app.ts".into()])
            .is_some()
    );
    assert!(
        run.record_edit(COORDINATOR, &["README.md".into()])
            .is_none()
    );
}

#[test]
fn finishing_requires_acknowledged_violations() {
    let mut run = running();
    run.record_edit("api", &["README.md".into()]);
    for name in ["api", "ui"] {
        run.report_done(name, "done").expect("report");
        run.verify("check", name, Verdict::Pass, "", "tests ok")
            .expect("pass");
    }
    let error = run.finish("done", None).expect_err("violations");
    assert!(error.0.contains("acknowledge_violations"), "{error}");
    run.finish("done", Some("README change was intended and reviewed"))
        .expect("finish");
}

#[test]
fn retiring_reassigns_coverage() {
    let mut run = new_run();
    run.define_roster(vec![
        worker("api", &["a"]),
        worker("ui", &["b"]),
        verifier("check-a", &["api", "ui"]),
        verifier("check-b", &[]),
    ])
    .expect("roster");
    run.retire("check-a", "too slow").expect("retire");
    assert_eq!(
        run.member("check-b").map(|m| m.verifies.clone()),
        Some(vec!["api".to_string(), "ui".to_string()])
    );
    let error = run.retire("check-b", "gone").expect_err("last verifier");
    assert!(error.0.contains("no verifier has room"), "{error}");

    run.retire("ui", "not needed").expect("retire worker");
    assert_eq!(
        run.member("check-b").map(|m| m.verifies.clone()),
        Some(vec!["api".to_string()])
    );
    assert!(run.post("api", &[], MessageKind::Note, "@ui hi").is_err());
}

#[test]
fn add_members_validates_against_existing_roster() {
    let mut run = running();
    let error = run
        .add_members(vec![worker("docs", &["server/docs/"])])
        .expect_err("overlap");
    assert!(error.0.contains("non-overlapping"), "{error}");
    let added = run
        .add_members(vec![worker("docs", &["docs/"])])
        .expect("add");
    assert_eq!(added[0].activity, Activity::Invited);
    assert!(
        run.member("check")
            .expect("check")
            .verifies
            .contains(&"docs".to_string())
    );
}

#[test]
fn idle_members_wake_for_mentions_and_report_owed_work() {
    let mut run = running();
    run.post("check", &[], MessageKind::Question, "@api which port?")
        .expect("post");
    assert_eq!(run.on_member_idle("api"), MemberIdle::Wake);

    let latest = run.latest_seq();
    run.mark_delivered("api", latest);
    // The worker owes a report, so the coordinator hears about the park.
    match run.on_member_idle("api") {
        MemberIdle::Notified(recipients) => assert_eq!(recipients, vec![COORDINATOR.to_string()]),
        other => panic!("expected a notice, got {other:?}"),
    }
    assert_eq!(
        run.member("api").map(|m| m.activity),
        Some(Activity::Parked)
    );

    // Notices stop after the limit.
    for _ in 1..MAX_IDLE_NOTICES {
        assert!(matches!(run.on_member_idle("api"), MemberIdle::Notified(_)));
    }
    assert_eq!(run.on_member_idle("api"), MemberIdle::Park);

    // A verifier with nothing waiting parks silently.
    assert_eq!(run.on_member_idle("check"), MemberIdle::Park);
    run.report_done("ui", "done").expect("report");
    let latest = run.latest_seq();
    run.mark_delivered("check", latest);
    assert!(matches!(
        run.on_member_idle("check"),
        MemberIdle::Notified(_)
    ));
}

#[test]
fn coordinator_idle_parks_wakes_and_stalls() {
    let mut run = new_run();
    assert_eq!(
        run.on_coordinator_idle(),
        CoordinatorIdle::Park,
        "forming waits for the user"
    );

    let mut run = running();
    assert_eq!(
        run.on_coordinator_idle(),
        CoordinatorIdle::Park,
        "members are active"
    );

    run.post("api", &[], MessageKind::Question, "@coordinator scope?")
        .expect("post");
    assert_eq!(run.on_coordinator_idle(), CoordinatorIdle::Wake);
    let latest = run.latest_seq();
    run.mark_delivered(COORDINATOR, latest);

    // Everyone parked with work outstanding: nudge, then stall without progress.
    for name in ["api", "ui", "check"] {
        if let Some(member) = run.members.iter_mut().find(|m| m.name == name) {
            member.activity = Activity::Parked;
        }
    }
    for _ in 0..MAX_COORDINATOR_NUDGES {
        assert!(matches!(
            run.on_coordinator_idle(),
            CoordinatorIdle::Nudge(_)
        ));
    }
    assert_eq!(run.on_coordinator_idle(), CoordinatorIdle::Stalled);
    run.on_user_input();
    assert!(matches!(
        run.on_coordinator_idle(),
        CoordinatorIdle::Nudge(_)
    ));

    // Asking the user pauses resumption.
    run.wait_for_user("which database?").expect("ask");
    assert_eq!(run.on_coordinator_idle(), CoordinatorIdle::Stalled);
}

#[test]
fn coordinator_is_nudged_to_spawn_and_to_finish() {
    let mut run = new_run();
    run.define_roster(vec![worker("api", &["a"]), verifier("check", &[])])
        .expect("roster");
    match run.on_coordinator_idle() {
        CoordinatorIdle::Nudge(text) => assert!(text.contains("have not joined"), "{text}"),
        other => panic!("expected a nudge, got {other:?}"),
    }
    // Spawned threads that have not joined yet count as on their way.
    run.note_spawned_thread();
    run.note_spawned_thread();
    assert_eq!(run.on_coordinator_idle(), CoordinatorIdle::Park);

    let mut run = running();
    for name in ["api", "ui"] {
        run.report_done(name, "done").expect("report");
        run.verify("check", name, Verdict::Pass, "", "ok")
            .expect("pass");
    }
    let latest = run.latest_seq();
    run.mark_delivered(COORDINATOR, latest);
    match run.on_coordinator_idle() {
        CoordinatorIdle::Nudge(text) => assert!(text.contains("swarm_finish"), "{text}"),
        other => panic!("expected a nudge, got {other:?}"),
    }
}

#[test]
fn path_helpers() {
    assert!(paths_overlap("src/", "src/a.rs"));
    assert!(paths_overlap("src", "src/a.rs"));
    assert!(!paths_overlap("src/a.rs", "src/ab.rs"));
    assert!(path_within("./server/main.rs", "server/"));
    assert!(path_within("/repo/server/main.rs", "server/"));
    assert!(path_within("/repo/README.md", "README.md"));
    assert!(!path_within("/repo/web/main.rs", "server/"));
    assert_eq!(
        patch_paths(
            "*** Begin Patch\n*** Add File: a.rs\n+x\n*** Update File: ./b.rs\n*** Move to: c.rs\n*** Delete File: d.rs\n*** End Patch"
        ),
        vec!["a.rs", "b.rs", "c.rs", "d.rs"]
    );
    assert_eq!(
        extract_mentions("@API, ping @ui-team. not@me @9x"),
        vec!["api", "ui-team"]
    );
}

#[test]
fn run_state_round_trips_through_json() {
    let run = running();
    let json = serde_json::to_string(&run).expect("serialize");
    let restored: SwarmRun = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, run);
}
