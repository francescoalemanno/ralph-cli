use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::protocol::PLANNING_QUESTION_EVENT;
use crate::workflow::load_workflow_from_path;

pub const RUNTIME_DIR_NAME: &str = ".ralph-runtime";
pub const AGENT_EVENTS_WAL_FILE_NAME: &str = "agent-events.wal.ndjson";
pub const MAIN_CHANNEL_ID: &str = "main";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventRecord {
    pub v: u8,
    pub ts_unix_ms: u64,
    pub run_id: String,
    #[serde(default = "default_channel_id")]
    pub channel_id: String,
    pub event: String,
    pub body: String,
    pub project_dir: Utf8PathBuf,
    pub run_dir: Utf8PathBuf,
    pub prompt_path: Utf8PathBuf,
    pub prompt_name: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEventLogRead {
    pub next_offset: u64,
    pub records: Vec<AgentEventRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAgentEvent {
    pub event: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopControlDecision {
    Continue,
    StopOk(String),
    StopError(String),
    Route(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningQuestionJsonPayload {
    pub question: String,
    pub options: Vec<String>,
    pub context: String,
}

#[derive(Debug, Deserialize)]
struct RawPlanningQuestionJsonPayload {
    question: Option<String>,
    options: Option<Vec<String>>,
    context: Option<String>,
}

fn default_channel_id() -> String {
    MAIN_CHANNEL_ID.to_owned()
}

pub fn agent_events_wal_path(run_dir: &Utf8Path) -> Utf8PathBuf {
    run_dir
        .join(RUNTIME_DIR_NAME)
        .join(AGENT_EVENTS_WAL_FILE_NAME)
}

pub fn current_agent_events_offset(run_dir: &Utf8Path) -> Result<u64> {
    let path = agent_events_wal_path(run_dir);
    match fs::metadata(&path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path)),
    }
}

pub fn append_agent_event(run_dir: &Utf8Path, record: &AgentEventRecord) -> Result<()> {
    append_agent_event_to_wal_path(&agent_events_wal_path(run_dir), record)
}

pub fn append_agent_event_to_wal_path(
    wal_path: &Utf8Path,
    record: &AgentEventRecord,
) -> Result<()> {
    if let Some(parent) = wal_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(wal_path)
        .with_context(|| format!("failed to open {}", wal_path))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("failed to serialize event for {}", wal_path))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", wal_path))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", wal_path))?;
    Ok(())
}

pub fn read_agent_events_since(run_dir: &Utf8Path, offset: u64) -> Result<AgentEventLogRead> {
    read_agent_events_since_path(&agent_events_wal_path(run_dir), offset)
}

pub fn reduce_loop_control(
    records: &[AgentEventRecord],
    current_prompt_name: &str,
) -> Option<LoopControlDecision> {
    let mut decision = None;
    for record in records {
        if record.channel_id != MAIN_CHANNEL_ID {
            continue;
        }
        let next = match record.event.as_str() {
            "loop-continue" => Some(LoopControlDecision::Continue),
            "loop-stop:ok" => Some(LoopControlDecision::StopOk(record.body.clone())),
            "loop-stop:error" => Some(LoopControlDecision::StopError(record.body.clone())),
            "loop-route" if record.body == current_prompt_name => {
                Some(LoopControlDecision::Continue)
            }
            "loop-route" => Some(LoopControlDecision::Route(record.body.clone())),
            _ => None,
        };
        if let Some(next) = next {
            decision = Some(next);
        }
    }
    decision
}

pub fn read_agent_events_since_path(wal_path: &Utf8Path, offset: u64) -> Result<AgentEventLogRead> {
    let file = match std::fs::File::open(wal_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentEventLogRead {
                next_offset: 0,
                records: Vec::new(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("failed to read {}", wal_path)),
    };

    let len = file
        .metadata()
        .with_context(|| format!("failed to stat {}", wal_path))?
        .len();
    let start = offset.min(len);
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek {}", wal_path))?;

    let mut records = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .with_context(|| format!("failed to read {}", wal_path))?;
        if bytes_read == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<AgentEventRecord>(trimmed) {
            records.push(record);
        }
    }

    Ok(AgentEventLogRead {
        next_offset: len,
        records,
    })
}

pub fn latest_agent_event_body_from_wal(
    wal_path: &Utf8Path,
    event: &str,
) -> Result<Option<String>> {
    latest_agent_event_body_from_wal_in_channel(wal_path, event, None)
}

pub fn latest_agent_event_body_from_wal_in_channel(
    wal_path: &Utf8Path,
    event: &str,
    channel_id: Option<&str>,
) -> Result<Option<String>> {
    let records = read_agent_events_since_path(wal_path, 0)?.records;
    Ok(records
        .into_iter()
        .rev()
        .find(|record| {
            record.event == event
                && channel_id.is_none_or(|channel_id| record.channel_id == channel_id)
        })
        .map(|record| record.body))
}

