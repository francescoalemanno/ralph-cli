use crate::ClarificationRequest;

pub fn parse_clarification_request(output: &str) -> Option<ClarificationRequest> {
    let start_tag = "<ralph-question>";
    let end_tag = "</ralph-question>";

    let Some(start) = output.find(start_tag) else {
        return None;
    };
    let Some(end) = output[start + start_tag.len()..].find(end_tag) else {
        return None;
    };

    let json_start = start + start_tag.len();
    let json_end = json_start + end;
    let payload = output[json_start..json_end].trim();
    if payload.is_empty() {
        return None;
    }

    let request: ClarificationRequest = serde_json::from_str(payload).ok()?;

    if request.question.trim().is_empty() {
        return None;
    }
    Some(request)
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
}
