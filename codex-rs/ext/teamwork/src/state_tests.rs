use super::*;
use pretty_assertions::assert_eq;

fn brief() -> Brief {
    Brief {
        objective: "Port the parser".to_string(),
        requirements: vec!["keep the public API".to_string()],
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "ac1".to_string(),
            description: "all parser tests pass".to_string(),
            verification: "cargo test -p parser".to_string(),
        }],
        out_of_scope: Vec::new(),
        assumptions: Vec::new(),
    }
}

fn track(id: &str, files: &[&str]) -> Track {
    Track {
        id: id.to_string(),
        description: format!("implement {id}"),
        files: files.iter().map(ToString::to_string).collect(),
        role: "worker".to_string(),
        model: None,
        reasoning_effort: Some("medium".to_string()),
    }
}

fn milestone(id: &str, tracks: Vec<Track>) -> MilestonePlan {
    MilestonePlan {
        id: id.to_string(),
        title: format!("milestone {id}"),
        deliverable: format!("deliverable {id}"),
        tracks,
    }
}

fn planned_run(milestones: Vec<MilestonePlan>) -> TeamworkRun {
    let mut run = TeamworkRun::new("run12345678", "lead", "port the parser");
    run.record_brief(brief()).expect("brief");
    run.record_plan(milestones).expect("plan");
    run
}

fn pass_all(run: &mut TeamworkRun, tickets: &[GateTicket]) -> VerdictOutcome {
    let mut outcome = VerdictOutcome::AwaitingOtherGates;
    for (index, ticket) in tickets.iter().enumerate() {
        outcome = run
            .record_verdict(
                &ticket.token,
                &format!("reviewer-{}-{index}", ticket.token),
                false,
                Verdict::Pass,
                String::new(),
                "ran cargo test".to_string(),
            )
            .expect("verdict");
    }
    outcome
}

#[test]
fn brief_requires_verifiable_acceptance_criteria() {
    let mut run = TeamworkRun::new("run", "lead", "goal");
    let mut missing_verification = brief();
    missing_verification.acceptance_criteria[0].verification = " ".to_string();
    assert!(run.record_brief(missing_verification).is_err());
    assert_eq!(run.phase, Phase::Scoping);

    run.record_brief(brief()).expect("brief");
    assert_eq!(run.phase, Phase::Planning);
}

#[test]
fn plan_cannot_be_recorded_before_brief() {
    let mut run = TeamworkRun::new("run", "lead", "goal");
    let error = run
        .record_plan(vec![milestone("m1", vec![track("a", &["src/a.rs"])])])
        .expect_err("plan before brief");
    assert!(error.0.contains("scoping"));
}

#[test]
fn plan_rejects_overlapping_tracks() {
    let mut run = TeamworkRun::new("run", "lead", "goal");
    run.record_brief(brief()).expect("brief");
    let error = run
        .record_plan(vec![milestone(
            "m1",
            vec![track("a", &["src/"]), track("b", &["./src/lib.rs"])],
        )])
        .expect_err("overlap");
    assert!(error.0.contains("non-overlapping"));
    assert_eq!(run.phase, Phase::Planning);
}

#[test]
fn paths_overlap_respects_component_boundaries() {
    assert!(paths_overlap("src", "src/lib.rs"));
    assert!(paths_overlap("src/lib.rs", "./src/lib.rs"));
    assert!(!paths_overlap("src/lib.rs", "src/lib.rs.bak"));
    assert!(!paths_overlap("src/a", "src/ab"));
}

#[test]
fn milestone_advances_only_after_every_gate_passes() {
    let mut run = planned_run(vec![
        milestone("m1", vec![track("a", &["src/a.rs"])]),
        milestone("m2", vec![track("b", &["src/b.rs"])]),
    ]);
    let tickets = run
        .submit_for_review("m1", "implemented a".to_string())
        .expect("submit");
    assert_eq!(tickets.len(), 3);
    assert_eq!(run.phase, Phase::Gating);

    let outcome = run
        .record_verdict(
            &tickets[0].token,
            "critic",
            false,
            Verdict::Pass,
            String::new(),
            "looked at the diff".to_string(),
        )
        .expect("critic");
    assert_eq!(outcome, VerdictOutcome::AwaitingOtherGates);
    assert_eq!(run.phase, Phase::Gating);

    let outcome = pass_all(&mut run, &tickets[1..]);
    assert_eq!(
        outcome,
        VerdictOutcome::MilestonePassed {
            next_milestone_id: "m2".to_string()
        }
    );
    assert_eq!(run.phase, Phase::Executing);
    assert_eq!(run.current().map(|m| m.id.as_str()), Some("m2"));
}

