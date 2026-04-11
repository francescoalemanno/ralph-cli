#![cfg(not(windows))]

use std::{fs, process::{Command, Stdio}};

use camino::Utf8PathBuf;

#[test]
fn hidden_test_workflow_runs_end_to_end_via_the_cli_binary() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp.path().join("project");
    let config_home = temp.path().join("config-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(&config_home).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ralph"))
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("run")
        .arg("--cli")
        .arg("--agent")
        .arg("__test_shell")
        .arg("test-workflow")
        .env("HOME", &home_dir)
        .env("RALPH_CONFIG_HOME", &config_home)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\n\nstderr:\n{stderr}"
    );

    assert!(stdout.contains("alpha round 1"));
    assert!(stdout.contains("beta round 4"));
    assert!(stdout.contains("gamma round 4"));
    assert!(stdout.contains("finalize after 4 rounds"));
    assert!(stdout.contains("Workflow complete: completed 4 coordinated rounds"));
    assert!(stdout.contains("test-workflow [completed] prompt=finalize"));
    assert!(!stdout.contains("<<<PAYLOAD:"));
    assert!(!stdout.contains("<<<SIGNAL:"));

    let run_root = project_dir
        .join(".ralph")
        .join("runs")
        .join("test-workflow");
    let mut run_dirs = fs::read_dir(&run_root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(run_dirs.len(), 1);
    let run_dir = run_dirs.pop().unwrap();

    let worklog = fs::read_to_string(run_dir.join("worklog.txt")).unwrap();
    assert!(worklog.contains("init"));
    assert!(worklog.contains("alpha:1"));
    assert!(worklog.contains("beta:4"));
    assert!(worklog.contains("gamma:4"));
    assert!(worklog.contains("finalize:4"));

    let summary = fs::read_to_string(run_dir.join("summary.txt")).unwrap();
    assert!(summary.contains("rounds=4"));
    assert!(summary.contains("entries=14"));

    let wal = ralph_core::read_agent_events_since(&Utf8PathBuf::from_path_buf(run_dir).unwrap(), 0)
        .unwrap();
    let phase_events = wal
        .records
        .iter()
        .filter(|record| record.event == "test-phase")
        .map(|record| record.body.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        phase_events,
        vec![
            "init",
            "alpha:1",
            "beta:1",
            "gamma:1",
            "alpha:2",
            "beta:2",
            "gamma:2",
            "alpha:3",
            "beta:3",
            "gamma:3",
            "alpha:4",
            "beta:4",
            "gamma:4",
            "finalize:4",
        ]
    );
    assert_eq!(
        wal.records.last().map(|record| record.event.as_str()),
        Some("loop-stop:ok")
    );
    assert_eq!(
        wal.records.last().map(|record| record.body.as_str()),
        Some("completed 4 coordinated rounds")
    );
}

#[test]
fn cli_run_reports_explicit_session_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp.path().join("project");
    let config_home = temp.path().join("config-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(&config_home).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ralph"))
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("run")
        .arg("--cli")
        .arg("--agent")
        .arg("__test_shell")
        .arg("--session-timeout")
        .arg("1s")
        .arg("test-timeout-workflow")
        .arg("--script")
        .arg("sleep 2")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\n\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("runner session timeout after 1s"));
}

#[test]
fn cli_run_reports_explicit_idle_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp.path().join("project");
    let config_home = temp.path().join("config-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(&config_home).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ralph"))
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("run")
        .arg("--cli")
        .arg("--agent")
        .arg("__test_shell")
        .arg("--idle-timeout")
        .arg("1s")
        .arg("test-timeout-workflow")
        .arg("--script")
        .arg("printf warmup; sleep 2")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\n\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("warmup"));
    assert!(stderr.contains("runner idle timeout after 1s"));
}
