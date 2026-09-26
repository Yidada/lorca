//! Claude Code owns inference, native tools, permissions, and durable session history.
//! Protocol reference: anthropics/claude-agent-sdk-python's subprocess transport/query.
use super::*;
use anyhow::{bail, Context};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinSet;

use crate::local_store::ClaudeSession;
mod output;
mod transport;
use output::Output;
use transport::Connection;

pub(super) async fn run(
    app: &Arc<App>,
    job: &Job,
    bot: &Bot,
    routine: Option<&Routine>,
    trigger: &Trigger,
    cancel: CancellationToken,
) -> TurnOutcome {
    let Some(chat) = app.chat(&job.chat_id) else {
        return TurnOutcome::Skipped;
    };
    let mut output = Output::new(app, job, bot);
    let result = async {
        let workdir = bot.working_directory(&app.config.home);
        std::fs::create_dir_all(&workdir)?;
        let workdir = std::fs::canonicalize(workdir)?;
        let stored = app.store.claude_session(&chat.meta.id, &bot.id, &workdir.to_string_lossy())?;
        let session = stored.clone().unwrap_or_else(|| ClaudeSession { session_id: uuid::Uuid::new_v4().to_string(), after_message_id: None });
        let (tools, plugins, instructions) = native::context(app, job, bot, &chat, routine, trigger);
        let prompt = PromptFile::new(&app.config.home, &instructions)?;
        let mut connection = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            connection = Connection::spawn(&workdir, bot, &session, stored.is_some(), &prompt.0) => connection?,
        };
        let bridge = Arc::new(Bridge { app: app.clone(), bot: bot.clone(), chat: chat.clone(), tools, plugins, unattended: routine.is_some() });
        let result = tokio::select! {
            _ = cancel.cancelled() => Ok(()),
            result = run_connected(&mut connection, app, job, bot, &chat, routine, &workdir, session, stored.is_none(), bridge, &cancel, &mut output) => result,
        };
        if cancel.is_cancelled() {
            native::close_permissions(app, &chat.meta.id);
            connection.stop().await;
        }
        let diagnostic = connection.diagnostic();
        connection.finish().await;
        result.map_err(|error| if diagnostic.is_empty() { error } else { error.context(format!("Claude: {diagnostic}")) })
    }.await;
    native::close_permissions(app, &chat.meta.id);
    output.finish();
    if let Err(error) = result {
        output.fail(&format!("{error:#}"));
    }
    if let Some(error) = &output.error {
        crate::push::failed(app, &chat, bot, error);
    } else if !cancel.is_cancelled() {
        if let Some(text) = &output.last_text {
            crate::push::reply(app, &chat, bot, text);
        }
    }
    if let Some(line) = turn_log_line(output.last_text.as_deref(), &output.tools_used, output.error.is_some()) {
        let _ =
            MemoryStore::for_bot(&app.config.home, bot).append_log(&line, Some(&format!("in {}", chat_source(&chat))), now_secs() as i64);
    }
    if output.error.is_some() {
        TurnOutcome::Skipped
    } else if output.last_text.is_some() {
        TurnOutcome::Sent
    } else {
        TurnOutcome::Pass
    }
}

