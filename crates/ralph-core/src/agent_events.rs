use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

pub const RUNTIME_DIR_NAME: &str = ".ralph-runtime";
pub const AGENT_EVENTS_WAL_FILE_NAME: &str = "agent-events.wal.ndjson";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventRecord {
    pub v: u8,
    pub ts_unix_ms: u64,
    pub run_id: String,
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
pub enum LoopControlDecision {
    Continue,
    StopOk(String),
    StopError(String),
    Route(String),
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
    let wal_path = agent_events_wal_path(run_dir);
    let file = match std::fs::File::open(&wal_path) {
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

pub fn reduce_loop_control(
    records: &[AgentEventRecord],
    current_prompt_name: &str,
) -> Option<LoopControlDecision> {
    let mut decision = None;
    for record in records {
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

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::{
        AGENT_EVENTS_WAL_FILE_NAME, AgentEventRecord, LoopControlDecision, RUNTIME_DIR_NAME,
        agent_events_wal_path, append_agent_event, current_agent_events_offset,
        read_agent_events_since, reduce_loop_control,
    };

    fn sample_record(event: &str, body: &str, run_id: &str) -> AgentEventRecord {
        AgentEventRecord {
            v: 1,
            ts_unix_ms: 42,
            run_id: run_id.to_owned(),
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
}
