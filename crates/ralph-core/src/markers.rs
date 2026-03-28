use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningMarker {
    Done,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderMarker {
    Done,
    Continue,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MarkerError {
    #[error("missing exact completion marker")]
    MissingMarker,
    #[error("multiple exact markers are not allowed")]
    DuplicateMarkers,
    #[error("malformed marker-like line: {0}")]
    MalformedMarker(String),
}

pub fn parse_planning_marker_from_output(output: &str) -> Result<PlanningMarker, MarkerError> {
    let exact = [
        "<plan-promise>DONE</plan-promise>",
        "<plan-promise>CONTINUE</plan-promise>",
    ];
    match parse_exact_marker(output, &exact, "<plan-promise>")? {
        "<plan-promise>DONE</plan-promise>" => Ok(PlanningMarker::Done),
        "<plan-promise>CONTINUE</plan-promise>" => Ok(PlanningMarker::Continue),
        _ => unreachable!(),
    }
}

pub fn parse_builder_marker_from_output(output: &str) -> Result<BuilderMarker, MarkerError> {
    let exact = ["<promise>DONE</promise>", "<promise>CONTINUE</promise>"];
    match parse_exact_marker(output, &exact, "<promise>")? {
        "<promise>DONE</promise>" => Ok(BuilderMarker::Done),
        "<promise>CONTINUE</promise>" => Ok(BuilderMarker::Continue),
        _ => unreachable!(),
    }
}

pub fn strip_persisted_promise_markers(contents: &str) -> String {
    let mut lines = contents
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed != "<promise>DONE</promise>" && trimmed != "<promise>CONTINUE</promise>"
        })
        .collect::<Vec<_>>();

    while matches!(lines.last(), Some(last) if last.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

pub fn append_persisted_done_marker(contents: &str) -> String {
    let stripped = strip_persisted_promise_markers(contents);
    if stripped.is_empty() {
        "<promise>DONE</promise>\n".to_owned()
    } else {
        format!("{stripped}<promise>DONE</promise>\n")
    }
}

fn parse_exact_marker<'a>(
    output: &'a str,
    accepted: &[&'a str],
    marker_prefix: &str,
) -> Result<&'a str, MarkerError> {
    let mut exact = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if accepted.contains(&trimmed) {
            exact.push(trimmed);
            continue;
        }
        if trimmed.contains(marker_prefix) {
            return Err(MarkerError::MalformedMarker(trimmed.to_owned()));
        }
    }

    match exact.len() {
        0 => Err(MarkerError::MissingMarker),
        1 => Ok(exact[0]),
        _ => Err(MarkerError::DuplicateMarkers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_builder_marker() {
        let output = "work\n<promise>CONTINUE</promise>\n";
        assert_eq!(
            parse_builder_marker_from_output(output).unwrap(),
            BuilderMarker::Continue
        );
    }

    #[test]
    fn rejects_inline_builder_marker() {
        let output = "done <promise>DONE</promise>";
        assert!(matches!(
            parse_builder_marker_from_output(output),
            Err(MarkerError::MalformedMarker(_))
        ));
    }

    #[test]
    fn strips_all_persisted_markers() {
        let contents = "Task 1\n<promise>CONTINUE</promise>\n\n<promise>DONE</promise>\n";
        assert_eq!(strip_persisted_promise_markers(contents), "Task 1\n");
    }

    #[test]
    fn appends_final_done_marker() {
        let contents = "Task 1\nTask 2\n";
        assert_eq!(
            append_persisted_done_marker(contents),
            "Task 1\nTask 2\n<promise>DONE</promise>\n"
        );
    }
}