/// The file avoids command-line size limits and is private to this one execution.
struct PromptFile(PathBuf);
impl PromptFile {
    fn new(home: &Path, text: &str) -> anyhow::Result<Self> {
        use std::io::Write;
        let path = home.join(format!(".claude-prompt-{}.txt", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let guard = Self(path);
        file.write_all(text.as_bytes())?;
        Ok(guard)
    }
}
impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connected(
    connection: &mut Connection,
    app: &Arc<App>,
    job: &Job,
    bot: &Bot,
    chat: &Chat,
    routine: Option<&Routine>,
    workdir: &Path,
    mut session: ClaudeSession,
    fresh: bool,
    bridge: Arc<Bridge>,
    cancel: &CancellationToken,
    output: &mut Output,
) -> anyhow::Result<()> {
    let (messages, found) = app
        .store
        .context(&chat.meta.id, session.after_message_id.as_deref(), Some(MAX_CONTEXT_MESSAGES))?;
    if session.after_message_id.is_some() && !found {
        bail!("The message used to resume this Claude session is unavailable. Create a new chat to start with fresh context.");
    }
    let mut content = input_for(app, bot, workdir, &messages, fresh).await;
    if let Some(routine) = routine {
        content.push(json!({ "type": "text", "text": format!("Routine {}: {}", routine.name, routine.prompt) }));
    }
    if job.kind == "room_turn" {
        content.push(json!({ "type": "text", "text": room_turn_cue(app, chat, bot, job) }));
    }
    if let Some(setup) = &job.setup {
        content.push(json!({ "type": "text", "text": setup_cue(app, setup) }));
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "Continue." }));
    }
    let after = messages.last().map(|m| m.id.clone()).or(session.after_message_id.clone());
    let input_id = uuid::Uuid::new_v4().to_string();
    let workdir_key = workdir.to_string_lossy();
    let mut initialized = false;
    let mut accepted = false;
    let mut session_seen = false;
    let mut controls = JoinSet::new();
    let mut requests: HashMap<String, CancellationToken> = HashMap::new();
    let startup = tokio::time::sleep(Duration::from_secs(45));
    tokio::pin!(startup);
    connection.send(json!({ "type": "control_request", "request_id": "lorca-initialize", "request": { "subtype": "initialize" } }))?;
    loop {
        let value = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = &mut startup, if !initialized => bail!("Claude did not initialize within 45 seconds"),
            result = controls.join_next(), if !controls.is_empty() => {
                let (id, response) = result.context("Claude control task disappeared")??;
                if let Some(token) = requests.remove(&id) {
                    if !token.is_cancelled() { connection.send(response)?; }
                }
                continue;
            }
            value = connection.next() => value?,
        };
        match value["type"].as_str().unwrap_or("") {
            "control_request" => {
                let id = value["request_id"]
                    .as_str()
                    .context("Claude control request has no ID")?
                    .to_string();
                if requests.contains_key(&id) || requests.len() >= 32 {
                    bail!("Claude sent duplicate or too many outstanding control requests");
                }
                let token = cancel.child_token();
                requests.insert(id.clone(), token.clone());
                let bridge = bridge.clone();
                let request = value["request"].clone();
                controls.spawn(async move {
                    // Let permission/tool handlers observe cancellation and close their cards.
                    // Dropping their future in a competing select would skip that cleanup.
                    let response = bridge.control(&id, &request, &token).await;
                    (id, response)
                });
            }
            "control_cancel_request" => {
                if let Some(token) = value["request_id"].as_str().and_then(|id| requests.get(id)) {
                    token.cancel();
                }
            }
            "control_response" if value["response"]["request_id"] == "lorca-initialize" => {
                if initialized {
                    bail!("Claude initialized twice");
                }
                if value["response"]["subtype"] != "success" {
                    bail!("Claude initialization failed: {}", value["response"]["error"]);
                }
                initialized = true;
                connection.send(json!({ "type": "user", "uuid": input_id, "session_id": session.session_id, "parent_tool_use_id": null, "message": { "role": "user", "content": content } }))?;
            }
            "system" if value["subtype"] == "init" => {
                let id = value["session_id"].as_str().context("Claude did not return its session ID")?;
                if id != session.session_id {
                    bail!("Claude returned a different session; refusing to join unrelated context");
                }
                session_seen = true;
                app.store.save_claude_session(&chat.meta.id, &bot.id, &workdir_key, &session)?;
            }
            "result" => {
                if value["session_id"].as_str().is_some_and(|id| id != session.session_id) {
                    bail!("Claude result belongs to a different session");
                }
                if value["is_error"] == true
                    || value["subtype"].as_str().is_some_and(|s| s.starts_with("error"))
                    || value["terminal_reason"] == "prompt_too_long"
                {
                    let detail = value["errors"]
                        .as_array()
                        .map(|errors| errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
                        .filter(|s| !s.is_empty())
                        .or_else(|| value["result"].as_str().map(str::to_string))
                        .unwrap_or_else(|| value.to_string());
                    bail!("Claude turn failed: {detail}");
                }
                if !initialized || !session_seen {
                    bail!("Claude completed without initializing a session");
                }
                session.after_message_id = after.clone();
                app.store.save_claude_session(&chat.meta.id, &bot.id, &workdir_key, &session)?;
                if !output.has_text() {
                    if let Some(text) = value["result"].as_str() {
                        output.text("result", text, true);
                    }
                }
                return Ok(());
            }
            _ => {
                // A replay ack or new assistant response confirms this input was accepted.
                // Do not advance on system/init: auth/startup can still fail before delivery.
                let root = value["parent_tool_use_id"].is_null();
                let ack = value["type"] == "user" && value["uuid"] == input_id;
                let response = value["type"] == "assistant" || value["type"] == "stream_event";
                if initialized && session_seen && !accepted && root && (ack || response) {
                    accepted = true;
                    session.after_message_id = after.clone();
                    app.store.save_claude_session(&chat.meta.id, &bot.id, &workdir_key, &session)?;
                }
                output.event(&value);
            }
        }
    }
}

async fn input_for(app: &Arc<App>, bot: &Bot, workdir: &Path, messages: &[Message], fresh: bool) -> Vec<Value> {
    use base64::Engine;
    let input = native::input_for(app, bot, workdir, messages, fresh).await;
    let mut content = Vec::new();
    for item in input {
        if item["type"] == "text" {
            content.push(item);
        } else if let Some(path) = item["path"].as_str() {
            let media = Path::new(path)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime = match media.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => continue,
            };
            if let Ok(data) = std::fs::read(path) {
                if data.len() <= 5 * 1024 * 1024 {
                    content.push(json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": base64::engine::general_purpose::STANDARD.encode(data) } }));
                }
            }
        }
    }
    content
}