pub fn validate_agent_event(event: &str, body: &str, prompt_path: Option<&Utf8Path>) -> Result<()> {
    let trimmed_event = event.trim();
    if trimmed_event.is_empty() {
        return Err(anyhow!("event name cannot be empty"));
    }

    if trimmed_event == PLANNING_QUESTION_EVENT {
        parse_planning_question_json_payload(body)?;
        return Ok(());
    }

    if !trimmed_event.starts_with("loop-") {
        return Ok(());
    }

    match trimmed_event {
        "loop-continue" | "loop-stop:ok" | "loop-stop:error" => Ok(()),
        "loop-route" => validate_loop_route_body(body, prompt_path),
        _ => Err(anyhow!(unsupported_loop_event_message(trimmed_event))),
    }
}

pub fn parse_planning_question_json_payload(body: &str) -> Result<PlanningQuestionJsonPayload> {
    let payload: RawPlanningQuestionJsonPayload =
        serde_json::from_str(body).context("planning-question JSON payload is invalid")?;

    let question = payload
        .question
        .map(|question| question.trim().to_owned())
        .filter(|question| !question.is_empty())
        .ok_or_else(|| {
            anyhow!("planning-question JSON payload must include a non-empty `question`")
        })?;

    let options = payload
        .options
        .unwrap_or_default()
        .into_iter()
        .map(|option| option.trim().to_owned())
        .filter(|option| !option.is_empty())
        .collect::<Vec<_>>();
    if options.is_empty() {
        return Err(anyhow!(
            "planning-question JSON payload must include at least one non-empty entry in `options`"
        ));
    }

    let context = payload
        .context
        .map(|context| context.trim().to_owned())
        .filter(|context| !context.is_empty())
        .ok_or_else(|| {
            anyhow!("planning-question JSON payload must include a non-empty `context`")
        })?;

    Ok(PlanningQuestionJsonPayload {
        question,
        options,
        context,
    })
}

fn validate_loop_route_body(body: &str, prompt_path: Option<&Utf8Path>) -> Result<()> {
    let trimmed = body.trim();
    let routes = available_routes(prompt_path)?;
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!(invalid_route_message(trimmed, &routes)));
    }
    if routes.iter().any(|route| route == trimmed) {
        return Ok(());
    }
    Err(anyhow!(invalid_route_message(trimmed, &routes)))
}

fn available_routes(prompt_path: Option<&Utf8Path>) -> Result<Vec<String>> {
    let prompt_path = prompt_path
        .filter(|path| !path.as_str().is_empty())
        .ok_or_else(|| {
            anyhow!("missing RALPH_PROMPT_PATH; `loop-route` requires workflow source context")
        })?;
    let workflow = load_workflow_from_path(prompt_path)?;
    Ok(workflow
        .prompt_ids()
        .into_iter()
        .map(str::to_owned)
        .collect())
}

fn invalid_route_message(route: &str, routes: &[String]) -> String {
    if routes.is_empty() {
        format!("\"{route}\" is not a valid event body for `loop-route`.\nNo routes are available.")
    } else {
        format!(
            "\"{route}\" is not a valid event body for `loop-route`.\nChoose among the available routes:\n{}",
            routes.join("\n")
        )
    }
}

