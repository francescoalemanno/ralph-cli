use std::time::{SystemTime, UNIX_EPOCH};

pub const HOST_CHANNEL_ID: &str = "host";
pub const PLANNING_QUESTION_EVENT: &str = "planning-question";
pub const PLANNING_ANSWER_EVENT: &str = "planning-answer";
pub const PLANNING_REVIEW_EVENT: &str = "planning-review";
pub const PLANNING_PROGRESS_EVENT: &str = "planning-progress";
pub const PLANNING_PLAN_FILE_EVENT: &str = "planning-plan-file";
pub const PLANNING_TARGET_PATH_EVENT: &str = "planning-target-path";

pub fn current_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub fn format_timeout_duration(total_seconds: u64) -> String {
    if total_seconds.is_multiple_of(3600) {
        return format!("{}h", total_seconds / 3600);
    }
    if total_seconds.is_multiple_of(60) {
        return format!("{}m", total_seconds / 60);
    }
    format!("{}s", total_seconds)
}

#[cfg(test)]
mod tests {
    use super::format_timeout_duration;

    #[test]
    fn timeout_duration_prefers_larger_units() {
        assert_eq!(format_timeout_duration(3600), "1h");
        assert_eq!(format_timeout_duration(600), "10m");
        assert_eq!(format_timeout_duration(59), "59s");
    }
}
