//! End-to-end tests of the `kargo` binary against the in-process mock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;

use assert_cmd::Command;
use common::{DOWN_TOKEN, GOOD_TOKEN, Mock, NO_COMPANY_TOKEN, catalogue_json};
use predicates::prelude::*;
use serde_json::{Value, json};

struct Env {
    mock: Mock,
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self { mock: Mock::start(), dir: tempfile::tempdir().unwrap() }
    }

    fn kargo(&self) -> Command {
        let mut cmd = Command::cargo_bin("kargo").unwrap();
        cmd.env("KARGO_URL", &self.mock.url)
            .env("KARGO_CONFIG_DIR", self.dir.path().join("config"))
            .env("KARGO_CACHE_DIR", self.dir.path().join("cache"))
            .env_remove("KARGO_TOKEN")
            .env_remove("NO_COLOR");
        cmd
    }

    fn authed(&self) -> Command {
        let mut cmd = self.kargo();
        cmd.env("KARGO_TOKEN", GOOD_TOKEN);
        cmd
    }
}

fn stdout_json(cmd: &mut Command) -> Value {
    let output = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap()
}

fn stderr_json(assert: &assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stderr).unwrap()
}

#[test]
fn tools_lists_catalogue_commands() {
    let env = Env::new();
    env.kargo()
        .arg("tools")
        .assert()
        .success()
        .stdout(predicate::str::contains("ping").and(predicate::str::contains("Rename yourself")))
        .stdout(predicate::str::contains("peek"));
}

#[test]
fn tools_json_is_the_server_catalogue_verbatim() {
    let env = Env::new();
    let printed = stdout_json(env.kargo().args(["tools", "--json"]));
    assert_eq!(printed, catalogue_json(1));
}

#[test]
fn catalogue_is_cached_between_invocations() {
    let env = Env::new();
    env.kargo().arg("tools").assert().success();
    env.kargo().arg("tools").assert().success();
    assert_eq!(env.mock.catalogue_hits(), 1);
}

#[test]
fn help_prints_the_server_text() {
    let env = Env::new();
    env.kargo().arg("help").assert().success().stdout(predicate::str::starts_with("Fixture help"));
}

#[test]
fn view_passes_a_validated_section() {
    let env = Env::new();
    let json = stdout_json(env.authed().args(["view", "--section", "beta", "--json"]));
    assert_eq!(json["section"], "beta");
    env.authed()
        .args(["view", "--section", "beta"])
        .assert()
        .success()
        .stdout(predicate::str::contains("one"));
}

#[test]
fn view_rejects_unknown_sections_with_the_valid_list() {
    let env = Env::new();
    env.authed()
        .args(["view", "--section", "nope"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("alpha, beta"));
}

#[test]
fn act_builds_flags_from_the_schema() {
    let env = Env::new();
    let json = stdout_json(env.authed().args(["act", "ping", "--target", "alpha", "--count", "3", "--json"]));
    assert_eq!(json["result"], json!({"pong": "alpha", "count": 3}));
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body, json!({"command": {"command": "ping", "target": "alpha", "count": 3}}));
}

#[test]
fn act_sends_the_note() {
    let env = Env::new();
    env.authed().args(["act", "ping", "--target", "x", "--note", "hello there"]).assert().success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["note"], "hello there");
}