fn unsupported_loop_event_message(event: &str) -> String {
    format!(
        "`{event}` is not a supported loop event.\nChoose among:\nloop-continue\nloop-stop:ok\nloop-stop:error\nloop-route"
    )
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::{
        AGENT_EVENTS_WAL_FILE_NAME, AgentEventRecord, LoopControlDecision, MAIN_CHANNEL_ID,
        PlanningQuestionJsonPayload, RUNTIME_DIR_NAME, agent_events_wal_path, append_agent_event,
        append_agent_event_to_wal_path, current_agent_events_offset,
        latest_agent_event_body_from_wal, latest_agent_event_body_from_wal_in_channel,
        parse_planning_question_json_payload, read_agent_events_since, reduce_loop_control,
        validate_agent_event,
    };

    fn sample_record(event: &str, body: &str, run_id: &str) -> AgentEventRecord {
        AgentEventRecord {
            v: 1,
            ts_unix_ms: 42,
            run_id: run_id.to_owned(),
            channel_id: MAIN_CHANNEL_ID.to_owned(),
            event: event.to_owned(),
            body: body.to_owned(),
            project_dir: Utf8PathBuf::from("/tmp/project"),
            run_dir: Utf8PathBuf::from("/tmp/project/.ralph/runs/fixture-flow/run-1"),
            prompt_path: Utf8PathBuf::from("/tmp/.config/ralph/workflows/fixture-flow.yml"),
            prompt_name: "task".to_owned(),
            pid: 123,
        }
    }

    #[test]
    fn wal_path_lives_under_run_runtime_dir() {
        let run_dir = Utf8PathBuf::from("/tmp/project/.ralph/runs/fixture-flow/run-1");
        assert_eq!(
            agent_events_wal_path(&run_dir),
            run_dir
                .join(RUNTIME_DIR_NAME)
                .join(AGENT_EVENTS_WAL_FILE_NAME)
        );
    }

    #[test]
    fn append_and_read_events_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let first = sample_record("note", "hello", "run-1");
        let second = sample_record("loop-stop:ok", "done", "run-1");

        append_agent_event(&run_dir, &first).unwrap();
        let offset = current_agent_events_offset(&run_dir).unwrap();
        append_agent_event(&run_dir, &second).unwrap();

        let read = read_agent_events_since(&run_dir, offset).unwrap();
        assert_eq!(read.records, vec![second]);
        assert!(read.next_offset >= offset);
    }

    #[test]
    fn append_to_explicit_wal_path_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let wal_path = Utf8PathBuf::from_path_buf(temp.path().join("events.wal.ndjson")).unwrap();
        let record = sample_record("note", "hello", "run-1");

        append_agent_event_to_wal_path(&wal_path, &record).unwrap();

        let read = super::read_agent_events_since_path(&wal_path, 0).unwrap();
        assert_eq!(read.records, vec![record]);
    }

    #[test]
    fn last_loop_event_wins() {
        let records = vec![
            sample_record("note", "observed", "run-1"),
            sample_record("loop-continue", "keep going", "run-1"),
            sample_record("loop-stop:error", "blocked", "run-1"),
        ];

        assert_eq!(
            reduce_loop_control(&records, "task"),
            Some(LoopControlDecision::StopError("blocked".to_owned()))
        );
    }

    #[test]
    fn route_to_current_prompt_collapses_to_continue() {
        let records = vec![sample_record("loop-route", "task", "run-1")];

        assert_eq!(
            reduce_loop_control(&records, "task"),
            Some(LoopControlDecision::Continue)
        );
    }

    #[test]
    fn latest_event_reads_from_wal_path() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        append_agent_event(&run_dir, &sample_record("phase", "first", "run-1")).unwrap();
        append_agent_event(&run_dir, &sample_record("phase", "second", "run-1")).unwrap();

        let latest =
            latest_agent_event_body_from_wal(&agent_events_wal_path(&run_dir), "phase").unwrap();
        assert_eq!(latest.as_deref(), Some("second"));
    }

    #[test]
    fn latest_event_can_filter_by_channel() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let mut first = sample_record("review", "qt", "run-1");
        first.channel_id = "QT".to_owned();
        append_agent_event(&run_dir, &first).unwrap();
        let mut second = sample_record("review", "oe", "run-1");
        second.channel_id = "OE".to_owned();
        append_agent_event(&run_dir, &second).unwrap();

        let global =
            latest_agent_event_body_from_wal(&agent_events_wal_path(&run_dir), "review").unwrap();
        let qt = latest_agent_event_body_from_wal_in_channel(
            &agent_events_wal_path(&run_dir),
            "review",
            Some("QT"),
        )
        .unwrap();

        assert_eq!(global.as_deref(), Some("oe"));
        assert_eq!(qt.as_deref(), Some("qt"));
    }

    #[test]
    fn validates_route_events_against_workflow_prompt_ids() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_path = Utf8PathBuf::from_path_buf(temp.path().join("route-test.yml")).unwrap();
        std::fs::write(
            workflow_path.as_std_path(),
            r#"
version: 1
workflow_id: route-test
title: Route Test
entrypoint: alpha
prompts:
  alpha:
    title: Alpha
    fallback-route: no-route-error
    prompt: hello
  beta:
    title: Beta
    fallback-route: no-route-error
    prompt: world
"#,
        )
        .unwrap();

        validate_agent_event("loop-route", "beta", Some(&workflow_path)).unwrap();
        let error = validate_agent_event("loop-route", "broken", Some(&workflow_path))
            .unwrap_err()
            .to_string();
        assert!(error.contains("\"broken\" is not a valid event body for `loop-route`."));
        assert!(error.contains("alpha"));
        assert!(error.contains("beta"));
    }

    #[test]
    fn parses_planning_question_json_payload() {
        let payload = parse_planning_question_json_payload(
            r#"{"question":"What do you want to build?","options":["CLI","Web"],"context":"Needed for planning"}"#,
        )
        .unwrap();
        assert_eq!(
            payload,
            PlanningQuestionJsonPayload {
                question: "What do you want to build?".to_owned(),
                options: vec!["CLI".to_owned(), "Web".to_owned()],
                context: "Needed for planning".to_owned(),
            }
        );
    }

    #[test]
    fn planning_question_requires_non_empty_question_options_and_context() {
        let missing_question =
            parse_planning_question_json_payload(r#"{"options":["CLI"],"context":"Needed"}"#)
                .unwrap_err()
                .to_string();
        assert!(missing_question.contains("non-empty `question`"));

        let missing_options = parse_planning_question_json_payload(
            r#"{"question":"What?","options":[],"context":"Needed"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(missing_options.contains("non-empty entry in `options`"));

        let missing_context =
            parse_planning_question_json_payload(r#"{"question":"What?","options":["CLI"]}"#)
                .unwrap_err()
                .to_string();
        assert!(missing_context.contains("non-empty `context`"));
    }

    #[test]
    fn validates_planning_question_payload_shape() {
        let error = validate_agent_event(
            "planning-question",
            r#"{"question":"What?","options":"CLI","context":"Needed"}"#,
            None,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("planning-question JSON payload is invalid"));
        assert!(rendered.contains("expected a sequence"));
    }
}