#[test]
fn lead_and_implementers_cannot_submit_verdicts() {
    let mut run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    let tickets = run
        .submit_for_review("m1", "done".to_string())
        .expect("submit");
    let token = &tickets[0].token;
    assert!(
        run.record_verdict(
            token,
            "lead",
            false,
            Verdict::Pass,
            String::new(),
            "x".into()
        )
        .is_err()
    );
    assert!(
        run.record_verdict(
            token,
            "worker",
            true,
            Verdict::Pass,
            String::new(),
            "x".into()
        )
        .is_err()
    );
    run.record_verdict(
        token,
        "critic",
        false,
        Verdict::Pass,
        String::new(),
        "x".into(),
    )
    .expect("independent reviewer");
    let error = run
        .record_verdict(
            &tickets[1].token,
            "critic",
            false,
            Verdict::Pass,
            String::new(),
            "x".into(),
        )
        .expect_err("same reviewer for two gates");
    assert!(error.0.contains("own agent"));
}

#[test]
fn failed_gate_returns_to_execution_and_blocks_after_budget() {
    let mut run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    for round in 1..=MAX_FIX_ROUNDS + 1 {
        let tickets = run
            .submit_for_review("m1", format!("attempt {round}"))
            .expect("submit");
        pass_all(&mut run, &tickets[1..]);
        let outcome = run
            .record_verdict(
                &tickets[0].token,
                &format!("critic-{round}"),
                false,
                Verdict::Fail,
                "off-by-one in parser".to_string(),
                String::new(),
            )
            .expect("fail verdict");
        if round <= MAX_FIX_ROUNDS {
            assert_eq!(
                outcome,
                VerdictOutcome::MilestoneNeedsFixes { fix_round: round }
            );
            assert_eq!(run.phase, Phase::Executing);
        } else {
            assert!(matches!(outcome, VerdictOutcome::Blocked { .. }));
            assert_eq!(run.phase, Phase::Blocked);
        }
    }

    assert!(run.resume_after_user_input());
    assert_eq!(run.phase, Phase::Executing);
    assert_eq!(run.current().map(|m| m.fix_rounds), Some(0));
}

#[test]
fn final_audit_gates_completion() {
    let mut run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    let tickets = run
        .submit_for_review("m1", "done".to_string())
        .expect("submit");
    let VerdictOutcome::FinalAuditOpened { ticket } = pass_all(&mut run, &tickets) else {
        panic!("expected the final audit to open");
    };
    assert_eq!(run.phase, Phase::FinalAudit);
    assert_eq!(ticket.kind, GateKind::SuccessAuditor);

    let outcome = run
        .record_verdict(
            &ticket.token,
            "auditor",
            false,
            Verdict::Fail,
            "ac1 not verified".to_string(),
            String::new(),
        )
        .expect("final fail");
    assert_eq!(outcome, VerdictOutcome::FinalAuditFailed { fix_round: 1 });
    assert_eq!(run.phase, Phase::FinalFix);
    assert!(run.submit_for_review("m1", "fixed".to_string()).is_err());

    let tickets = run
        .submit_for_review(FINAL_MILESTONE_ID, "fixed".to_string())
        .expect("resubmit");
    let outcome = pass_all(&mut run, &tickets);
    assert_eq!(outcome, VerdictOutcome::Complete);
    assert_eq!(run.phase, Phase::Complete);
}

#[test]
fn idle_without_progress_blocks_the_run() {
    let mut run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    for _ in 0..MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS {
        assert_eq!(run.on_idle(), ContinuationDecision::Continue);
    }
    assert!(matches!(
        run.on_idle(),
        ContinuationDecision::BlockedNoProgress { .. }
    ));
    assert_eq!(run.phase, Phase::Blocked);
    assert_eq!(run.on_idle(), ContinuationDecision::Skip);
}

#[test]
fn progress_resets_the_idle_counter() {
    let mut run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    assert_eq!(run.on_idle(), ContinuationDecision::Continue);
    assert_eq!(run.on_idle(), ContinuationDecision::Continue);
    run.submit_for_review("m1", "done".to_string())
        .expect("submit");
    for _ in 0..MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS {
        assert_eq!(run.on_idle(), ContinuationDecision::Continue);
    }
}

#[test]
fn scoping_does_not_auto_continue() {
    let mut run = TeamworkRun::new("run", "lead", "goal");
    assert_eq!(run.on_idle(), ContinuationDecision::Skip);
}

#[test]
fn verdicts_while_blocked_keep_the_run_blocked() {
    let mut run = planned_run(vec![
        milestone("m1", vec![track("a", &["src/a.rs"])]),
        milestone("m2", vec![track("b", &["src/b.rs"])]),
    ]);
    let tickets = run
        .submit_for_review("m1", "done".to_string())
        .expect("submit");
    run.block("need a decision on the API name".to_string())
        .expect("block");
    pass_all(&mut run, &tickets);
    assert_eq!(run.phase, Phase::Blocked);
    assert!(run.resume_after_user_input());
    assert_eq!(run.phase, Phase::Executing);
    assert_eq!(run.current().map(|m| m.id.as_str()), Some("m2"));
}

#[test]
fn run_round_trips_through_json() {
    let run = planned_run(vec![milestone("m1", vec![track("a", &["src/a.rs"])])]);
    let json = serde_json::to_string(&run).expect("serialize");
    let restored: TeamworkRun = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, run);
}
