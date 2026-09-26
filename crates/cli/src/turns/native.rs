//! Lorca context and tool dispatch shared by native runtimes.
use super::*;
use std::path::Path;

pub(super) fn context(
    app: &Arc<App>,
    job: &Job,
    bot: &Bot,
    chat: &Chat,
    routine: Option<&Routine>,
    trigger: &Trigger,
) -> (Vec<Arc<dyn Tool>>, Arc<crate::plugins::mcp::TurnTools>, String) {
    let store = MemoryStore::for_bot(&app.config.home, bot);
    let (plugin_tools, briefs) = crate::plugins::mcp::turn_tools(app, &chat.meta.id, trigger, bot, routine.is_some());
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ListTeammates {
            app: app.clone(),
            chat_id: chat.meta.id.clone(),
        }),
        Arc::new(MessageBot {
            app: app.clone(),
            chat_id: chat.meta.id.clone(),
            bot: bot.clone(),
            hops: job.hops,
        }),
        Arc::new(CreateBot {
            app: app.clone(),
            chat_id: chat.meta.id.clone(),
            bot: bot.clone(),
        }),
        Arc::new(EditBot {
            app: app.clone(),
            bot: bot.clone(),
        }),
        Arc::new(Routines {
            app: app.clone(),
            bot: bot.clone(),
        }),
        Arc::new(SearchPlugins { app: app.clone() }),
        Arc::new(InstallPlugin {
            app: app.clone(),
            chat_id: chat.meta.id.clone(),
            bot: bot.clone(),
            unattended: routine.is_some(),
        }),
        Arc::new(ConnectPlugin {
            app: app.clone(),
            chat_id: chat.meta.id.clone(),
            bot: bot.clone(),
        }),
        Arc::new(Recall {
            app: app.clone(),
            store: store.clone(),
            bot: bot.clone(),
        }),
    ];
    tools.extend(memory_tools(&store, chat));
    tools.extend(plugin_tools.discovery_tools());
    let mut instructions = system_prompt(app, chat, bot, job, &store, routine, &briefs);
    instructions.push_str("\nLorca tool catalog (call through lorca_call; use tool=catalog to refresh after plugin discovery):\n");
    instructions.push_str(&serde_json::to_string(&tools.iter().map(|t| t.spec()).collect::<Vec<_>>()).expect("tool catalog serializes"));
    (tools, plugin_tools, instructions)
}

/// A cancelled future must leave neither an unanswered card nor a dropped sender behind.
pub(super) fn close_permissions(app: &App, chat_id: &str) {
    let pending: Vec<_> = app
        .pending_permissions
        .lock()
        .unwrap()
        .extract_if(|_, (chat, _)| chat == chat_id)
        .collect();
    for (id, (_, sender)) in pending {
        let _ = sender.send(crate::plugins::mcp::Decision::Denied);
        if let Some(mut row) = app.message(chat_id, &id) {
            if let Body::Permission { decision, .. } = &mut row.body {
                *decision = "denied".into();
            }
            app.upsert_message(row, true);
        }
    }
}

pub(super) async fn input_for(app: &Arc<App>, bot: &Bot, workdir: &Path, messages: &[Message], include_own: bool) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages.iter().filter(|m| m.is_complete()) {
        match (&message.author, &message.body) {
            (Author::Bot { bot_id }, Body::Handoff { to, reason, .. }) if bot_id != &bot.id => {
                let from = app.bot(bot_id).map(|b| b.name).unwrap_or_else(|| bot_id.clone());
                let text = if to == &bot.id {
                    format!("[Message from {from}]: {reason}")
                } else {
                    let to = app.bot(to).map(|b| b.name).unwrap_or_else(|| to.clone());
                    format!("[{from} → {to}]: {reason}")
                };
                input.push(json!({ "type": "text", "text": text }));
            }
            (
                Author::System,
                Body::Notice {
                    text,
                    routine_id: Some(id),
                },
            ) => {
                let text = match app.routine(id) {
                    Some(routine) => format!("[Routine {}]: {}", routine.name, routine.prompt),
                    None => format!("[{text} ran on its schedule]"),
                };
                input.push(json!({ "type": "text", "text": text }));
            }
            _ => {}
        }
        if let Body::Text {
            text,
            attachments,
            mentions,
        } = &message.body
        {
            if !include_own && matches!(&message.author, Author::Bot { bot_id } if bot_id == &bot.id) {
                continue;
            }
            let label = match &message.author {
                Author::You => "User".into(),
                Author::Bot { bot_id } => app.bot(bot_id).map(|b| b.name).unwrap_or_else(|| bot_id.clone()),
                Author::System => "Context".into(),
            };
            input.push(json!({ "type": "text", "text": format!("[{label}] {}", with_mention_ids(app, text, mentions)) }));
            crate::files::prefetch(app, attachments).await;
            for attachment in attachments {
                if let Some(path) = crate::files::materialize(app, attachment, workdir) {
                    input.push(json!({ "type": "text", "text": format!("Attachment {}: {}", attachment.name, path.display()) }));
                    if attachment.is_image() {
                        input.push(json!({ "type": "localImage", "path": path }));
                    }
                } else {
                    input.push(json!({ "type": "text", "text": format!("Attachment {} is unavailable on this Runner", attachment.name) }));
                }
            }
        }
    }
    input
}

pub(super) async fn call_tool(
    args: &Value,
    call_id: &str,
    base: &[Arc<dyn Tool>],
    plugins: &crate::plugins::mcp::TurnTools,
    cancel: &CancellationToken,
) -> Result<ToolResult, ToolError> {
    let name = args["tool"].as_str().unwrap_or("");
    let tools = plugins.tools_with_selected(base);
    async {
        if name == "catalog" {
            return Ok(ToolResult::text(serde_json::to_string(
                &tools.iter().map(|t| t.spec()).collect::<Vec<_>>(),
            )?));
        }
        let tool = tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| ToolError(format!("Unknown Lorca tool {name}; search plugins or call catalog first")))?;
        let args = tool.prepare_arguments(args["arguments"].clone());
        let args = lorca_agent::schema::validate_tool_arguments(name, &tool.parameters(), &args).map_err(ToolError)?;
        tool.execute(call_id, args, cancel.clone(), Arc::new(|_| {})).await
    }
    .await
}
