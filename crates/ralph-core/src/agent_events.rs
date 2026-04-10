use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::workflow::load_workflow_from_path;

pub const RUNTIME_DIR_NAME: &str = ".ralph-runtime";
pub const AGENT_EVENTS_WAL_FILE_NAME: &str = "agent-events.wal.ndjson";
pub const MAIN_CHANNEL_ID: &str = "main";
const MARKER_START: &str = "<<<";
const SIGNAL_START: &str = "<<<SIGNAL:";
const PAYLOAD_START: &str = "<<<PAYLOAD:";
const MARKER_END: &str = ">>>";
const PAYLOAD_END: &str = "<<<END-PAYLOAD>>>";

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedAgentOutput {
    pub visible_text: String,
    pub events: Vec<ParsedAgentEvent>,
}

#[derive(Debug, Default)]
pub struct AgentOutputProcessor {
    buffer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopControlDecision {
    Continue,
    StopOk(String),
    StopError(String),
    Route(String),
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
    let wal_path = agent_events_wal_path(run_dir);
    if let Some(parent) = wal_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&wal_path)
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

    if !trimmed_event.starts_with("loop-") {
        return Ok(());
    }

    match trimmed_event {
        "loop-continue" | "loop-stop:ok" | "loop-stop:error" => Ok(()),
        "loop-route" => validate_loop_route_body(body, prompt_path),
        _ => Err(anyhow!(unsupported_loop_event_message(trimmed_event))),
    }
}

impl AgentOutputProcessor {
    pub fn push_str(&mut self, chunk: &str) -> ParsedAgentOutput {
        self.buffer.push_str(chunk);
        self.drain(false)
    }

    pub fn finish(&mut self) -> ParsedAgentOutput {
        self.drain(true)
    }

    fn drain(&mut self, eof: bool) -> ParsedAgentOutput {
        let buffer = std::mem::take(&mut self.buffer);
        let mut visible_text = String::new();
        let mut events = Vec::new();
        let mut cursor = 0;

        while cursor < buffer.len() {
            let remaining = &buffer[cursor..];
            let Some(start) = remaining.find(MARKER_START) else {
                let flush_len = if eof {
                    remaining.len()
                } else {
                    remaining
                        .len()
                        .saturating_sub(partial_marker_prefix_len(remaining))
                };
                visible_text.push_str(&remaining[..flush_len]);
                cursor += flush_len;
                break;
            };

            visible_text.push_str(&remaining[..start]);
            cursor += start;

            let remaining = &buffer[cursor..];
            if let Some(signal_body) = remaining.strip_prefix(SIGNAL_START) {
                if let Some(end) = signal_body.find(MARKER_END) {
                    let name = &signal_body[..end];
                    events.push(ParsedAgentEvent {
                        event: name.trim().to_owned(),
                        body: String::new(),
                    });
                    cursor += SIGNAL_START.len() + end + MARKER_END.len();
                    continue;
                }

                if eof {
                    visible_text.push_str(remaining);
                    cursor = buffer.len();
                }
                break;
            }

            if let Some(payload_body) = remaining.strip_prefix(PAYLOAD_START) {
                if let Some(header_end) = payload_body.find(MARKER_END) {
                    let name = &payload_body[..header_end];
                    let body_start = PAYLOAD_START.len() + header_end + MARKER_END.len();
                    if let Some(body_end) = remaining[body_start..].find(PAYLOAD_END) {
                        let body = &remaining[body_start..body_start + body_end];
                        events.push(ParsedAgentEvent {
                            event: name.trim().to_owned(),
                            body: body.to_owned(),
                        });
                        cursor += body_start + body_end + PAYLOAD_END.len();
                        continue;
                    }
                }

                if eof {
                    visible_text.push_str(remaining);
                    cursor = buffer.len();
                }
                break;
            }

            if !eof && (SIGNAL_START.starts_with(remaining) || PAYLOAD_START.starts_with(remaining))
            {
                break;
            }

            visible_text.push('<');
            cursor += 1;
        }

        self.buffer.push_str(&buffer[cursor..]);

        ParsedAgentOutput {
            visible_text,
            events,
        }
    }
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

fn partial_marker_prefix_len(text: &str) -> usize {
    if text.ends_with("<<") {
        2
    } else if text.ends_with('<') {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::{
        AGENT_EVENTS_WAL_FILE_NAME, AgentEventRecord, AgentOutputProcessor, LoopControlDecision,
        MAIN_CHANNEL_ID, RUNTIME_DIR_NAME, agent_events_wal_path, append_agent_event,
        current_agent_events_offset, latest_agent_event_body_from_wal,
        latest_agent_event_body_from_wal_in_channel, read_agent_events_since, reduce_loop_control,
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
    fn processor_extracts_signal_and_payload_markers() {
        let mut processor = AgentOutputProcessor::default();
        let parsed = processor.push_str(
            "alpha\n<<<SIGNAL:loop-continue>>>\n<<<PAYLOAD:test-phase>>>beta<<<END-PAYLOAD>>>\nomega\n",
        );

        assert_eq!(parsed.visible_text, "alpha\n\n\nomega\n");
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].event, "loop-continue");
        assert_eq!(parsed.events[0].body, "");
        assert_eq!(parsed.events[1].event, "test-phase");
        assert_eq!(parsed.events[1].body, "beta");
    }

    #[test]
    fn processor_handles_partial_markers_across_chunks() {
        let mut processor = AgentOutputProcessor::default();

        let first = processor.push_str("before<<<PAYL");
        assert_eq!(first.visible_text, "before");
        assert!(first.events.is_empty());

        let second = processor.push_str("OAD:test>>>body");
        assert_eq!(second.visible_text, "");
        assert!(second.events.is_empty());

        let third = processor.push_str("<<<END-PAYLOAD>>>after");
        assert_eq!(third.visible_text, "after");
        assert_eq!(third.events.len(), 1);
        assert_eq!(third.events[0].event, "test");
        assert_eq!(third.events[0].body, "body");
    }

    #[test]
    fn processor_flushes_incomplete_marker_as_text_on_finish() {
        let mut processor = AgentOutputProcessor::default();
        let parsed = processor.push_str("hello<<<SIGNAL:loop-stop:ok");
        assert_eq!(parsed.visible_text, "hello");

        let finished = processor.finish();
        assert_eq!(finished.visible_text, "<<<SIGNAL:loop-stop:ok");
        assert!(finished.events.is_empty());
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
    is_interactive: false
    fallback-route: no-route-error
    prompt: hello
  beta:
    title: Beta
    is_interactive: false
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
}
