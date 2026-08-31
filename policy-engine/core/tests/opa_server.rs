#![cfg(all(feature = "opa", unix))]

use agent_control_specification_core::{canonical_json, OpaRegoRunner, RegoPolicyInvocation};
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn server_mode_matches_exec_mode_verdicts_and_reuses_one_server() {
    let Some(exec_runner) = require_opa_or_skip() else {
        return;
    };
    let server_runner = require_opa_or_skip()
        .unwrap()
        .with_server_mode(true)
        .with_data_path(fixture_path("verdict.rego"));
    let exec_runner = exec_runner.with_data_path(fixture_path("verdict.rego"));

    for text in ["hello", "please block this"] {
        let invocation = verdict_invocation(text);
        assert_eq!(
            server_runner.evaluate(&invocation).unwrap(),
            exec_runner.evaluate(&invocation).unwrap(),
            "server and exec mode disagree on {text:?}"
        );
    }
    assert_eq!(
        server_runner.active_server_pids().len(),
        1,
        "one source set must reuse one server"
    );
}

#[test]
fn server_mode_reports_an_undefined_result_exactly_as_exec_mode() {
    let Some(runner) = require_opa_or_skip() else {
        return;
    };
    let runner = runner
        .with_server_mode(true)
        .with_data_path(fixture_path("verdict.rego"));
    let invocation = invocation_for("data.agent_control_specification.input.missing", "hello");

    let error = runner.evaluate(&invocation).unwrap_err();

    assert_eq!(error.reason(), "runtime_error:policy_invocation_failed");
    assert!(
        error.detail().contains("OPA query returned no result"),
        "{}",
        error.detail()
    );
    assert_eq!(
        runner.active_server_pids().len(),
        1,
        "an undefined result is an answer, not a dead server"
    );
}

#[test]
fn server_mode_times_out_pathological_eval_without_falling_back() {
    let Some(runner) = require_opa_or_skip() else {
        return;
    };
    let slow = scratch_file(
        "slow.rego",
        "package slow\n\nverdict := {\"decision\": \"allow\"} if {\n\tcount(numbers.range(1, 100000000)) > 0\n}\n",
    );
    let runner = runner
        .with_server_mode(true)
        .with_data_path(&slow)
        .with_eval_timeout(Duration::from_millis(50));
    let invocation = invocation_for("data.slow.verdict", "hello");

    let started = Instant::now();
    let error = runner.evaluate(&invocation).unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(error.reason(), "runtime_error:policy_invocation_failed");
    assert!(
        error.detail().contains("OPA eval exceeded timeout"),
        "{}",
        error.detail()
    );
}

#[test]
fn a_killed_server_falls_back_to_exec_and_respawns_on_the_next_evaluation() {
    let Some(runner) = require_opa_or_skip() else {
        return;
    };
    let runner = runner
        .with_server_mode(true)
        .with_data_path(fixture_path("verdict.rego"));
    let invocation = verdict_invocation("please block this");

    let first = runner.evaluate(&invocation).unwrap();
    let pid = runner.active_server_pids()[0];
    kill(pid);

    let fallback = runner.evaluate(&invocation).unwrap();
    assert_eq!(first, fallback, "fallback must not change the verdict");
    assert!(
        runner.active_server_pids().is_empty(),
        "the dead server must be evicted"
    );

    let respawned = runner.evaluate(&invocation).unwrap();
    assert_eq!(first, respawned);
    let pids = runner.active_server_pids();
    assert_eq!(pids.len(), 1, "the next evaluation must respawn a server");
    assert_ne!(pids[0], pid);
}

#[test]
fn dropping_the_runner_kills_its_server() {
    let Some(runner) = require_opa_or_skip() else {
        return;
    };
    let runner = runner
        .with_server_mode(true)
        .with_data_path(fixture_path("verdict.rego"));
    runner.evaluate(&verdict_invocation("hello")).unwrap();
    let pid = runner.active_server_pids()[0];
    assert!(alive(pid), "server must be running before the drop");

    drop(runner);

    assert!(
        !alive(pid),
        "dropping the runner must kill and reap its server"
    );
}

#[test]
fn from_environment_reads_the_server_lever() {
    env::set_var("ACS_OPA_SERVER", "1");
    let enabled = OpaRegoRunner::from_environment().server_mode();
    env::remove_var("ACS_OPA_SERVER");
    let disabled = OpaRegoRunner::from_environment().server_mode();
    assert!(enabled);
    assert!(!disabled);
}

fn verdict_invocation(text: &str) -> RegoPolicyInvocation {
    invocation_for("data.agent_control_specification.input.verdict", text)
}

fn invocation_for(query: &str, text: &str) -> RegoPolicyInvocation {
    let input = json!({"policy_target": {"value": {"text": text}}});
    RegoPolicyInvocation {
        query: query.to_string(),
        bundle: None,
        bundle_url: None,
        adapter_config: Default::default(),
        canonical_input: canonical_json(&input).unwrap(),
        input,
    }
}

fn require_opa_or_skip() -> Option<OpaRegoRunner> {
    let runner = OpaRegoRunner::new();
    if runner.is_available() {
        Some(runner)
    } else if env::var("AGENT_CONTROL_REQUIRE_OPA").as_deref() == Ok("1") {
        panic!("AGENT_CONTROL_REQUIRE_OPA=1 but the 'opa' executable is not available on PATH");
    } else {
        eprintln!("skipping OPA-dependent test; set AGENT_CONTROL_REQUIRE_OPA=1 to fail when OPA is missing");
        None
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("opa")
        .join(name)
}

fn scratch_file(name: &str, contents: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("opa-server-tests")
        .join(format!("{}-{unique}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

fn kill(pid: u32) {
    assert!(Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    // The unreaped child stays a zombie (kill -0 still succeeds), but its listener
    // closes on death — a short wait is all the refused-connection path needs.
    std::thread::sleep(Duration::from_millis(200));
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
