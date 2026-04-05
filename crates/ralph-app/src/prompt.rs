use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

const RALPH_ENV_PROJECT_DIR: &str = "{ralph-env:PROJECT_DIR}";
const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";

pub(crate) fn interpolate_prompt_env(
    prompt_text: &str,
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
) -> Result<String> {
    let replacements = [
        (RALPH_ENV_PROJECT_DIR, absolute_unix_path(project_dir)?),
        (RALPH_ENV_TARGET_DIR, absolute_unix_path(target_dir)?),
    ];

    let mut interpolated = prompt_text.to_owned();
    for (needle, value) in replacements {
        interpolated = interpolated.replace(needle, &value);
    }
    Ok(interpolated)
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
