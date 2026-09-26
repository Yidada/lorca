use super::*;
use std::path::PathBuf;
use std::sync::Mutex;

struct Fixture {
    app: Arc<App>,
    home: PathBuf,
    bot: Bot,
    chat: Chat,
    job: Job,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("lorca-claude-test-{}", uuid::Uuid::new_v4()));
        let app = App::load(crate::config::Config {
            home: home.clone(),
            port: 0,
        })
        .unwrap();
        let bot: Bot = serde_json::from_value(json!({ "id": "bot", "name": "Coder", "description": "Use Claude", "symbol_name": "sparkles", "accent": "blue", "runner_id": "runner", "harness": "claude", "provider": "deepseek", "created_at": 1.0 })).unwrap();
        let chat: Chat = serde_json::from_value(
            json!({ "id": "chat", "kind": "dm", "bot_ids": ["bot"], "is_pinned": false, "created_at": 1.0, "unread_count": 0 }),
        )
        .unwrap();
        let message = Message::new("chat", Author::You, Body::text("first request"));
        let job: Job = serde_json::from_value(json!({ "id": "job", "chat_id": "chat", "bot_id": "bot", "kind": "turn", "trigger_message_id": message.id, "requested_by": "runner", "created_at": 1.0 })).unwrap();
        {
            let mut state = app.state.lock().unwrap();
            state.bots.push(bot.clone());
            state.chats.push(chat.clone());
        }
        app.upsert_message(message, false);
        Self { app, home, bot, chat, job }
    }
    fn session(&self) -> (ClaudeSession, bool) {
        let stored = self.app.store.claude_session("chat", "bot", &self.home.to_string_lossy()).unwrap();
        let fresh = stored.is_none();
        (
            stored.unwrap_or_else(|| ClaudeSession {
                session_id: uuid::Uuid::new_v4().to_string(),
                after_message_id: None,
            }),
            fresh,
        )
    }
    fn bridge(&self, unattended: bool) -> Arc<Bridge> {
        let (tools, plugins, _) = native::context(&self.app, &self.job, &self.bot, &self.chat, None, &Trigger::default());
        Arc::new(Bridge {
            app: self.app.clone(),
            bot: self.bot.clone(),
            chat: self.chat.clone(),
            tools,
            plugins,
            unattended,
        })
    }
    async fn run(&self, connection: &mut Connection, session: ClaudeSession, fresh: bool) -> anyhow::Result<Output> {
        let mut output = Output::new(&self.app, &self.job, &self.bot);
        run_connected(
            connection,
            &self.app,
            &self.job,
            &self.bot,
            &self.chat,
            None,
            &self.home,
            session,
            fresh,
            self.bridge(false),
            &CancellationToken::new(),
            &mut output,
        )
        .await?;
        output.finish();
        Ok(output)
    }
}

fn pair(capacity: usize) -> (Connection, Connection) {
    let (a, b) = tokio::io::duplex(capacity);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    (Connection::from_io(ar, aw), Connection::from_io(br, bw))
}