struct Bridge {
    app: Arc<App>,
    bot: Bot,
    chat: Chat,
    tools: Vec<Arc<dyn Tool>>,
    plugins: Arc<crate::plugins::mcp::TurnTools>,
    unattended: bool,
}
impl Bridge {
    async fn control(&self, id: &str, request: &Value, cancel: &CancellationToken) -> Value {
        let result = match request["subtype"].as_str().unwrap_or("") {
            "mcp_message" if request["server_name"] == "lorca" => {
                Ok(json!({ "mcp_response": self.mcp(&request["message"], cancel).await }))
            }
            "can_use_tool" => Ok(self.permission(request, cancel).await),
            other => Err(format!("Unsupported Claude control request: {other}")),
        };
        match result {
            Ok(response) => {
                json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id, "response": response } })
            }
            Err(error) => control_error(id, &error),
        }
    }

    async fn permission(&self, request: &Value, cancel: &CancellationToken) -> Value {
        let name = request["tool_name"].as_str().unwrap_or("Unknown tool");
        let input = request["input"].clone();
        // The inner dispatch applies exact Lorca plugin rules and permission cards.
        if name == "mcp__lorca__lorca_call" {
            return json!({ "behavior": "allow", "updatedInput": input });
        }
        if name == "AskUserQuestion" {
            if !self.unattended {
                let questions = input["questions"]
                    .as_array()
                    .map(|questions| {
                        questions
                            .iter()
                            .map(|q| {
                                let options = q["options"]
                                    .as_array()
                                    .map(|options| options.iter().filter_map(|o| o["label"].as_str()).collect::<Vec<_>>().join(" / "))
                                    .unwrap_or_default();
                                format!("{}\n{}", q["question"].as_str().unwrap_or(""), options)
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                self.app.notice(
                    &self.chat.meta.id,
                    format!("Claude asks:\n{questions}\nReply in the chat to continue."),
                );
            }
            return json!({ "behavior": "deny", "message": "The question has been deferred to the Lorca chat. Wait for the user's next message; do not infer an answer.", "interrupt": true });
        }
        if self.unattended {
            return json!({ "behavior": "deny", "message": "No user is present to approve this scheduled action." });
        }
        let summary = format!("{name}\n{}", serde_json::to_string_pretty(&input).unwrap_or_default());
        let decision = crate::plugins::mcp::ask_with_rule(
            &self.app,
            &self.chat.meta.id,
            &self.bot.id,
            "claude",
            "Claude",
            "approval",
            &summary,
            request.clone(),
            request["decision_reason"].as_str().map(str::to_string),
            None,
            cancel,
        )
        .await;
        if matches!(
            decision,
            crate::plugins::mcp::Decision::Allowed | crate::plugins::mcp::Decision::Always
        ) {
            json!({ "behavior": "allow", "updatedInput": input })
        } else {
            json!({ "behavior": "deny", "message": "The user denied this action." })
        }
    }

    async fn mcp(&self, message: &Value, cancel: &CancellationToken) -> Value {
        let id = message.get("id").cloned();
        let result = match message["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({ "protocolVersion": "2024-11-05", "capabilities": { "tools": {} }, "serverInfo": { "name": "lorca", "version": crate::config::VERSION } }),
            ),
            "notifications/initialized" | "notifications/cancelled" | "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": [{
                "name": "lorca_call", "description": "Call a Lorca team, memory, routine, or discovered plugin tool. tool=catalog returns its current schemas.",
                "inputSchema": { "type": "object", "properties": { "tool": { "type": "string" }, "arguments": { "type": "object" } }, "required": ["tool", "arguments"], "additionalProperties": false }
            }] })),
            "tools/call" if message["params"]["name"] == "lorca_call" => {
                let call_id = id.as_ref().map(Value::to_string).unwrap_or_else(|| "claude".into());
                let result = native::call_tool(&message["params"]["arguments"], &call_id, &self.tools, &self.plugins, cancel).await;
                let (content, error) = match result {
                    Ok(result) => (
                        result
                            .content
                            .iter()
                            .map(|part| match part {
                                ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                                ContentPart::Image { data, mime_type } => json!({ "type": "image", "data": data, "mimeType": mime_type }),
                            })
                            .collect::<Vec<_>>(),
                        false,
                    ),
                    Err(error) => (vec![json!({ "type": "text", "text": error.to_string() })], true),
                };
                Ok(json!({ "content": content, "isError": error }))
            }
            method => Err(format!("Unsupported Lorca MCP method or tool: {method}")),
        };
        let mut response = json!({ "jsonrpc": "2.0" });
        if let Some(id) = id {
            response["id"] = id;
        }
        match result {
            Ok(result) => response["result"] = result,
            Err(message) => response["error"] = json!({ "code": -32601, "message": message }),
        }
        response
    }
}

fn control_error(id: &str, error: &str) -> Value {
    json!({ "type": "control_response", "response": { "subtype": "error", "request_id": id, "error": error } })
}

#[cfg(test)]
mod tests;
