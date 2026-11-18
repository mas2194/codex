//! End-to-end checks that the harness, not the prompt, drives a `/teamwork` run.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_teamwork_extension::TeamworkExtensionConfig;
use codex_teamwork_extension::TeamworkService;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn has_tool(request: &ResponsesRequest, name: &str) -> bool {
    request.body_json()["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
        || request.tool_by_name("functions", name).is_some()
}

fn teamwork_extensions(
    state_dir: std::path::PathBuf,
) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    codex_teamwork_extension::install(
        &mut extensions,
        TeamworkService::new(),
        Weak::new(),
        move |_config: &Config| TeamworkExtensionConfig {
            enabled: true,
            state_dir: Some(state_dir.clone()),
        },
    );
    Arc::new(extensions.build())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamwork_command_starts_a_harness_run() -> Result<()> {
    let server = start_mock_server().await;
    let state_dir = tempfile::tempdir()?;
    let brief = json!({
        "objective": "Port the parser",
        "acceptance_criteria": [
            {"id": "ac1", "description": "parser tests pass", "verification": "cargo test -p parser"}
        ]
    });
    let bad_plan = json!({
        "milestones": [{
            "id": "m1",
            "title": "Port",
            "deliverable": "ported parser",
            "tracks": [
                {"id": "a", "description": "lexer", "files": ["src/"]},
                {"id": "b", "description": "parser", "files": ["src/parser.rs"]}
            ]
        }]
    });
    let mock = mount_sse_sequence(
        &server,
        vec![
            // Core captures the first step's tools before turn input is contributed, so the
            // lead tools appear from the second step on.
            sse(vec![
                ev_response_created("r0"),
                ev_function_call("early", "teamwork_status", "{}"),
                ev_completed("r0"),
            ]),
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("brief", "teamwork_record_brief", &brief.to_string()),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_function_call("plan", "teamwork_record_plan", &bad_plan.to_string()),
                ev_function_call(
                    "verdict",
                    "teamwork_submit_verdict",
                    &json!({"gate_token": "x", "verdict": "pass", "evidence": "trust me"})
                        .to_string(),
                ),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Planning needs another try."),
                ev_completed("r3"),
            ]),
        ],
    )
    .await;

    let test = test_codex()
        .with_extensions(teamwork_extensions(state_dir.path().to_path_buf()))
        .build(&server)
        .await?;
    test.submit_turn("/teamwork port the parser to Rust")
        .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);

    // The harness injected the lead instructions for the literal command.
    let user_texts = requests[0].message_input_texts("user").join("\n");
    assert!(user_texts.contains("lead agent of a `/teamwork` run"));
    assert!(user_texts.contains("<user_goal>\nport the parser to Rust\n</user_goal>"));

    // Lead tools are exposed to the root thread from the second step; the verdict tool is not.
    assert!(!has_tool(&requests[0], "teamwork_record_brief"));
    let tool_request = &requests[1];
    assert!(has_tool(tool_request, "teamwork_record_brief"));
    assert!(has_tool(tool_request, "teamwork_submit_for_review"));
    assert!(!has_tool(tool_request, "teamwork_submit_verdict"));

    // The brief is accepted and the harness moves the run to planning.
    let brief_output = mock
        .function_call_output_text("brief")
        .context("brief output")?;
    let brief_output: Value = serde_json::from_str(&brief_output)?;
    assert_eq!(brief_output["phase"], "planning");

    // The harness rejects overlapping tracks.
    let plan_output = mock
        .function_call_output_text("plan")
        .context("plan output")?;
    assert!(plan_output.contains("non-overlapping"), "{plan_output}");

    // The lead cannot record a gate verdict for itself.
    let verdict_output = mock
        .function_call_output_text("verdict")
        .context("verdict output")?;
    assert!(
        !verdict_output.contains("\"recorded\":true"),
        "{verdict_output}"
    );

    // The run state was persisted for restarts.
    let persisted = std::fs::read_dir(state_dir.path())?
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .context("persisted run state")?;
    let persisted: Value = serde_json::from_slice(&std::fs::read(persisted.path())?)?;
    assert_eq!(persisted["phase"], "planning");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_messages_do_not_start_a_run() -> Result<()> {
    let server = start_mock_server().await;
    let state_dir = tempfile::tempdir()?;
    let mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_assistant_message("done", "hello"),
            ev_completed("r1"),
        ])],
    )
    .await;
    let test = test_codex()
        .with_extensions(teamwork_extensions(state_dir.path().to_path_buf()))
        .build(&server)
        .await?;
    test.submit_turn("please explain the teamwork module")
        .await?;

    let request = &mock.requests()[0];
    assert!(!has_tool(request, "teamwork_record_brief"));
    assert!(
        !request
            .message_input_texts("user")
            .join("\n")
            .contains("lead agent of a `/teamwork` run")
    );
    Ok(())
}