fn server(mode: &str, session: &ClaudeSession) -> (Connection, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let (client, mut server) = pair(1024);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = captured.clone();
    let mode = mode.to_string();
    let session_id = session.session_id.clone();
    let task = tokio::spawn(async move {
        let init = server.next().await.unwrap();
        assert_eq!(init["request"]["subtype"], "initialize");
        // MCP negotiation can arrive before the initialization reply.
        server.send(json!({ "type": "control_request", "request_id": "mcp", "request": { "subtype": "mcp_message", "server_name": "lorca", "message": { "jsonrpc": "2.0", "id": 1, "method": "tools/list" } } })).unwrap();
        let reply = server.next().await.unwrap();
        assert_eq!(
            reply["response"]["response"]["mcp_response"]["result"]["tools"][0]["name"],
            "lorca_call"
        );
        server.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "lorca-initialize", "response": {} } })).unwrap();
        let input = server.next().await.unwrap();
        requests.lock().unwrap().push(input.clone());
        if mode == "disconnect" {
            return;
        }
        server
            .send(json!({ "type": "system", "subtype": "init", "session_id": if mode == "wrong-session" { "wrong" } else { &session_id } }))
            .unwrap();
        server.send(input).unwrap();
        if mode == "approval" {
            server.send(json!({ "type": "control_request", "request_id": "permission", "request": { "subtype": "can_use_tool", "tool_name": "Write", "input": { "file_path": "/blocked" } } })).unwrap();
            let reply = server.next().await.unwrap();
            assert_eq!(reply["response"]["response"]["behavior"], "deny");
        }
        server
            .send(json!({ "type": "stream_event", "event": { "type": "message_start", "message": { "id": "reply" } } }))
            .unwrap();
        server.send(json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Done with Claude." } } })).unwrap();
        server.send(json!({ "type": "assistant", "message": { "id": "reply", "content": [{ "type": "text", "text": "Done with Claude." }, { "type": "tool_use", "id": "tool", "name": "Read", "input": { "file_path": "README.md" } }] } })).unwrap();
        server.send(json!({ "type": "assistant", "parent_tool_use_id": "child", "message": { "id": "child", "content": [{ "type": "text", "text": "hidden child output" }] } })).unwrap();
        server.send(json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "tool_use_id": "tool", "content": "read result" }] } })).unwrap();
        server.send(json!({ "type": "result", "subtype": if mode == "failed" { "error_during_execution" } else { "success" }, "is_error": mode == "failed", "session_id": session_id, "result": "Done with Claude.", "errors": ["model unavailable"] })).unwrap();
        // Keep pumps alive until the client's turn has finished and drops its connection.
        let _ = server.next().await;
    });
    (client, captured, task)
}

