use std::{
    collections::VecDeque,
    ffi::OsStr,
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stderr: Vec<u8>,
    pub cancelled: bool,
}

/// Run a program in a separate Unix process group; never invoke a shell implicitly.
pub async fn run_process<I, S>(
    program: &OsStr,
    args: I,
    cancel: &CancellationToken,
    force: &CancellationToken,
) -> io::Result<ProcessOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn()?;
    let pgid = child
        .id()
        .ok_or_else(|| io::Error::other("process has no PID"))? as i32;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    let mut reader = tokio::spawn(async move {
        let mut tail = VecDeque::with_capacity(65536);
        let mut buffer = [0u8; 8192];
        loop {
            let n = stderr.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            for byte in &buffer[..n] {
                if tail.len() == 65536 {
                    tail.pop_front();
                }
                tail.push_back(*byte);
            }
        }
        Ok::<Vec<u8>, io::Error>(tail.into_iter().collect())
    });
    // A child can outlive the group leader and keep stderr open, so cancellation
    // must remain active while the pipe is draining as well as while waiting.
    let status = tokio::select! {
        result = child.wait() => {
            let status = result?;
            tokio::select! {
                result = &mut reader => return Ok(ProcessOutput { status, stderr: result.map_err(io::Error::other)??, cancelled: false }),
                _ = cancel.cancelled() => Some(status),
            }
        },
        _ = cancel.cancelled() => None,
    };
    // SAFETY: negative PID targets only the process group we created.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = force.cancelled() => {} }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().await?,
    };
    let stderr = reader.await.map_err(io::Error::other)??;
    Ok(ProcessOutput {
        status,
        stderr,
        cancelled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    #[tokio::test]
    async fn retains_only_the_stderr_tail() -> io::Result<()> {
        let output = run_process(
            OsStr::new("/bin/sh"),
            [
                "-c",
                "head -c 70000 /dev/zero | tr '\\0' x >&2; printf END >&2",
            ],
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await?;
        assert!(output.status.success());
        assert_eq!(output.stderr.len(), 65536);
        assert!(output.stderr.ends_with(b"END"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_kills_process_group_and_reaps_leader() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("encoder.sh");
        let pids = dir.path().join("pids");
        fs::write(&script, "#!/bin/sh\nsleep 60 &\nc=$!\necho $$ $c > \"$1\"\ntrap 'kill $c 2>/dev/null; wait $c 2>/dev/null; exit 0' TERM\nwait $c\n")?;
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let script_arg = script.clone();
        let pids_arg = pids.clone();
        let task = tokio::spawn(async move {
            run_process(
                OsStr::new("/bin/sh"),
                [script_arg.as_os_str(), pids_arg.as_os_str()],
                &child_cancel,
                &CancellationToken::new(),
            )
            .await
        });
        for _ in 0..100 {
            if Path::new(&pids).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pids: Vec<i32> = fs::read_to_string(pids)?
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);
        cancel.cancel();
        let output = task.await.map_err(io::Error::other)??;
        assert!(output.cancelled);
        for pid in pids {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "process {pid} survived"
            );
        }
        Ok(())
    }
}