#[test]
fn act_resolves_schema_refs_to_enum_flags_and_booleans() {
    let env = Env::new();
    env.authed().args(["act", "rename", "--name", "Nova", "--style", "fancy"]).assert().success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"]["style"], "fancy");
    env.authed()
        .args(["act", "rename", "--name", "Nova", "--style", "loud"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("plain"));
    env.authed().args(["act", "toggle", "--on"]).assert().success();
    assert_eq!(env.mock.state.last_act_body.lock().unwrap().clone().unwrap()["command"]["on"], true);
    env.authed().args(["act", "toggle", "--on", "false", "--tags", "[\"a\"]"]).assert().success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"], json!({"command": "toggle", "on": false, "tags": ["a"]}));
}

#[test]
fn act_accepts_raw_json_input_and_flags_override_it() {
    let env = Env::new();
    env.authed()
        .args(["act", "ping", "--input", r#"{"target":"raw","count":5}"#, "--count", "7"])
        .assert()
        .success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"], json!({"command": "ping", "target": "raw", "count": 7}));
}

#[test]
fn act_help_lists_commands_and_command_help_lists_fields() {
    let env = Env::new();
    env.kargo()
        .args(["act", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ping").and(predicate::str::contains("Rename yourself")));
    env.kargo()
        .args(["act", "ping", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--target").and(predicate::str::contains("(required)")))
        .stdout(predicate::str::contains("kargo act ping --target alpha --count 2"));
}

#[test]
fn schema_violations_are_caught_locally_with_exit_2() {
    let env = Env::new();
    env.authed()
        .args(["act", "ping", "--count", "2"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("target"));
    env.authed().args(["act", "ping", "--target", "x", "--count", "0"]).assert().code(2);
    env.authed().args(["act", "ping", "--target", "x", "--count", "many"]).assert().code(2);
    assert_eq!(env.mock.state.act_hits.load(Ordering::SeqCst), 0, "nothing may reach the server");
}

#[test]
fn unknown_commands_and_flags_are_usage_errors() {
    let env = Env::new();
    env.authed().args(["act", "teleport"]).assert().code(2);
    env.authed().args(["act", "ping", "--bogus", "1"]).assert().code(2);
    env.authed().arg("bogus").assert().code(2);
}

#[test]
fn rule_refusals_exit_5() {
    let env = Env::new();
    let assert = env.authed().args(["act", "rename", "--name", "taken", "--json"]).assert().code(5);
    let error = stderr_json(&assert);
    assert_eq!(error["kind"], "refused");
    assert_eq!(error["error"], "that name is taken");
    assert_eq!(String::from_utf8_lossy(&assert.get_output().stdout), "");
}

#[test]
fn missing_or_bad_token_exits_3() {
    let env = Env::new();
    let assert = env.kargo().args(["view", "--json"]).assert().code(3);
    let error = stderr_json(&assert);
    assert_eq!(error["kind"], "auth");
    assert!(error["hint"].as_str().unwrap().contains("auth login"));
    env.kargo().env("KARGO_TOKEN", "wrong").args(["act", "peek"]).assert().code(3);
}

#[test]
fn no_company_exits_7() {
    let env = Env::new();
    env.kargo().env("KARGO_TOKEN", NO_COMPANY_TOKEN).args(["act", "peek"]).assert().code(7);
}

#[test]
fn server_errors_exit_6() {
    let env = Env::new();
    env.kargo().env("KARGO_TOKEN", DOWN_TOKEN).args(["act", "peek"]).assert().code(6);
}

#[test]
fn unreachable_server_exits_6_and_reports_json() {
    let env = Env::new();
    let assert =
        env.kargo().env("KARGO_URL", "http://127.0.0.1:1").args(["tools", "--json"]).assert().code(6);
    assert_eq!(stderr_json(&assert)["kind"], "unavailable");
}

#[test]
fn rate_limits_are_waited_out_then_retried() {
    let env = Env::new();
    env.mock.state.rate_limited_acts.store(1, Ordering::SeqCst);
    let started = std::time::Instant::now();
    env.authed().args(["act", "peek"]).assert().success();
    assert!(started.elapsed() >= std::time::Duration::from_secs(1), "must honour Retry-After");
    assert_eq!(env.mock.state.act_hits.load(Ordering::SeqCst), 2);
}

#[test]
fn persistent_rate_limiting_exits_4_without_hammering() {
    let env = Env::new();
    env.mock.state.rate_limited_acts.store(100, Ordering::SeqCst);
    env.authed().args(["act", "peek", "--max-wait", "2"]).assert().code(4);
    assert!(env.mock.state.act_hits.load(Ordering::SeqCst) <= 3);
    env.mock.state.act_hits.store(0, Ordering::SeqCst);
    env.authed().args(["--max-wait", "0", "act", "peek"]).assert().code(4);
    assert_eq!(env.mock.state.act_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn newer_catalogue_schema_asks_for_an_upgrade() {
    let env = Env::new();
    *env.mock.state.schema_version.lock().unwrap() = Some(2);
    let assert = env.kargo().args(["tools", "--json"]).assert().code(1);
    let error = stderr_json(&assert);
    assert_eq!(error["kind"], "upgrade_required");
    assert!(error["hint"].as_str().unwrap().contains("upgrade"));
}

#[test]
fn auth_login_status_logout_round_trip() {
    let env = Env::new();
    env.kargo().args(["auth", "status"]).assert().success().stdout(predicate::str::contains("No token"));
    env.kargo()
        .args(["auth", "login"])
        .write_stdin(format!("{GOOD_TOKEN}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("good…6789").and(predicate::str::contains(GOOD_TOKEN).not()));
    let status = stdout_json(env.kargo().args(["auth", "status", "--json"]));
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["token"], "good…6789");
    // The stored token is used without KARGO_TOKEN.
    env.kargo().args(["act", "peek"]).assert().success();
    env.kargo().args(["auth", "logout"]).assert().success();
    env.kargo().args(["act", "peek"]).assert().code(3);
}

#[cfg(unix)]
#[test]
fn stored_token_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.kargo().args(["auth", "login"]).write_stdin("abcdefgh12345678\n").assert().success();
    let mode = std::fs::metadata(env.dir.path().join("config/token")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn login_rejects_empty_input() {
    let env = Env::new();
    env.kargo().args(["auth", "login"]).write_stdin("\n").assert().code(2);
}

#[test]
fn environment_token_wins_over_the_stored_one() {
    let env = Env::new();
    env.kargo().args(["auth", "login"]).write_stdin("stored-token-123456\n").assert().success();
    env.authed().args(["act", "peek"]).assert().success();
}

#[test]
fn colour_is_off_when_piped_on_when_forced_and_no_color_wins() {
    let env = Env::new();
    env.authed().arg("view").assert().success().stdout(predicate::str::contains("\x1b").not());
    env.authed()
        .arg("view")
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b["));
    env.authed()
        .arg("view")
        .env("CLICOLOR_FORCE", "1")
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b").not());
    env.authed()
        .args(["act", "rename", "--name", "taken"])
        .env("CLICOLOR_FORCE", "1")
        .env("NO_COLOR", "1")
        .assert()
        .code(5)
        .stderr(predicate::str::contains("\x1b").not());
}

#[test]
fn negative_numbers_are_accepted_for_integer_and_number_flags() {
    let env = Env::new();
    env.authed().args(["act", "shift", "--dx", "-3", "--scale", "-1.5"]).assert().success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"], json!({"command": "shift", "dx": -3, "scale": -1.5}));
}

#[test]
fn clashing_field_names_are_reachable_only_through_input_and_help_says_so() {
    let env = Env::new();
    env.kargo()
        .args(["act", "tangle", "--help"])
        .env("COLUMNS", "300")
        .assert()
        .success()
        .stdout(predicate::str::contains("Set only through --input"))
        .stdout(predicate::str::contains("clash with a kargo option): max_wait, note"))
        .stdout(predicate::str::contains("--ok-field"));
    env.authed()
        .args([
            "act",
            "tangle",
            "--ok-field",
            "z",
            "--note",
            "n",
            "--input",
            r#"{"my_key":"a","max_wait":"b","note":"field"}"#,
        ])
        .assert()
        .success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(
        body["command"],
        json!({"command": "tangle", "my_key": "a", "max_wait": "b", "note": "field", "ok_field": "z"})
    );
    assert_eq!(body["note"], "n", "--note stays the action note");
}

#[test]
fn http_400_is_a_usage_error_exit_2() {
    let env = Env::new();
    let assert = env.authed().args(["act", "ping", "--target", "reject400", "--json"]).assert().code(2);
    let error = stderr_json(&assert);
    assert_eq!(error["kind"], "usage");
    assert_eq!(error["error"], "malformed request");
}

#[test]
fn json_usage_errors_are_exactly_one_object_on_stderr() {
    let env = Env::new();
    for args in [
        vec!["--json", "act"],
        vec!["act", "--json"],
        vec!["act", "ping", "--bogus", "1", "--json"],
        vec!["act", "teleport", "--json"],
        vec!["--json", "bogus"],
    ] {
        let assert = env.authed().args(&args).assert().code(2);
        let error = stderr_json(&assert);
        assert_eq!(error["kind"], "usage", "{args:?}");
        assert_eq!(String::from_utf8_lossy(&assert.get_output().stdout), "", "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_a_clean_usage_error() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    let bad = std::ffi::OsStr::from_bytes(b"\xff\xfe");
    env.kargo().arg(bad).assert().code(2).stderr(predicate::str::contains("panicked").not());
    env.authed()
        .args(["act", "ping", "--target"])
        .arg(bad)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("panicked").not());
}

/// Run `kargo` with stdout closed before it can write, and return its exit status and stderr.
#[cfg(unix)]
fn run_with_closed_stdout(env: &Env, args: &[&str]) -> (std::process::ExitStatus, String) {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_kargo"))
        .args(args)
        .env("KARGO_URL", &env.mock.url)
        .env("KARGO_CONFIG_DIR", env.dir.path().join("config"))
        .env("KARGO_CACHE_DIR", env.dir.path().join("cache"))
        .env("KARGO_TOKEN", GOOD_TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    (child.wait().unwrap(), stderr)
}

#[cfg(unix)]
#[test]
fn a_closed_stdout_is_a_quiet_success_not_a_panic() {
    let env = Env::new();
    for args in [&["tools"][..], &["view"], &["help"], &["completions", "bash"], &["--help"]] {
        let (status, stderr) = run_with_closed_stdout(&env, args);
        assert!(status.success(), "{args:?}: {status:?} {stderr}");
        assert_eq!(stderr, "", "{args:?}");
    }
}

#[test]
fn terminal_escape_sequences_from_the_server_never_reach_the_terminal() {
    let env = Env::new();
    env.mock.state.hostile.store(true, Ordering::SeqCst);
    let clean = |assert: assert_cmd::assert::Assert| {
        let output = assert.get_output();
        let all =
            format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(!all.contains(['\u{1b}', '\u{7}', '\u{9b}']), "{all:?}");
        all
    };
    assert!(clean(env.kargo().arg("help").assert().success()).contains("Fixture help"));
    assert!(clean(env.kargo().arg("tools").assert().success()).contains("Pe[31mek"));
    clean(env.kargo().args(["act", "--help"]).assert().success());
    assert!(
        clean(env.authed().args(["act", "rename", "--name", "hostile"]).assert().code(5))
            .contains("bad]0;owned name")
    );
}

#[test]
fn hostile_keys_names_examples_and_enum_values_never_reach_the_terminal() {
    let env = Env::new();
    env.mock.state.hostile.store(true, Ordering::SeqCst);
    let text_of = |assert: assert_cmd::assert::Assert| {
        let output = assert.get_output();
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
    };
    let clean = |text: String| {
        assert!(!text.contains(['\u{1b}', '\u{7}', '\u{9b}']), "{text:?}");
        text
    };
    let view = clean(text_of(env.authed().arg("view").assert().success()));
    assert!(view.contains("k]0;PWNED[2J"), "keys are shown, cleaned: {view}");
    clean(text_of(env.authed().args(["act", "rename", "--help"]).assert().success()));
    clean(text_of(env.authed().args(["act", "rename", "--name", "x", "--style", "nope"]).assert().code(2)));
    let tools = clean(text_of(env.authed().arg("tools").assert().success()));
    assert!(!tools.contains("Evil"), "the command with the unusable name is not offered: {tools}");
    assert!(tools.contains("warning: skipped"), "{tools}");
}

#[test]
fn hostile_catalogues_never_panic_and_the_first_of_a_repeated_name_wins() {
    let env = Env::new();
    env.mock.state.junk_names.store(true, Ordering::SeqCst);
    let quiet = |assert: assert_cmd::assert::Assert| {
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(!stderr.contains("panicked"), "{stderr}");
        stderr
    };
    let warned = quiet(env.authed().arg("tools").assert().success());
    assert!(warned.contains("warning: skipped") && warned.contains("`-x`"), "{warned}");
    quiet(env.authed().args(["act", "--help"]).assert().success());
    quiet(env.authed().args(["act", "ping", "--help"]).assert().success());
    quiet(env.authed().args(["act", "peek", "--help"]).assert().success());
    quiet(env.authed().args(["act", "peek", "--ok-field", "z"]).assert().success());
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"], json!({"command": "peek", "ok_field": "z"}));
    quiet(env.authed().args(["act", "ping", "--target", "t"]).assert().success());
    quiet(env.authed().args(["act", "-x"]).assert().code(2));
    // The skipped fields are still reachable (and validated) through --input.
    quiet(env.authed().args(["act", "peek", "--input", r#"{"-x": "v"}"#]).assert().success());
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"]["-x"], "v");
}

#[test]
fn json_output_stays_pure_when_the_catalogue_has_unusable_entries() {
    let env = Env::new();
    env.mock.state.junk_names.store(true, Ordering::SeqCst);
    let assert = env.authed().args(["act", "peek", "--json"]).assert().success();
    assert_eq!(String::from_utf8_lossy(&assert.get_output().stderr), "");
    let assert = env.authed().args(["tools", "--json"]).assert().success();
    assert_eq!(String::from_utf8_lossy(&assert.get_output().stderr), "");
    let assert = env.authed().args(["view", "--json"]).assert().success();
    assert_eq!(String::from_utf8_lossy(&assert.get_output().stderr), "");
}

#[test]
fn input_that_names_a_command_is_a_schema_error_and_never_sent() {
    let env = Env::new();
    let assert = env
        .authed()
        .args(["act", "ping", "--target", "x", "--input", r#"{"command":"other_cmd"}"#, "--json"])
        .assert()
        .code(2);
    assert_eq!(stderr_json(&assert)["kind"], "schema");
    env.authed().args(["act", "peek", "--input", r#"{"command":"destroy_thing"}"#]).assert().code(2);
    assert_eq!(env.mock.state.act_hits.load(Ordering::SeqCst), 0, "the server never sees it");
    env.authed().args(["act", "ping", "--target", "x"]).assert().success();
    let body = env.mock.state.last_act_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["command"]["command"], "ping");
}

#[test]
fn tools_json_writes_the_servers_bytes_verbatim_fresh_and_from_the_cache() {
    let env = Env::new();
    env.mock.state.pretty_catalogue.store(true, Ordering::SeqCst);
    let expected = format!("{}\n\n", serde_json::to_string_pretty(&catalogue_json(1)).unwrap());
    for _ in 0..2 {
        let output = env.kargo().args(["tools", "--json"]).assert().success().get_output().stdout.clone();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
    assert_eq!(env.mock.catalogue_hits(), 1, "the second run came from the cache");
}

#[test]
fn a_reply_beyond_the_size_cap_is_a_protocol_error_with_exit_8() {
    let env = Env::new();
    env.mock.state.huge_view.store(true, Ordering::SeqCst);
    let assert = env.authed().args(["view", "--json"]).assert().code(8);
    assert_eq!(stderr_json(&assert)["kind"], "protocol");
}

#[test]
fn auth_status_asks_the_server_and_says_when_it_cannot() {
    let env = Env::new();
    let status = |cmd: &mut Command| stdout_json(cmd.args(["auth", "status", "--json"]));
    let good = status(&mut env.authed());
    assert_eq!(good["authenticated"], true);
    assert_eq!(good["token_present"], true);
    let wrong = status(env.kargo().env("KARGO_TOKEN", "wrong-token-12345678"));
    assert_eq!(wrong["authenticated"], false);
    assert_eq!(wrong["token_present"], true);
    let none = status(&mut env.kargo());
    assert_eq!(none["authenticated"], false);
    assert_eq!(none["token_present"], false);
    let offline = status(env.authed().env("KARGO_URL", "http://127.0.0.1:1"));
    assert!(offline["authenticated"].is_null(), "{offline}");
    assert_eq!(offline["token_present"], true);
    assert!(offline["error"].as_str().unwrap().contains("cannot reach"));
    env.authed().args(["auth", "status"]).assert().success().stdout(predicate::str::contains("accepted"));
    env.kargo()
        .env("KARGO_TOKEN", "wrong-token-12345678")
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("refused"));
}

#[test]
fn an_enormous_retry_after_is_clamped_instead_of_panicking() {
    let env = Env::new();
    *env.mock.state.retry_after.lock().unwrap() = "1e20".into();
    env.mock.state.rate_limited_acts.store(5, Ordering::SeqCst);
    env.authed().args(["act", "peek"]).assert().code(4).stderr(predicate::str::contains("panicked").not());
}

#[test]
fn schemas_with_external_references_are_a_clean_schema_error() {
    let env = Env::new();
    for name in ["xref_http", "xref_file"] {
        let assert = env.authed().args(["act", name, "--input", r#"{"a": 1}"#, "--json"]).assert().code(2);
        assert_eq!(stderr_json(&assert)["kind"], "schema", "{name}");
        assert!(!String::from_utf8_lossy(&assert.get_output().stderr).contains("panicked"));
    }
}

#[test]
fn completions_need_no_home_directory() {
    Command::cargo_bin("kargo")
        .unwrap()
        .env_clear()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kargo"));
}

#[cfg(unix)]
#[test]
fn serve_needs_no_home_directory_and_exits_cleanly_on_sigterm() {
    use std::io::{BufRead as _, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mock = Mock::start();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kargo"))
        .args(["serve", "--bind", "127.0.0.1:0", "--url", &mock.url])
        .env_clear()
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("listening on"), "{line:?}");
    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "serve did not shut down on SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
}

#[test]
fn completions_are_generated() {
    Command::cargo_bin("kargo")
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kargo"));
}

#[test]
fn top_level_help_documents_exit_codes_and_environment() {
    Command::cargo_bin("kargo")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes:").and(predicate::str::contains("KARGO_TOKEN")));
}

#[test]
fn usage_errors_are_json_when_asked() {
    let assert = Command::cargo_bin("kargo").unwrap().args(["--json", "bogus"]).assert().code(2);
    assert_eq!(stderr_json(&assert)["kind"], "usage");
}

#[test]
fn a_request_timeout_below_one_second_is_a_usage_error() {
    for args in [vec!["serve", "--request-timeout", "0"], vec!["serve", "--request-timeout", "0", "--json"]] {
        // `--bind 127.0.0.1:0` and a timeout keep a regression from serving forever.
        let mut cmd = Command::cargo_bin("kargo").unwrap();
        let assert = cmd
            .args(&args)
            .args(["--bind", "127.0.0.1:0"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .code(2);
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(stderr.contains("request-timeout"), "{stderr}");
        if args.contains(&"--json") {
            assert_eq!(stderr_json(&assert)["kind"], "usage");
        }
    }
}

#[test]
fn a_sent_command_answered_by_anything_but_the_games_verdict_is_unknown_exit_9() {
    let env = Env::new();
    for target in ["gateway502", "page200"] {
        let assert = env.authed().args(["act", "ping", "--target", target, "--json"]).assert().code(9);
        let error = stderr_json(&assert);
        assert_eq!(error["kind"], "unknown", "{target}");
        assert!(!error["hint"].as_str().unwrap().contains("retry"), "{target}");
    }
    let assert = env.authed().args(["act", "ping", "--target", "down503", "--json"]).assert().code(6);
    assert_eq!(stderr_json(&assert)["kind"], "unavailable", "a 503 means the game is down");
}
