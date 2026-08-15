//! Renders D2 diagrams with the [`d2` CLI](https://d2lang.com), which the user
//! is expected to install themselves. Zed never downloads it.

use anyhow::{Context as _, Result, bail};
use smol::io::AsyncWriteExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The name the `d2` binary is published under.
const D2_BINARY_NAME: &str = "d2";

/// Locates the `d2` binary to render with: `configured_path` when the user set
/// one, otherwise `d2` from `PATH`. Returns `None` when D2 isn't installed, in
/// which case diagrams are left as ordinary code blocks.
///
/// This touches the filesystem, so callers should cache the result rather than
/// resolve it per render.
pub fn locate(configured_path: Option<&Path>) -> Option<PathBuf> {
    which::which(configured_path.unwrap_or(Path::new(D2_BINARY_NAME))).ok()
}

/// Renders D2 `source` to an SVG document, using the built-in D2 theme
/// `theme_id`.
pub async fn render(
    binary: &Path,
    arguments: &[String],
    source: &str,
    theme_id: u32,
) -> Result<String> {
    // The two trailing `-` arguments make `d2` read the diagram from stdin and
    // write the SVG to stdout.
    let mut child = smol::process::Command::new(binary)
        .args(arguments)
        .arg(format!("--theme={theme_id}"))
        .arg("-")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", binary.display()))?;

    let mut stdin = child.stdin.take().context("D2 stdin was not piped")?;
    stdin.write_all(source.as_bytes()).await?;
    stdin.flush().await?;
    drop(stdin);

    let output = child
        .output()
        .await
        .context("failed while waiting for the D2 renderer")?;

    if !output.status.success() {
        bail!(
            "D2 rendering exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end(),
        );
    }

    String::from_utf8(output.stdout).context("D2 SVG output contains invalid UTF-8")
}

#[cfg(test)]
mod tests {
    #[test]
    fn locates_only_binaries_that_exist() {
        assert!(super::locate(Some(std::path::Path::new("/bin/sh"))).is_some());
        assert!(super::locate(Some(std::path::Path::new("/nonexistent/d2"))).is_none());
    }
}
