//! Screencast output selection.
//!
//! When a session starts and more than one output is connected, we have to pick
//! which one to share. The order is:
//!
//! 1. **Pin** — `[portal] screencast_output` in the config (or the
//!    `$SHOESTRING_SCREENCAST_OUTPUT` env override). Deterministic, no UI.
//! 2. **Single output** — nothing to choose.
//! 3. **Chooser** — `[portal] screencast_chooser` (or
//!    `$SHOESTRING_SCREENCAST_CHOOSER`): `"region"` (default) pops the
//!    `shoestring-region` overlay and the user clicks the monitor to share;
//!    `"none"` shares the first output; anything else is run as a dmenu-style
//!    command (connector names on stdin, the chosen line on stdout).
//!
//! Only an explicit user cancel (Escape in the region overlay, or a non-zero
//! exit / empty selection from a command chooser) aborts the share; any other
//! anomaly (chooser binary missing, unknown output picked) degrades to the
//! first output with a warning so screen sharing still works.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use shoestring_config::Portal;

use crate::capture::OutputInfo;

/// The outcome of choosing an output for a screencast session.
pub enum Choice {
    /// Share this output (by connector name).
    Output(String),
    /// The user explicitly cancelled — abort the share.
    Cancelled,
}

/// A `shoestring-region` selection: the output the drag started on plus the
/// rectangle, in that output's logical coordinates.
pub struct RegionSel {
    pub output: String,
    pub x: i32,
    pub y: i32,
    #[allow(dead_code)]
    pub w: i32,
    #[allow(dead_code)]
    pub h: i32,
}

/// Pick the output to share for this session. `outputs` is the live set of
/// connected outputs (from [`crate::capture::Capture::outputs`]).
pub fn choose_output(cfg: &Portal, outputs: &[OutputInfo]) -> Choice {
    let names: Vec<&str> = outputs.iter().map(|o| o.name.as_str()).collect();

    // 1. Pin: env overrides config.
    let pin =
        env_nonempty("SHOESTRING_SCREENCAST_OUTPUT").or_else(|| cfg.screencast_output.clone());
    if let Some(name) = pin {
        if outputs.iter().any(|o| o.name == name) {
            tracing::info!(output = %name, "screencast: using pinned output");
            return Choice::Output(name);
        }
        tracing::warn!(pinned = %name, available = ?names,
            "pinned screencast output not connected; falling back to chooser");
    }

    // 2. Nothing to choose.
    match outputs.len() {
        0 => return Choice::Cancelled,
        1 => return Choice::Output(outputs[0].name.clone()),
        _ => {}
    }

    // 3. Run the configured chooser.
    let chooser = env_nonempty("SHOESTRING_SCREENCAST_CHOOSER")
        .unwrap_or_else(|| cfg.screencast_chooser.clone());
    match chooser.trim() {
        "none" => fallback_first(
            outputs,
            "multiple outputs and screencast_chooser=\"none\": sharing the first. \
             Pin one with [portal] screencast_output, or set [portal] screencast_chooser.",
        ),
        "region" => match run_region() {
            Ok(Some(sel)) if outputs.iter().any(|o| o.name == sel.output) => {
                tracing::info!(output = %sel.output, "screencast: output chosen via region");
                Choice::Output(sel.output)
            }
            Ok(Some(sel)) => fallback_first(
                outputs,
                &format!("region chooser picked unknown output {:?}", sel.output),
            ),
            Ok(None) => {
                tracing::info!("screencast: output choice cancelled (region)");
                Choice::Cancelled
            }
            Err(e) => fallback_first(outputs, &format!("region chooser failed: {e:#}")),
        },
        cmd => match run_command_chooser(cmd, &names) {
            Ok(Some(name)) if outputs.iter().any(|o| o.name == name) => {
                tracing::info!(output = %name, "screencast: output chosen via command");
                Choice::Output(name)
            }
            Ok(Some(name)) => {
                fallback_first(outputs, &format!("chooser picked unknown output {name:?}"))
            }
            Ok(None) => {
                tracing::info!("screencast: output choice cancelled (command chooser)");
                Choice::Cancelled
            }
            Err(e) => fallback_first(outputs, &format!("chooser command {cmd:?} failed: {e:#}")),
        },
    }
}

/// Share the first output, logging `why` (a misconfiguration or chooser failure
/// — never an explicit user cancel).
fn fallback_first(outputs: &[OutputInfo], why: &str) -> Choice {
    let first = outputs[0].name.clone();
    tracing::warn!(sharing = %first, "{why}");
    Choice::Output(first)
}

/// Run `shoestring-region` and parse its selection. `Ok(None)` ⇒ the user
/// cancelled (Escape, non-zero exit). The binary is `$SHOESTRING_REGION_BIN`
/// or `shoestring-region` on `$PATH`.
pub fn run_region() -> Result<Option<RegionSel>> {
    let bin = std::env::var("SHOESTRING_REGION_BIN").unwrap_or_else(|_| "shoestring-region".into());
    let out = Command::new(&bin)
        .output()
        .with_context(|| format!("spawn {bin}"))?;
    if !out.status.success() {
        return Ok(None); // Escape / cancel.
    }
    let line = String::from_utf8_lossy(&out.stdout);
    parse_region_line(line.trim()).map(Some)
}

/// Parse one `shoestring-region` output line: `name x y w h`.
fn parse_region_line(line: &str) -> Result<RegionSel> {
    let mut it = line.split_whitespace();
    let output = it
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("empty region selection"))?
        .to_string();
    let mut int = |what: &str| -> Result<i32> {
        it.next()
            .ok_or_else(|| anyhow!("region line missing {what}"))?
            .parse()
            .with_context(|| format!("region {what}"))
    };
    Ok(RegionSel {
        output,
        x: int("x")?,
        y: int("y")?,
        w: int("w")?,
        h: int("h")?,
    })
}

/// Run a dmenu-style chooser: write `names` (one per line) to its stdin and read
/// the selected connector name from the first stdout line. `cmd` is split on
/// whitespace into argv (no shell). `Ok(None)` ⇒ cancelled (non-zero exit or
/// empty selection).
fn run_command_chooser(cmd: &str, names: &[&str]) -> Result<Option<String>> {
    let mut argv = cmd.split_whitespace();
    let prog = argv
        .next()
        .ok_or_else(|| anyhow!("empty chooser command"))?;
    let mut child = Command::new(prog)
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {prog}"))?;
    // The name list is tiny, so writing it all before reading can't deadlock.
    {
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("chooser stdin"))?;
        for name in names {
            writeln!(stdin, "{name}").context("write chooser stdin")?;
        }
    }
    let out = child.wait_with_output().context("await chooser")?;
    if !out.status.success() {
        return Ok(None);
    }
    let sel = String::from_utf8_lossy(&out.stdout);
    // Take the first whitespace token of the first non-empty line.
    let name = sel
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string);
    Ok(name)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_region_line() {
        let s = parse_region_line("DP-2 100 200 640 480").unwrap();
        assert_eq!(s.output, "DP-2");
        assert_eq!((s.x, s.y, s.w, s.h), (100, 200, 640, 480));
    }

    #[test]
    fn region_line_needs_name() {
        assert!(parse_region_line("").is_err());
    }
}
