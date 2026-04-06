use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

const RALPH_ENV_PROJECT_DIR: &str = "{ralph-env:PROJECT_DIR}";
const RALPH_REQUEST: &str = "{ralph-request}";

pub(crate) fn interpolate_workflow_prompt(
    prompt_text: &str,
    project_dir: &Utf8Path,
    request: Option<&str>,
) -> Result<String> {
    let replacements = [(RALPH_ENV_PROJECT_DIR, absolute_unix_path(project_dir)?)];

    let mut interpolated = prompt_text.to_owned();
    for (needle, value) in replacements {
        interpolated = interpolated.replace(needle, &value);
    }
    if let Some(request) = request {
        interpolated = interpolated.replace(RALPH_REQUEST, request);
    }
    Ok(interpolated)
}

#[cfg(test)]
mod tests {
    use super::interpolate_workflow_prompt;
    use camino::Utf8Path;

    #[test]
    fn workflow_prompt_interpolates_only_project_dir_and_request() {
        let rendered = interpolate_workflow_prompt(
            "project={ralph-env:PROJECT_DIR}\nrequest={ralph-request}",
            Utf8Path::new("/tmp/project"),
            Some("ship it"),
        )
        .unwrap();

        assert!(rendered.contains("project=/tmp/project"));
        assert!(rendered.contains("request=ship it"));
    }
}

fn absolute_unix_path(path: &Utf8Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd =
            Utf8PathBuf::from_path_buf(std::env::current_dir().context("failed to read cwd")?)
                .map_err(|_| anyhow!("current directory is not valid UTF-8"))?;
        cwd.join(path)
    };
    Ok(absolute.as_str().replace('\\', "/"))
}
