use super::*;
use crate::state::Phase;
use pretty_assertions::assert_eq;

#[test]
fn descendants_follow_the_parent_chain() {
    let service = TeamworkService::new();
    service.register_thread("child", Some("lead"));
    service.register_thread("grandchild", Some("child"));
    service.register_thread("other", Some("someone-else"));

    assert!(service.is_descendant("child", "lead"));
    assert!(service.is_descendant("grandchild", "lead"));
    assert!(!service.is_descendant("other", "lead"));
    assert!(!service.is_descendant("lead", "lead"));
}

#[test]
fn open_tickets_are_visible_only_to_descendants() {
    let service = TeamworkService::new();
    service.register_thread("gate", Some("lead"));
    service.register_thread("stranger", Some("elsewhere"));
    let runtime = service.runtime_or_insert("lead", None);
    runtime.with_run(|run| {
        let mut new_run = TeamworkRun::new("run", "lead", "goal");
        new_run.gates.push(crate::state::GateTicket {
            token: "tok".to_string(),
            milestone_id: Some("m1".to_string()),
            kind: crate::state::GateKind::Critic,
            round: 1,
            status: TicketStatus::Open,
            reviewer_thread_id: None,
            findings: String::new(),
            evidence: String::new(),
        });
        *run = Some(new_run);
    });

    assert!(service.has_open_tickets_for("gate"));
    assert!(!service.has_open_tickets_for("stranger"));
    assert!(service.runtime_for_ticket("gate", "tok").is_some());
    assert!(service.runtime_for_ticket("stranger", "tok").is_none());
}

#[tokio::test]
async fn state_round_trips_through_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("teamwork").join("lead.json");

    let service = TeamworkService::new();
    let runtime = service.runtime_or_insert("lead", Some(path.clone()));
    runtime.with_run(|run| *run = Some(TeamworkRun::new("run", "lead", "goal")));
    runtime.save().await;

    let restored_service = TeamworkService::new();
    let restored = restored_service.runtime_or_insert("lead", Some(path.clone()));
    restored.load().await;
    assert_eq!(restored.snapshot(), runtime.snapshot());

    runtime.with_run(|run| {
        if let Some(run) = run {
            run.phase = Phase::Complete;
        }
    });
    runtime.save().await;
    let completed = TeamworkService::new().runtime_or_insert("lead", Some(path));
    completed.load().await;
    assert_eq!(completed.snapshot(), None);
}
