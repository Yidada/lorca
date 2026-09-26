//! The CLI's SDK transport: independent pipe pumps prevent startup/input deadlocks.
use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(super) struct Connection {
    child: Option<Child>,
    process_group: Option<u32>,
    writer: Option<mpsc::UnboundedSender<Value>>,
    reader: mpsc::Receiver<anyhow::Result<Value>>,
    pumps: Vec<JoinHandle<()>>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Connection {
    pub(super) async fn spawn(workdir: &Path, bot: &Bot, session: &ClaudeSession, resume: bool, prompt: &Path) -> anyhow::Result<Self> {
        let executable = std::env::var_os("LORCA_CLAUDE_BIN").unwrap_or_else(|| "claude".into());
        let mut command = lorca_agent::login_shell::command(executable).await;
        command.args(cli_args(bot, session, resume, prompt)?);
        // Keep user-facing CLAUDE_CODE_* settings. Only remove parent-session markers.
        let inherited: Vec<_> = command
            .as_std()
            .get_envs()
            .map(|(key, _)| key.to_os_string())
            .chain(std::env::vars_os().map(|(key, _)| key))
            .filter_map(|key| {
                let key = key.to_string_lossy();
                (key.starts_with("CLAUDECODE_")
                    || matches!(
                        key.as_ref(),
                        "CLAUDECODE"
                            | "CLAUDE_CODE_ENTRYPOINT"
                            | "CLAUDE_CODE_EXECPATH"
                            | "CLAUDE_CODE_SESSION_ID"
                            | "CLAUDE_CODE_SSE_PORT"
                    ))
                .then(|| key.into_owned())
            })
            .collect();
        for key in inherited {
            command.env_remove(key);
        }
        command.env_remove("CLAUDECODE");
        command.env("CLAUDE_CODE_RESUME_INTERRUPTED_TURN", "0");
        // Each Lorca job owns one process tree; native work must finish in that turn.
        command.env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1");
        command
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .context("Cannot start Claude. Install Claude Code and sign in on this Runner, or set LORCA_CLAUDE_BIN")?;
        let stdin = child.stdin.take().context("Claude stdin is unavailable")?;
        let stdout = child.stdout.take().context("Claude stdout is unavailable")?;
        let mut stderr = child.stderr.take().context("Claude stderr is unavailable")?;
        let mut connection = Self::from_io(stdout, stdin);
        let tail = connection.stderr.clone();
        connection.pumps.push(tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(8192);
                tail.drain(..excess);
            }
        }));
        connection.process_group = child.id();
        connection.child = Some(child);
        Ok(connection)
    }

    pub(super) fn from_io(reader: impl AsyncRead + Unpin + Send + 'static, mut writer: impl AsyncWrite + Unpin + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let (events, reader) = {
            let (events, rx) = mpsc::channel(64);
            let target = events.clone();
            let pump = tokio::spawn(async move {
                let mut reader = BufReader::new(reader);
                loop {
                    let frame = read_frame(&mut reader).await;
                    let end = frame.is_err();
                    if target.send(frame).await.is_err() || end {
                        break;
                    }
                }
            });
            ((events, pump), rx)
        };
        let (events, read_pump) = events;
        let write_pump = tokio::spawn(async move {
            while let Some(value) = rx.recv().await {
                let result = async {
                    let mut bytes = serde_json::to_vec(&value)?;
                    bytes.push(b'\n');
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    let _ = events.send(Err(error.context("Writing to Claude"))).await;
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        Self {
            child: None,
            process_group: None,
            writer: Some(tx),
            reader,
            pumps: vec![read_pump, write_pump],
            stderr: Default::default(),
        }
    }

    pub(super) fn send(&self, value: Value) -> anyhow::Result<()> {
        self.writer
            .as_ref()
            .context("Claude input is closed")?
            .send(value)
            .map_err(|_| anyhow::anyhow!("Claude input is closed"))
    }

    pub(super) async fn next(&mut self) -> anyhow::Result<Value> {
        self.reader.recv().await.context("Claude closed its event stream")?
    }

    pub(super) fn diagnostic(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).trim().to_string()
    }

    pub(super) async fn stop(&mut self) {
        let _ = self.send(json!({ "type": "control_request", "request_id": "lorca-interrupt", "request": { "subtype": "interrupt" } }));
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while let Ok(event) = self.next().await {
                if event["type"] == "result" {
                    break;
                }
            }
        })
        .await;
        self.writer.take();
    }

    pub(super) async fn finish(&mut self) {
        self.writer.take();
        if let Some(child) = &mut self.child {
            // Keep the process handle until Drop has cleaned up its owned descendants.
            let _ = tokio::time::timeout(Duration::from_secs(6), async {
                while child.try_wait()?.is_none() {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok::<_, std::io::Error>(())
            })
            .await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            if let Some(pid) = self.process_group {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .output();
                }
            }
            let _ = child.start_kill();
        }
        for pump in &self.pumps {
            pump.abort();
        }
    }
}

