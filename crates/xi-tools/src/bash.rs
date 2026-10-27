use serde::Deserialize;
use serde_json::Value;
use xi_protocol::ToolCallId;

use crate::{Context, Ran, parse};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub(crate) command: String,
    #[serde(default = "timeout")]
    timeout: u64,
}
fn timeout() -> u64 {
    120
}
pub(crate) fn input(input: &Value) -> Result<Input, String> {
    let input: Input = parse(input)?;
    if !(1..=600).contains(&input.timeout) {
        return Err("timeout must be 1..=600 seconds".into());
    }
    if input.command.contains('\0') {
        return Err("command must not contain NUL".into());
    }
    Ok(input)
}

#[cfg(unix)]
struct ProcessGroup {
    pid: nix::unistd::Pid,
    reaped: bool,
    armed: bool,
}
#[cfg(unix)]
impl ProcessGroup {
    fn kill(&self) {
        use nix::sys::signal::{Signal, kill, killpg};
        // The helper creates a session before executing any user command.
        // A cancellation before setsid must also kill the helper itself.
        let _ = killpg(self.pid, Signal::SIGKILL);
        if !self.reaped {
            let _ = kill(self.pid, Signal::SIGKILL);
        }
        // Close the race where setsid happened between the first two calls.
        let _ = killpg(self.pid, Signal::SIGKILL);
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

#[cfg(unix)]
pub(crate) async fn run(call: &ToolCallId, value: &Value, ctx: &Context) -> Result<Ran, String> {
    use std::os::fd::OwnedFd;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    let input = input(value)?;
    if ctx.cancel.is_cancelled() {
        return Ok(crate::output::text(call, "[cancelled]".into(), true, ctx));
    }
    let shell = if Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut spec = ctx.sandbox.clone();
    spec.cwd.clone_from(&ctx.workspace);
    let launch_error = if spec.enabled {
        "Sandbox could not start"
    } else {
        "Shell could not start"
    };
    let mut command = spec
        .command(&ctx.helper, shell, ["-c", &input.command])
        .map_err(|error| format!("{launch_error}: {error}"))?;
    let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
    command
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(|error| error.to_string())?)
        .stderr(writer);
    // Sandboxed commands need setsid in the helper; direct commands already
    // have their own process group from Spec::command.
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("{launch_error}: {error}"))?;
    drop(command); // Close the parent's write handles so EOF can be observed.
    let mut group = ProcessGroup {
        pid: nix::unistd::Pid::from_raw(child.id().expect("spawned child") as i32),
        reaped: false,
        armed: true,
    };
    let fd: OwnedFd = reader.into();
    let mut reader = tokio::fs::File::from_std(std::fs::File::from(fd));
    let mut capture = crate::output::Capture::new(call, ctx);
    let deadline = tokio::time::sleep(Duration::from_secs(input.timeout));
    tokio::pin!(deadline);
    let mut bytes = [0_u8; 16 * 1024];
    let mut status = None;
    let mut eof = false;
    let mut stopped = None;
    while !eof || status.is_none() {
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled(), if stopped.is_none() => {
                stopped = Some("[cancelled]".to_owned());
                group.kill();
            }
            () = &mut deadline, if stopped.is_none() => {
                stopped = Some(format!("[timed out after {}s]", input.timeout));
                group.kill();
            }
            result = child.wait(), if status.is_none() => {
                status = Some(result.map_err(|error| error.to_string())?);
                group.reaped = true;
            }
            result = reader.read(&mut bytes), if !eof => {
                let length = result.map_err(|error| error.to_string())?;
                if length == 0 { eof = true; }
                else { capture.push(&bytes[..length]).map_err(|error| format!("Cannot spill shell output: {error}"))?; }
            }
        }
    }
    let status = status.expect("child was waited");
    let is_error = stopped.is_some() || !status.success();
    let footer = stopped.unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        if ctx.sandbox.enabled && status.code() == Some(xi_sandbox::EXIT_SETUP_FAILED) {
            "[Sandbox could not start: helper exited 125]\n[exit 125]".to_owned()
        } else if let Some(code) = status.code() {
            format!("[exit {code}]")
        } else {
            format!(
                "[exit {}: signal {}]",
                128 + status.signal().unwrap_or(0),
                status.signal().unwrap_or(0)
            )
        }
    });
    capture
        .push(format!("\n{footer}").as_bytes())
        .map_err(|error| format!("Cannot spill shell output: {error}"))?;
    group.armed = false;
    Ok(capture.finish(is_error))
}

#[cfg(not(unix))]
pub(crate) async fn run(_call: &ToolCallId, _value: &Value, _ctx: &Context) -> Result<Ran, String> {
    Err("Sandbox could not start: shell tools require a Unix platform".into())
}