#[tokio::test]
async fn starts_and_resumes_without_replaying_own_text_and_isolates_sessions() {
    let fixture = Fixture::new();
    let mut first_session = None;
    for i in 0..2 {
        if i == 1 {
            fixture
                .app
                .upsert_message(Message::new("chat", Author::You, Body::text("second request")), false);
        }
        let (session, fresh) = fixture.session();
        if let Some(first) = &first_session {
            assert_eq!(&session.session_id, first);
        } else {
            first_session = Some(session.session_id.clone());
        }
        let (mut connection, requests, task) = server("ok", &session);
        let output = fixture.run(&mut connection, session, fresh).await.unwrap();
        assert_eq!(output.last_text.as_deref(), Some("Done with Claude."));
        assert_eq!(output.tools_used, ["Read"]);
        let input = requests.lock().unwrap()[0]["message"]["content"].to_string();
        assert!(input.contains(if fresh { "first request" } else { "second request" }));
        assert!(!input.contains("Done with Claude"));
        if !fresh {
            assert!(!input.contains("first request"));
        }
        drop(connection);
        task.await.unwrap();
    }
    let rows = fixture.app.store.all("chat").unwrap();
    assert_eq!(
        rows.iter()
            .filter(|m| matches!(&m.body, Body::Text { text, .. } if text == "Done with Claude."))
            .count(),
        2
    );
    assert!(!rows
        .iter()
        .any(|m| matches!(&m.body, Body::Text { text, .. } if text.contains("hidden child"))));
    assert!(rows
        .iter()
        .filter_map(|m| if let Body::Tool { is_running, result, .. } = &m.body {
            Some(!is_running && result.is_some())
        } else {
            None
        })
        .all(|done| done));
    let store = crate::local_store::LocalStore::open(&fixture.home.join("lorca.sqlite3")).unwrap();
    assert!(store
        .claude_session("chat", "bot", &fixture.home.to_string_lossy())
        .unwrap()
        .is_some());
    for (chat, bot, dir) in [
        ("other", "bot", fixture.home.to_string_lossy().as_ref()),
        ("chat", "other", "/other"),
        ("chat", "bot", "/other"),
    ] {
        assert!(store.claude_session(chat, bot, dir).unwrap().is_none());
    }
    store.clear().unwrap();
    assert!(store
        .claude_session("chat", "bot", &fixture.home.to_string_lossy())
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn startup_failure_does_not_advance_cursor_and_errors_do_not_fall_back_to_another_session() {
    for mode in ["disconnect", "wrong-session", "failed"] {
        let fixture = Fixture::new();
        let (session, fresh) = fixture.session();
        let (mut connection, _, task) = server(mode, &session);
        let error = fixture.run(&mut connection, session, fresh).await.err().expect("must fail");
        assert!(!error.to_string().is_empty());
        let stored = fixture
            .app
            .store
            .claude_session("chat", "bot", &fixture.home.to_string_lossy())
            .unwrap();
        if mode == "failed" {
            assert!(stored.unwrap().after_message_id.is_some());
        } else {
            assert!(stored.is_none());
        }
        drop(connection);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn permission_round_trip_denies_and_closes_cards() {
    let fixture = Fixture::new();
    let (session, fresh) = fixture.session();
    let (mut connection, _, task) = server("approval", &session);
    let app = fixture.app.clone();
    let answer = tokio::spawn(async move {
        loop {
            let id = app.pending_permissions.lock().unwrap().keys().next().cloned();
            if let Some(id) = id {
                crate::plugins::mcp::answer(&app, &id, crate::plugins::mcp::Decision::Denied);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.run(&mut connection, session, fresh))
        .await
        .unwrap()
        .unwrap();
    answer.await.unwrap();
    assert!(fixture.app.pending_permissions.lock().unwrap().is_empty());
    drop(connection);
    task.await.unwrap();
}

#[tokio::test]
async fn permissions_support_allow_cancel_unattended_and_questions() {
    let fixture = Fixture::new();
    let request = json!({ "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "echo hello" } });
    assert_eq!(
        fixture.bridge(true).permission(&request, &CancellationToken::new()).await["behavior"],
        "deny"
    );
    for allowed in [true, false] {
        let bridge = fixture.bridge(false);
        let cancel = CancellationToken::new();
        let pending = {
            let bridge = bridge.clone();
            let request = request.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { bridge.permission(&request, &cancel).await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let id = fixture.app.pending_permissions.lock().unwrap().keys().next().cloned();
                if let Some(id) = id {
                    if allowed {
                        crate::plugins::mcp::answer(&fixture.app, &id, crate::plugins::mcp::Decision::Allowed);
                    } else {
                        cancel.cancel();
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let response = pending.await.unwrap();
        assert_eq!(response["behavior"], if allowed { "allow" } else { "deny" });
        native::close_permissions(&fixture.app, "chat");
        assert!(fixture.app.pending_permissions.lock().unwrap().is_empty());
    }
    let question = fixture.bridge(false).permission(&json!({ "tool_name": "AskUserQuestion", "input": { "questions": [{ "question": "Which folder?", "options": [{ "label": "A" }] }] } }), &CancellationToken::new()).await;
    assert_eq!(question["interrupt"], true);
    assert!(fixture
        .app
        .store
        .all("chat")
        .unwrap()
        .iter()
        .any(|m| matches!(&m.body, Body::Notice { text, .. } if text.contains("Which folder?"))));
}

#[tokio::test]
async fn mcp_dispatches_catalog_and_validates_tools_without_provider_credentials() {
    let fixture = Fixture::new();
    let bridge = fixture.bridge(false);
    for (tool, args, error) in [
        ("catalog", json!({}), false),
        ("list_teammates", json!({}), false),
        ("unknown", json!({}), true),
        ("message_bot", json!({}), true),
    ] {
        let response = bridge.mcp(&json!({ "id": 1, "method": "tools/call", "params": { "name": "lorca_call", "arguments": { "tool": tool, "arguments": args } } }), &CancellationToken::new()).await;
        assert_eq!(response["result"]["isError"], error, "{response}");
        assert!(response["result"]["content"][0]["text"].is_string());
    }
    assert_eq!(
        bridge
            .control("unknown", &json!({ "subtype": "future_protocol" }), &CancellationToken::new())
            .await["response"]["subtype"],
        "error"
    );
}

#[tokio::test]
async fn duplex_pumps_handle_large_input_and_startup_output_without_deadlock() {
    let (client, mut server) = pair(256);
    client.send(json!({ "input": "x".repeat(200_000) })).unwrap();
    server.send(json!({ "output": "y".repeat(200_000) })).unwrap();
    let mut client = client;
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(client.next(), server.next()) })
        .await
        .unwrap();
    assert_eq!(a.unwrap()["output"].as_str().unwrap().len(), 200_000);
    assert_eq!(b.unwrap()["input"].as_str().unwrap().len(), 200_000);
}

#[tokio::test]
async fn malformed_protocol_and_eof_are_errors_and_stop_sends_interrupt() {
    for input in ["{broken}\n", ""] {
        let mut connection = Connection::from_io(std::io::Cursor::new(input.as_bytes().to_vec()), tokio::io::sink());
        assert!(connection.next().await.is_err());
    }
    let (mut client, mut server) = pair(1024);
    let task = tokio::spawn(async move {
        assert_eq!(server.next().await.unwrap()["request"]["subtype"], "interrupt");
        server.send(json!({ "type": "result" })).unwrap();
        let _ = server.next().await;
    });
    client.stop().await;
    drop(client);
    task.await.unwrap();
}

#[test]
fn options_preserve_native_defaults_and_never_bypass_permissions() {
    let mut fixture = Fixture::new();
    let (session, _) = fixture.session();
    let args = transport::cli_args(&fixture.bot, &session, false, Path::new("/prompt")).unwrap();
    assert!(args.contains(&format!("--session-id={}", session.session_id)));
    assert!(!args
        .iter()
        .any(|a| a.contains("bypass") || a.starts_with("--model") || a == "--strict-mcp-config" || a == "--system-prompt"));
    fixture.bot.model = Some("sonnet".into());
    fixture.bot.thinking = Some("high".into());
    let args = transport::cli_args(&fixture.bot, &session, true, Path::new("/prompt")).unwrap();
    assert!(args.contains(&format!("--resume={}", session.session_id)));
    assert!(args.contains(&"--model=sonnet".into()) && args.contains(&"--effort=high".into()));
    fixture.bot.thinking = Some("off".into());
    assert!(transport::cli_args(&fixture.bot, &session, true, Path::new("/prompt")).is_err());
}

/// Uses the Runner's Claude login; makes real model requests only when explicitly selected.
#[tokio::test]
#[ignore = "requires Claude Code installed and signed in; uses model credits"]
async fn claude_live_read_resume_and_lorca_tool() {
    let fixture = Fixture::new();
    let marker = format!("marker-{}", uuid::Uuid::new_v4());
    std::fs::write(fixture.home.join("probe.txt"), &marker).unwrap();
    let first = Message::new(
        "chat",
        Author::You,
        Body::text(
            "Read probe.txt with your native Read tool, then call mcp__lorca__lorca_call with tool=list_teammates and arguments={}. Reply with only the exact file contents.",
        ),
    );
    fixture.app.upsert_message(first, false);
    for i in 0..2 {
        if i == 1 {
            fixture.app.upsert_message(
                Message::new(
                    "chat",
                    Author::You,
                    Body::text("Without using tools, repeat the exact marker you read in the previous turn."),
                ),
                false,
            );
        }
        let (session, fresh) = fixture.session();
        let (_, _, instructions) = native::context(&fixture.app, &fixture.job, &fixture.bot, &fixture.chat, None, &Trigger::default());
        let prompt = PromptFile::new(&fixture.home, &instructions).unwrap();
        let mut connection = Connection::spawn(&fixture.home, &fixture.bot, &session, !fresh, &prompt.0)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(180), fixture.run(&mut connection, session, fresh)).await;
        let diagnostic = connection.diagnostic();
        connection.finish().await;
        let output = result
            .expect("Claude live turn timed out")
            .unwrap_or_else(|e| panic!("{e:#}\n{diagnostic}"));
        assert!(
            output.last_text.as_deref().unwrap_or("").contains(&marker),
            "{:?}",
            output.last_text
        );
        if i == 0 {
            assert!(output.tools_used.iter().any(|t| t == "Read"), "{:?}", output.tools_used);
            assert!(
                output.tools_used.iter().any(|t| t == "mcp__lorca__lorca_call"),
                "{:?}",
                output.tools_used
            );
        }
    }
}

#[tokio::test]
async fn api_round_trips_claude_settings_and_native_plugins_require_review() {
    let fixture = Fixture::new();
    let result = crate::api::dispatch(
        &fixture.app,
        "bots.update",
        json!({ "id": "bot", "harness": "claude", "model": "sonnet", "thinking": "high" }),
    )
    .await
    .unwrap();
    assert_eq!(result["bot"]["harness"], "claude");
    assert_eq!(result["bot"]["model"], "sonnet");
    let verdict = crate::plugins::review::decide(
        &fixture.app,
        &fixture.bot,
        "chat",
        &Trigger::default(),
        "plugin",
        "Plugin",
        "write",
        "Write a file",
        &json!({}),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(verdict, crate::plugins::review::Outcome::Ask { .. }));
    let (session, _) = fixture.session();
    fixture
        .app
        .store
        .save_claude_session("chat", "bot", &fixture.home.to_string_lossy(), &session)
        .unwrap();
    fixture.app.store.clear_claude_sessions("chat", "bot").unwrap();
    assert!(fixture.session().1);
}

#[tokio::test]
#[ignore = "requires Claude Code installed and signed in; uses model credits"]
async fn claude_live_denied_write_has_no_side_effect() {
    let fixture = Fixture::new();
    let config = fixture.home.join(".claude");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("settings.local.json"),
        r#"{"permissions":{"ask":["Write","Edit","Bash"]}}"#,
    )
    .unwrap();
    let mut original = fixture.app.message("chat", &fixture.job.trigger_message_id).unwrap();
    original.body = Body::text(
        "Use the native Write tool to create denied.txt containing HELLO in this directory. If permission is denied, stop immediately, say denied, and do not try any alternative tool or path.",
    );
    fixture.app.upsert_message(original, false);
    let (session, fresh) = fixture.session();
    let (_, _, instructions) = native::context(&fixture.app, &fixture.job, &fixture.bot, &fixture.chat, None, &Trigger::default());
    let prompt = PromptFile::new(&fixture.home, &instructions).unwrap();
    let mut connection = Connection::spawn(&fixture.home, &fixture.bot, &session, !fresh, &prompt.0)
        .await
        .unwrap();
    let app = fixture.app.clone();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = count.clone();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let answers = tokio::spawn(async move {
        loop {
            let ids = app.pending_permissions.lock().unwrap().keys().cloned().collect::<Vec<_>>();
            for id in ids {
                if crate::plugins::mcp::answer(&app, &id, crate::plugins::mcp::Decision::Denied) {
                    observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            tokio::select! { _ = stopped.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(10)) => {} }
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(120), fixture.run(&mut connection, session, fresh)).await;
    stop.cancel();
    answers.await.unwrap();
    let diagnostic = connection.diagnostic();
    connection.finish().await;
    result.unwrap().unwrap_or_else(|e| panic!("{e:#}\n{diagnostic}"));
    assert!(
        count.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "Claude must request permission"
    );
    assert!(!fixture.home.join("denied.txt").exists());
    assert!(fixture.app.pending_permissions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_control_request_releases_its_permission_card() {
    let fixture = Fixture::new();
    let (session, fresh) = fixture.session();
    let id = session.session_id.clone();
    let app = fixture.app.clone();
    let (mut client, mut server) = pair(4096);
    let task = tokio::spawn(async move {
        server.next().await.unwrap();
        server
            .send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "lorca-initialize" } }))
            .unwrap();
        server.next().await.unwrap();
        server
            .send(json!({ "type": "system", "subtype": "init", "session_id": id }))
            .unwrap();
        server.send(json!({ "type": "control_request", "request_id": "cancel-me", "request": { "subtype": "can_use_tool", "tool_name": "Write", "input": {} } })).unwrap();
        while app.pending_permissions.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server
            .send(json!({ "type": "control_cancel_request", "request_id": "cancel-me" }))
            .unwrap();
        while !app.pending_permissions.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server
            .send(json!({ "type": "result", "subtype": "success", "session_id": id, "result": "Cancelled" }))
            .unwrap();
        // The abandoned request gets no late reply.
        assert!(server.next().await.is_err());
    });
    tokio::time::timeout(Duration::from_secs(3), fixture.run(&mut client, session, fresh))
        .await
        .unwrap()
        .unwrap();
    drop(client);
    task.await.unwrap();
    assert!(fixture.app.pending_permissions.lock().unwrap().is_empty());
    assert!(fixture
        .app
        .store
        .all("chat")
        .unwrap()
        .iter()
        .any(|m| matches!(&m.body, Body::Permission { decision, .. } if decision == "denied")));
}