async fn read_frame(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> anyhow::Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            bail!("Claude closed its event stream before the turn completed");
        }
        let n = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
        if bytes.len() + n > MAX_FRAME {
            bail!("Claude event exceeded the 16 MiB frame limit");
        }
        bytes.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if bytes.last() != Some(&b'\n') {
            continue;
        }
        let line = std::str::from_utf8(&bytes)?.trim();
        if line.starts_with('{') {
            return serde_json::from_str(line).context("Claude returned malformed JSON");
        }
        // Some CLI versions emit startup diagnostics on stdout.
        bytes.clear();
    }
}

pub(super) fn cli_args(bot: &Bot, session: &ClaudeSession, resume: bool, prompt: &Path) -> anyhow::Result<Vec<String>> {
    uuid::Uuid::parse_str(&session.session_id).context("Stored Claude session ID is invalid")?;
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--permission-mode",
        "default",
        "--permission-prompt-tool",
        "stdio",
        "--mcp-config",
        r#"{"mcpServers":{"lorca":{"type":"sdk","name":"lorca"}}}"#,
        "--append-system-prompt-file",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    args.push(prompt.to_string_lossy().into_owned());
    args.push(format!("--{}={}", if resume { "resume" } else { "session-id" }, session.session_id));
    if let Some(model) = bot.model.as_deref().filter(|m| !m.is_empty()) {
        args.push(format!("--model={model}"));
    }
    if let Some(effort) = bot.thinking.as_deref() {
        if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            bail!("Claude does not support thinking level {effort}; select low, medium, high, xhigh, max, or Default");
        }
        args.push(format!("--effort={effort}"));
    }
    Ok(args)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drop_kills_owned_descendants_even_after_the_leader_exits() {
        for leader_exits in [false, true] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(if leader_exits {
                    "sleep 60 & echo ready; exit 0"
                } else {
                    "sleep 60 & echo ready; wait"
                })
                .stdout(Stdio::piped())
                .process_group(0)
                .kill_on_drop(true);
            let mut child = command.spawn().unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            stdout.read_line(&mut line).await.unwrap();
            assert_eq!(line.trim(), "ready");
            let mut connection = Connection::from_io(tokio::io::empty(), tokio::io::sink());
            connection.process_group = child.id();
            connection.child = Some(child);
            if leader_exits {
                connection.finish().await;
                assert!(connection.child.as_ref().unwrap().id().is_none());
            }
            drop(connection);
            // The sleep inherits stdout. EOF proves no owned descendant still holds it open,
            // even on hosts whose init temporarily leaves a reparented child as a zombie.
            tokio::time::timeout(Duration::from_secs(3), stdout.read_to_end(&mut Vec::new()))
                .await
                .unwrap()
                .unwrap();
        }
    }
}
