//! End-to-end checks that the harness, not the prompt, drives a `/swarm` run.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_swarm_extension::SwarmExtensionConfig;
use codex_swarm_extension::SwarmService;
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

fn swarm_extensions(
    state_dir: std::path::PathBuf,
) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    codex_swarm_extension::install(
        &mut extensions,
        SwarmService::new(),
        Weak::new(),
        move |_config: &Config| SwarmExtensionConfig {
            enabled: true,
            state_dir: Some(state_dir.clone()),
        },
    );
    Arc::new(extensions.build())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_command_starts_a_harness_run() -> Result<()> {
    let server = start_mock_server().await;
    let state_dir = tempfile::tempdir()?;
    let overlapping = json!({
        "members": [
            {"name": "api", "role": "worker", "assignment": "backend", "files": ["src/"]},
            {"name": "ui", "role": "worker", "assignment": "frontend", "files": ["src/ui.rs"]},
            {"name": "check", "role": "verifier", "assignment": "verify"}
        ]
    });
    let roster = json!({
        "members": [
            {"name": "api", "role": "worker", "assignment": "Build the backend", "files": ["server/"], "model": "fast-model", "reasoning_effort": "low"},
            {"name": "ui", "role": "worker", "assignment": "Build the frontend", "files": ["web/"], "groups": ["frontend"]},
            {"name": "check", "role": "verifier", "assignment": "Verify both workers"}
        ]
    });
    let mock = mount_sse_sequence(
        &server,
        vec![
            // Core captures the first step's tools before turn input is contributed, so the
            // coordinator tools appear from the second step on.
            sse(vec![
                ev_response_created("r0"),
                ev_function_call("early", "swarm_status", "{}"),
                ev_completed("r0"),
            ]),
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("overlap", "swarm_define_roster", &overlapping.to_string()),
                ev_function_call(
                    "finish_early",
                    "swarm_finish",
                    &json!({"summary": "done"}).to_string(),
                ),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_function_call("roster", "swarm_define_roster", &roster.to_string()),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_response_created("r3"),
                ev_function_call(
                    "post",
                    "swarm_post",
                    &json!({"message": "@frontend please start with the login form"}).to_string(),
                ),
                ev_function_call(
                    "verify",
                    "swarm_verify",
                    &json!({"worker": "api", "verdict": "pass", "evidence": "trust me"})
                        .to_string(),
                ),
                ev_function_call(
                    "finish",
                    "swarm_finish",
                    &json!({"summary": "done"}).to_string(),
                ),
                ev_completed("r3"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Members spawned; parking."),
                ev_completed("r4"),
            ]),
        ],
    )
    .await;

    let test = test_codex()
        .with_extensions(swarm_extensions(state_dir.path().to_path_buf()))
        .build(&server)
        .await?;
    test.submit_turn("/swarm add OAuth login").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);

    // The harness injected the coordinator instructions for the literal command.
    let user_texts = requests[0].message_input_texts("user").join("\n");
    assert!(user_texts.contains("coordinator of a `/swarm` run"));
    assert!(user_texts.contains("<user_task>\nadd OAuth login\n</user_task>"));

    // Forming tools from the second step; member tools are never offered to the coordinator.
    assert!(!has_tool(&requests[0], "swarm_define_roster"));
    assert!(has_tool(&requests[1], "swarm_define_roster"));
    assert!(!has_tool(&requests[1], "swarm_post"));
    assert!(!has_tool(&requests[1], "swarm_join"));
    assert!(!has_tool(&requests[1], "swarm_verify"));

    // Overlapping worker files are rejected, and finishing is not offered while forming.
    let overlap = mock
        .function_call_output_text("overlap")
        .context("overlap output")?;
    assert!(overlap.contains("non-overlapping"), "{overlap}");
    let finish_early = mock
        .function_call_output_text("finish_early")
        .context("finish_early output")?;
    assert!(
        !finish_early.contains("\"phase\":\"finished\""),
        "{finish_early}"
    );

    // A valid roster starts the swarm and returns one spawn message per member.
    let roster_output: Value = serde_json::from_str(
        &mock
            .function_call_output_text("roster")
            .context("roster output")?,
    )?;
    assert_eq!(roster_output["phase"], "running");
    let members = roster_output["members"].as_array().context("members")?;
    assert_eq!(members.len(), 3);
    let api = members
        .iter()
        .find(|member| member["name"] == "api")
        .context("api")?;
    assert_eq!(api["model"], "fast-model");
    let spawn_message = api["spawn_message"].as_str().context("spawn message")?;
    assert!(spawn_message.contains("swarm_join"));
    assert!(spawn_message.contains("You own and may edit only: server/"));
    let check = members
        .iter()
        .find(|member| member["name"] == "check")
        .context("check")?;
    assert_eq!(check["verifies"], json!(["api", "ui"]));

    // Running tools are offered after the roster is recorded.
    assert!(has_tool(&requests[3], "swarm_post"));
    assert!(has_tool(&requests[3], "swarm_finish"));
    assert!(!has_tool(&requests[3], "swarm_define_roster"));

    // Group mentions resolve to members.
    let post: Value = serde_json::from_str(
        &mock
            .function_call_output_text("post")
            .context("post output")?,
    )?;
    assert_eq!(post["recipients"], json!(["ui"]));

    // The coordinator cannot verify, and cannot finish before workers are verified.
    let verify = mock
        .function_call_output_text("verify")
        .context("verify output")?;
    assert!(!verify.contains("\"recorded\":true"), "{verify}");
    let finish = mock
        .function_call_output_text("finish")
        .context("finish output")?;
    assert!(finish.contains("not verified"), "{finish}");

    // State and the live board transcript are persisted.
    let state = std::fs::read_dir(state_dir.path())?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".json"))
        })
        .context("persisted state")?;
    let state: Value = serde_json::from_slice(&std::fs::read(state.path())?)?;
    assert_eq!(state["phase"], "running");
    let board = std::fs::read_dir(state_dir.path())?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".board.md"))
        })
        .context("board transcript")?;
    let board = std::fs::read_to_string(board.path())?;
    assert!(
        board.contains("please start with the login form"),
        "{board}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_messages_do_not_start_a_swarm() -> Result<()> {
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
        .with_extensions(swarm_extensions(state_dir.path().to_path_buf()))
        .build(&server)
        .await?;
    test.submit_turn("please explain the swarm module").await?;

    let request = &mock.requests()[0];
    assert!(!has_tool(request, "swarm_define_roster"));
    assert!(
        !request
            .message_input_texts("user")
            .join("\n")
            .contains("coordinator of a `/swarm` run")
    );
    Ok(())
}
