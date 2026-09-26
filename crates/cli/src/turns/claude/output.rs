use super::*;

pub(super) struct Output {
    app: Arc<App>,
    job: Job,
    bot: Bot,
    rows: HashMap<String, Message>,
    texts: HashMap<String, String>,
    completed: std::collections::HashSet<String>,
    flushed: HashMap<String, (usize, std::time::Instant)>,
    stream_id: String,
    pub(super) last_text: Option<String>,
    pub(super) error: Option<String>,
    pub(super) tools_used: Vec<String>,
}
impl Output {
    pub(super) fn new(app: &Arc<App>, job: &Job, bot: &Bot) -> Self {
        Self {
            app: app.clone(),
            job: job.clone(),
            bot: bot.clone(),
            rows: HashMap::new(),
            texts: HashMap::new(),
            completed: Default::default(),
            flushed: HashMap::new(),
            stream_id: String::new(),
            last_text: None,
            error: None,
            tools_used: Vec::new(),
        }
    }
    pub(super) fn has_text(&self) -> bool {
        self.texts.values().any(|s| !s.trim().is_empty())
    }
    pub(super) fn text(&mut self, id: &str, text: &str, complete: bool) {
        if self.completed.contains(id) {
            return;
        }
        self.texts.insert(id.into(), text.into());
        if complete {
            self.completed.insert(id.into());
        }
        if text.trim().is_empty() || is_pass(text) {
            return;
        }
        let (shown, time) = self.flushed.entry(id.into()).or_insert((0, std::time::Instant::now()));
        let cut = if complete {
            Some(text.len())
        } else {
            chunk_boundary(text, *shown, time.elapsed())
        };
        let Some(cut) = cut else { return };
        *shown = cut;
        *time = std::time::Instant::now();
        let row = self.rows.entry(id.into()).or_insert_with(|| {
            Message::new(
                &self.job.chat_id,
                Author::Bot {
                    bot_id: self.bot.id.clone(),
                },
                Body::text(""),
            )
        });
        row.body = Body::text(&text[..cut]);
        row.state = if complete {
            MessageState::Complete
        } else {
            MessageState::Streaming
        };
        self.app.upsert_message(row.clone(), true);
        if complete {
            self.last_text = Some(text.into());
        }
    }
    fn tool(&mut self, id: &str, name: &str, input: &Value) {
        if self.completed.contains(id) {
            return;
        }
        if !self.tools_used.iter().any(|t| t == name) {
            self.tools_used.push(name.into());
        }
        let detail = input["command"]
            .as_str()
            .or(input["file_path"].as_str())
            .or(input["tool"].as_str())
            .unwrap_or(name);
        let row = self.rows.entry(id.into()).or_insert_with(|| {
            Message::new(
                &self.job.chat_id,
                Author::Bot {
                    bot_id: self.bot.id.clone(),
                },
                Body::text(""),
            )
        });
        row.body = Body::Tool {
            name: name.into(),
            summary: format!("Claude · {name}"),
            detail: detail.into(),
            is_running: true,
            call_id: id.into(),
            arguments: input.clone(),
            result: None,
            is_error: false,
            description: None,
            target_bot_id: None,
            run: None,
        };
        row.state = MessageState::Streaming;
        self.app.upsert_message(row.clone(), true);
    }
    pub(super) fn event(&mut self, value: &Value) {
        let root = value["parent_tool_use_id"].is_null();
        match value["type"].as_str().unwrap_or("") {
            "stream_event" if root => {
                let event = &value["event"];
                match event["type"].as_str().unwrap_or("") {
                    "message_start" => self.stream_id = event["message"]["id"].as_str().unwrap_or("").into(),
                    "content_block_start" if event["content_block"]["type"] == "tool_use" => {
                        let block = &event["content_block"];
                        if let Some(id) = block["id"].as_str() {
                            self.tool(id, block["name"].as_str().unwrap_or("Tool"), &block["input"]);
                        }
                    }
                    "content_block_delta" if event["delta"]["type"] == "text_delta" && !self.stream_id.is_empty() => {
                        let id = format!("{}:{}", self.stream_id, event["index"]);
                        let text = self.texts.entry(id.clone()).or_default();
                        text.push_str(event["delta"]["text"].as_str().unwrap_or(""));
                        let text = text.clone();
                        self.text(&id, &text, false);
                    }
                    "content_block_delta" if event["delta"]["type"] == "thinking_delta" => {
                        crate::runtime::report_activity(&self.app, &self.job, JobActivity::Thinking);
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let message = &value["message"];
                let Some(id) = message["id"].as_str() else { return };
                if let Some(blocks) = message["content"].as_array() {
                    for (index, block) in blocks.iter().enumerate() {
                        match block["type"].as_str().unwrap_or("") {
                            "text" if root => self.text(&format!("{id}:{index}"), block["text"].as_str().unwrap_or(""), true),
                            "tool_use" => {
                                if let Some(id) = block["id"].as_str() {
                                    self.tool(id, block["name"].as_str().unwrap_or("Tool"), &block["input"]);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "user" => {
                if let Some(blocks) = value["message"]["content"].as_array() {
                    for block in blocks.iter().filter(|b| b["type"] == "tool_result") {
                        let Some(id) = block["tool_use_id"].as_str() else { continue };
                        self.completed.insert(id.into());
                        if let Some(row) = self.rows.get_mut(id) {
                            if let Body::Tool {
                                is_running,
                                is_error,
                                summary,
                                result,
                                ..
                            } = &mut row.body
                            {
                                *is_running = false;
                                *is_error = block["is_error"] == true;
                                *summary = if *is_error { "Failed" } else { "Finished" }.into();
                                *result = Some(block["content"].to_string());
                            }
                            row.state = MessageState::Complete;
                            self.app.upsert_message(row.clone(), true);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    pub(super) fn fail(&mut self, error: &str) {
        self.error = Some(error.into());
        let mut row = Message::new(
            &self.job.chat_id,
            Author::Bot {
                bot_id: self.bot.id.clone(),
            },
            Body::text(error),
        );
        row.state = MessageState::Failed { error: error.into() };
        self.app.upsert_message(row, true);
    }
    pub(super) fn finish(&mut self) {
        for (id, text) in self.texts.clone() {
            self.text(&id, &text, true);
        }
        for row in self.rows.values_mut() {
            if let Body::Tool { is_running, summary, .. } = &mut row.body {
                if *is_running {
                    *is_running = false;
                    *summary = "Stopped".into();
                    row.state = MessageState::Complete;
                    self.app.upsert_message(row.clone(), true);
                }
            }
        }
    }
}
