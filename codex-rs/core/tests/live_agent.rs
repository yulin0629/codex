#![expect(clippy::unwrap_used, clippy::expect_used)]

//! Live integration tests that exercise the full [`Agent`] stack **against the real
//! OpenAI `/v1/responses` API**.  These tests complement the lightweight mock‑based
//! unit tests by verifying that the agent can drive an end‑to‑end conversation,
//! stream incremental events, execute function‑call tool invocations and safely
//! chain multiple turns inside a single session – the exact scenarios that have
//! historically been brittle.
//!
//! The live tests are **ignored by default** so CI remains deterministic and free
//! of external dependencies.  Developers can opt‑in locally with e.g.
//!
//! ```bash
//! OPENAI_API_KEY=sk‑... cargo test --test live_agent -- --ignored --nocapture
//! ```
//!
//! Make sure your key has access to the experimental *Responses* API and that
//! any billable usage is acceptable.

use std::time::Duration;

use codex_core::Codex;
use codex_core::error::CodexErr;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
mod test_support;
use tempfile::TempDir;
use test_support::load_default_config_for_test;
use tokio::sync::Notify;
use tokio::time::timeout;

fn api_key_available() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok()
}

/// Helper that spawns a fresh Agent and sends the mandatory *ConfigureSession*
/// submission.  The caller receives the constructed [`Agent`] plus the unique
/// submission id used for the initialization message.
async fn spawn_codex() -> Result<Codex, CodexErr> {
    assert!(
        api_key_available(),
        "OPENAI_API_KEY must be set for live tests"
    );

    // Environment tweaks to keep the tests snappy and inexpensive while still
    // exercising retry/robustness logic.
    //
    // NOTE: Starting with the 2024 edition `std::env::set_var` is `unsafe`
    // because changing the process environment races with any other threads
    // that might be performing environment look-ups at the same time.
    // Restrict the unsafety to this tiny block that happens at the very
    // beginning of the test, before we spawn any background tasks that could
    // observe the environment.
    unsafe {
        std::env::set_var("OPENAI_REQUEST_MAX_RETRIES", "2");
        std::env::set_var("OPENAI_STREAM_MAX_RETRIES", "2");
    }

    let codex_home = TempDir::new().unwrap();
    let config = load_default_config_for_test(&codex_home);
    let (agent, _init_id) = Codex::spawn(config, std::sync::Arc::new(Notify::new())).await?;

    Ok(agent)
}

/// Verifies that the agent streams incremental *AgentMessage* events **before**
/// emitting `TaskComplete` and that a second task inside the same session does
/// not get tripped up by a stale `previous_response_id`.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_streaming_and_prev_id_reset() {
    if !api_key_available() {
        eprintln!("skipping live_streaming_and_prev_id_reset – OPENAI_API_KEY not set");
        return;
    }

    let codex = spawn_codex().await.unwrap();

    // ---------- Task 1 ----------
    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "Say the words 'stream test'".into(),
            }],
        })
        .await
        .unwrap();

    let mut saw_message_before_complete = false;
    loop {
        let ev = timeout(Duration::from_secs(60), codex.next_event())
            .await
            .expect("timeout waiting for task1 events")
            .expect("agent closed");

        match ev.msg {
            EventMsg::AgentMessage(_) => saw_message_before_complete = true,
            EventMsg::TaskComplete => break,
            EventMsg::Error(ErrorEvent { message }) => {
                panic!("agent reported error in task1: {message}")
            }
            _ => {
                // Ignore other events.
            }
        }
    }

    assert!(
        saw_message_before_complete,
        "Agent did not stream any AgentMessage before TaskComplete"
    );

    // ---------- Task 2 (same session) ----------
    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "Respond with exactly: second turn succeeded".into(),
            }],
        })
        .await
        .unwrap();

    let mut got_expected = false;
    loop {
        let ev = timeout(Duration::from_secs(60), codex.next_event())
            .await
            .expect("timeout waiting for task2 events")
            .expect("agent closed");

        match &ev.msg {
            EventMsg::AgentMessage(AgentMessageEvent { message })
                if message.contains("second turn succeeded") =>
            {
                got_expected = true;
            }
            EventMsg::TaskComplete => break,
            EventMsg::Error(ErrorEvent { message }) => {
                panic!("agent reported error in task2: {message}")
            }
            _ => {
                // Ignore other events.
            }
        }
    }

    assert!(got_expected, "second task did not receive expected answer");
}

/// Exercises a *function‑call → shell execution* round‑trip by instructing the
/// model to run a harmless `echo` command.  The test asserts that:
///   1. the function call is executed (we see `ExecCommandBegin`/`End` events)
///   2. the captured stdout reaches the client unchanged.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_shell_function_call() {
    if !api_key_available() {
        eprintln!("skipping live_shell_function_call – OPENAI_API_KEY not set");
        return;
    }

    let codex = spawn_codex().await.unwrap();

    const MARKER: &str = "codex_live_echo_ok";

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: format!(
                    "Use the shell function to run the command `echo {MARKER}` and no other commands."
                ),
            }],
        })
        .await
        .unwrap();

    let mut saw_begin = false;
    let mut saw_end_with_output = false;

    loop {
        let ev = timeout(Duration::from_secs(60), codex.next_event())
            .await
            .expect("timeout waiting for function‑call events")
            .expect("agent closed");

        match ev.msg {
            EventMsg::ExecCommandBegin(codex_core::protocol::ExecCommandBeginEvent {
                command,
                call_id: _,
                cwd: _,
            }) => {
                assert_eq!(command, vec!["echo", MARKER]);
                saw_begin = true;
            }
            EventMsg::ExecCommandEnd(codex_core::protocol::ExecCommandEndEvent {
                stdout,
                exit_code,
                call_id: _,
                stderr: _,
            }) => {
                assert_eq!(exit_code, 0, "echo returned non‑zero exit code");
                assert!(stdout.contains(MARKER));
                saw_end_with_output = true;
            }
            EventMsg::TaskComplete => break,
            EventMsg::Error(codex_core::protocol::ErrorEvent { message }) => {
                panic!("agent error during shell test: {message}")
            }
            _ => {
                // Ignore other events.
            }
        }
    }

    assert!(saw_begin, "ExecCommandBegin event missing");
    assert!(
        saw_end_with_output,
        "ExecCommandEnd with expected output missing"
    );
}
