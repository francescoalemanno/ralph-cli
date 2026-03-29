use crate::ClarificationRequest;

pub fn parse_clarification_request(output: &str) -> Option<ClarificationRequest> {
    let start_tag = "<ralph-question>";
    let end_tag = "</ralph-question>";
    let mut search_from = 0;

    'scan: while let Some(start) = output[search_from..].find(start_tag) {
        let start = search_from + start;
        let payload_start = start + start_tag.len();
        let candidate_from = payload_start;

        loop {
            let next_start = output[candidate_from..]
                .find(start_tag)
                .map(|offset| candidate_from + offset);
            let next_end = output[candidate_from..]
                .find(end_tag)
                .map(|offset| candidate_from + offset);

            match (next_start, next_end) {
                (Some(nested_start), Some(end)) if nested_start < end => {
                    search_from = nested_start;
                    continue 'scan;
                }
                (_, Some(end)) => {
                    let payload = output[payload_start..end].trim();
                    if let Ok(request) = serde_json::from_str::<ClarificationRequest>(payload) {
                        if !request.question.trim().is_empty() {
                            return Some(request);
                        }
                    }

                    search_from = end + end_tag.len();
                    continue 'scan;
                }
                (Some(nested_start), None) => {
                    search_from = nested_start;
                    continue 'scan;
                }
                (None, None) => return None,
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::parse_clarification_request;

    #[test]
    fn accepts_any_number_of_options() {
        let output = r#"<ralph-question>
{"question":"Which target?","options":[
    {"label":"one","description":"a"},
    {"label":"two","description":"b"},
    {"label":"three","description":"c"},
    {"label":"four","description":"d"},
    {"label":"five","description":"e"}
]}
</ralph-question>"#;

        let request = parse_clarification_request(output).unwrap();
        assert_eq!(request.options.len(), 5);
    }

    #[test]
    fn ignores_malformed_question_blocks() {
        assert!(parse_clarification_request("<ralph-question>{oops}</ralph-question>").is_none());
        assert!(parse_clarification_request("<ralph-question></ralph-question>").is_none());
        assert!(
            parse_clarification_request("<ralph-question>{\"options\":[]}</ralph-question>")
                .is_none()
        );
    }

    #[test]
    fn returns_first_valid_question_block() {
        let output = concat!(
            "<ralph-question>{oops}</ralph-question>\n",
            "<ralph-question>{\"question\":\"Unclosed\"",
            "<ralph-question>{\"question\":\"Recovered target\",\"options\":[]}</ralph-question>\n",
            "<ralph-question>{\"question\":\"Ignored\",\"options\":[]}</ralph-question>\n",
            "<ralph-question>{\"question\":\"Also ignored\",\"options\":[]}</ralph-question>\n"
        );

        let request = parse_clarification_request(output).unwrap();
        assert_eq!(request.question, "Recovered target");
    }

    #[test]
    fn accepts_tagged_question_embedded_in_surrounding_text() {
        let output = concat!(
            "thinking...XYZ",
            "<ralph-question>{\"question\":\"Which target?\",\"options\":[]}</ralph-question>",
            "WDX trailing notes"
        );

        let request = parse_clarification_request(output).unwrap();
        assert_eq!(request.question, "Which target?");
    }

    #[test]
    fn recovers_from_earlier_unclosed_block_before_valid_one() {
        let output = concat!(
            "<ralph-question>{\"question\":\"Broken\"",
            "\nintermediate reasoning\n",
            "<ralph-question>{\"question\":\"Actual question\",\"options\":[]}</ralph-question>\n",
            "<ralph-question>{\"question\":\"Ignored\",\"options\":[]}</ralph-question>\n"
        );

        let request = parse_clarification_request(output).unwrap();
        assert_eq!(request.question, "Actual question");
    }
}
