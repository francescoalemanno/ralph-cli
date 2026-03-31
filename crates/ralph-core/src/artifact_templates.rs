pub const REQUIRED_SPEC_HEADINGS: [&str; 7] = [
    "Goal",
    "User Requirements And Constraints",
    "Non-Goals",
    "Proposed Design",
    "Acceptance Criteria",
    "Risks",
    "Open Questions",
];

pub fn empty_spec_contents() -> String {
    String::new()
}

pub fn required_spec_format_outline() -> String {
    REQUIRED_SPEC_HEADINGS
        .iter()
        .map(|heading| format!("# {heading}\n..."))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn initial_spec_contents(planning_request: &str) -> String {
    let request = planning_request.trim();
    render_spec_contents(&[
        ("Goal", format!("Initial planning request: {request}")),
        (
            "User Requirements And Constraints",
            format!("Initial request captured before full planning:\n- {request}"),
        ),
        ("Non-Goals", "To be defined during planning.".to_owned()),
        (
            "Proposed Design",
            "To be defined during planning.".to_owned(),
        ),
        (
            "Acceptance Criteria",
            "To be defined during planning.".to_owned(),
        ),
        (
            "Risks",
            "Planning was interrupted before a full spec was produced.".to_owned(),
        ),
        (
            "Open Questions",
            "See the feedback file for clarification history and unresolved questions.".to_owned(),
        ),
    ])
}

pub fn default_progress_contents() -> String {
    String::new()
}

pub fn default_feedback_contents() -> String {
    feedback_file_contents("None.", "None.")
}

#[doc(hidden)]
pub fn sample_spec_contents(suffix: &str) -> String {
    render_spec_contents(&[
        ("Goal", format!("Goal {suffix}")),
        (
            "User Requirements And Constraints",
            format!("Requirements {suffix}"),
        ),
        ("Non-Goals", format!("Non-goals {suffix}")),
        ("Proposed Design", format!("Design {suffix}")),
        ("Acceptance Criteria", format!("Acceptance {suffix}")),
        ("Risks", format!("Risks {suffix}")),
        ("Open Questions", format!("Questions {suffix}")),
    ])
}

fn feedback_file_contents(recent: &str, older: &str) -> String {
    format!(
        "<RECENT-USER-FEEDBACK>\n{recent}\n</RECENT-USER-FEEDBACK>\n\n<OLDER-USER-FEEDBACK>\n{older}\n</OLDER-USER-FEEDBACK>\n"
    )
}

fn render_spec_contents(sections: &[(&str, String)]) -> String {
    sections
        .iter()
        .map(|(heading, body)| format!("# {heading}\n{body}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::{
        REQUIRED_SPEC_HEADINGS, default_feedback_contents, default_progress_contents,
        empty_spec_contents, initial_spec_contents, required_spec_format_outline,
        sample_spec_contents,
    };

    #[test]
    fn initial_spec_contains_required_sections() {
        let contents = initial_spec_contents("Implement feature");
        for heading in REQUIRED_SPEC_HEADINGS {
            assert!(contents.contains(&format!("# {heading}")));
        }
        assert!(!contents.contains("# Implementation Plan"));
    }

    #[test]
    fn required_spec_outline_contains_all_sections() {
        let outline = required_spec_format_outline();
        for heading in REQUIRED_SPEC_HEADINGS {
            assert!(outline.contains(&format!("# {heading}")));
        }
        assert!(!outline.contains("# Implementation Plan"));
    }

    #[test]
    fn default_progress_is_empty() {
        assert!(default_progress_contents().is_empty());
    }

    #[test]
    fn empty_spec_is_empty() {
        assert!(empty_spec_contents().is_empty());
    }

    #[test]
    fn default_feedback_contains_both_sections() {
        let contents = default_feedback_contents();
        assert!(contents.contains("<RECENT-USER-FEEDBACK>"));
        assert!(contents.contains("<OLDER-USER-FEEDBACK>"));
        assert!(contents.contains("None."));
    }

    #[test]
    fn sample_spec_is_complete() {
        let contents = sample_spec_contents("X");
        assert!(contents.contains("Goal X"));
        assert!(!contents.contains("# Implementation Plan"));
    }
}
