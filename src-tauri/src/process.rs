//! Bounded child IO and process-group lifetime, shared by discovery and adapters.
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub fn home_directory() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

pub fn directories() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    if let Some(home) = home_directory() {
        for path in [
            ".local/bin",
            ".cargo/bin",
            ".bun/bin",
            ".volta/bin",
            ".qoder/entry",
            ".opencode/bin",
            "bin",
        ] {
            dirs.push(home.join(path));
        }
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut bins: Vec<_> = entries.flatten().map(|e| e.path().join("bin")).collect();
            bins.sort();
            bins.reverse();
            dirs.extend(bins);
        }
        #[cfg(windows)]
        {
            dirs.push(home.join("AppData/Roaming/npm"));
        }
    }
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    dirs
}
pub fn executable(provider: &str) -> Option<PathBuf> {
    let names: &[&str] = match provider {
        "codex" => &["codex"],
        "claude" => &["claude"],
        "qoder" => &["qodercli", "qoder"],
        "opencode" => &["opencode"],
        _ => return None,
    };
    for name in names {
        for dir in directories() {
            for suffix in if cfg!(windows) {
                &[".exe", ".cmd", ".bat", ""][..]
            } else {
                &[""][..]
            } {
                let path = dir.join(format!("{name}{suffix}"));
                if path.is_file() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if path.metadata().ok()?.permissions().mode() & 0o111 == 0 {
                            continue;
                        }
                    }
                    return Some(path);
                }
            }
        }
    }
    None
}
pub fn command(provider: &str) -> Result<Command, String> {
    let binary = executable(provider).ok_or("CLI not found. Install it and refresh providers.")?;
    // Batch shims cannot safely receive arbitrary prompt arguments; native executables only.
    #[cfg(windows)]
    if binary.extension().is_some_and(|v| v == "cmd" || v == "bat") {
        return Err("This CLI uses a Windows batch shim; a native executable is required".into());
    }
    let mut command = Command::new(binary);
    if let Ok(path) = std::env::join_paths(directories()) {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}
pub struct Process {
    pid: Option<u32>,
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub lines: Lines<BufReader<tokio::io::Take<ChildStdout>>>,
    stderr: tokio::task::JoinHandle<()>,
}
impl Process {
    pub fn spawn(command: &mut Command) -> Result<Self, String> {
        let mut child = command.spawn().map_err(|_| "CLI could not start")?;
        let input = child.stdin.take().ok_or("CLI stdin unavailable")?;
        let output = child.stdout.take().ok_or("CLI stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("CLI stderr unavailable")?;
        let stderr = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr.take(1024 * 1024), &mut tokio::io::sink()).await;
        });
        Ok(Self {
            pid: child.id(),
            child,
            input: Some(input),
            lines: BufReader::new(output.take(4 * 1024 * 1024)).lines(),
            stderr,
        })
    }
    pub async fn send(&mut self, value: &serde_json::Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|_| "Cannot encode CLI input")?;
        bytes.push(b'\n');
        self.input
            .as_mut()
            .ok_or("CLI stdin closed")?
            .write_all(&bytes)
            .await
            .map_err(|_| "Cannot write CLI input".into())
    }
}
impl Process {
    fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        let _ = self.child.start_kill();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill_tree();
        self.stderr.abort();
    }
}
pub async fn capture(provider: &str, args: &[&str]) -> Result<(bool, String), String> {
    let dir = tempfile::tempdir().map_err(|_| "Cannot create isolated directory")?;
    let mut command = command(provider)?;
    command.args(args).current_dir(dir.path());
    let mut process = Process::spawn(&mut command)?;
    drop(process.input.take());
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut text = String::new();
        while let Some(line) = process
            .lines
            .next_line()
            .await
            .map_err(|_| "Invalid CLI output")?
        {
            if text.len() + line.len() > 128 * 1024 {
                return Err("CLI output limit exceeded".into());
            }
            text.push_str(&line);
            text.push('\n');
        }
        let status = process
            .child
            .wait()
            .await
            .map_err(|_| "Cannot wait for CLI")?;
        Ok((status.success(), text))
    })
    .await
    .map_err(|_| "CLI check timed out".to_string())?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closing_stdin_releases_headless_cli() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "cat >/dev/null; printf 'completed\\n'"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut process = Process::spawn(&mut command).unwrap();
        process
            .input
            .as_mut()
            .unwrap()
            .write_all(b"prompt")
            .await
            .unwrap();
        drop(process.input.take());
        let line = tokio::time::timeout(Duration::from_secs(1), process.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(line, "completed");
    }
    #[tokio::test]
    async fn split_utf8_and_child_group_cleanup() {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf '\\341\\200'; sleep 0.02; printf '\\231\\n'; sleep 60 & wait",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut process = Process::spawn(&mut command).unwrap();
        let pid = process.pid.unwrap();
        assert_eq!(process.lines.next_line().await.unwrap().unwrap(), "မ");
        process.kill_tree();
        process.child.wait().await.unwrap();
        drop(process);
        for _ in 0..40 {
            if unsafe { libc::kill(-(pid as i32), 0) } != 0 {
                return;
            }
            // Killed descendants may remain zombies until the OS reaps them.
            let snapshot = std::process::Command::new("ps")
                .args(["-axo", "pgid=,stat="])
                .output()
                .unwrap();
            let output = String::from_utf8_lossy(&snapshot.stdout);
            let running = output.lines().any(|line| {
                let mut columns = line.split_whitespace();
                columns.next().and_then(|v| v.parse::<u32>().ok()) == Some(pid)
                    && columns.next().is_some_and(|state| !state.starts_with('Z'))
            });
            if !running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("Process group survived cancellation");
    }
}
