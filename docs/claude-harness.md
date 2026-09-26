# Claude Code runtime

Select **Claude** under a bot's **Runtime** in the macOS New Bot sheet, onboarding, inspector, or the phone's New Bot and Details screens. The bot runs Claude Code on its assigned Runner. That computer must have Claude Code installed and signed in; opening `claude` there completes native setup. Set `LORCA_CLAUDE_BIN` before starting the Runner if its binary is outside PATH.

Choose **Runner default** to use the native model configuration, or select the `opus`, `sonnet`, or `haiku` alias. Thinking effort is optional and depends on the installed CLI and model. Unsupported settings produce a chat failure. Changing Runtime clears the apps' model and thinking overrides.

## Behavior

- Native coding tools, CLAUDE.md, skills, hooks, and the Runner's MCP servers are available. Lorca adds team, memory, routine, and plugin tools through `mcp__lorca__lorca_call`.
- The Runner stores the native session association locally. Subsequent turns resume that session; another Runner or working directory starts from the recent Lorca chat. Native transcript files stay in Claude's storage.
- Text streams into the chat; tool cards show execution and results. Permission requests Claude sends to its host can be answered from paired Devices. Claude's existing native allow rules apply. Lorca does not enable bypass permissions.
- Claude handles compaction and native model usage. Lorca provider credentials and its provider cost estimates do not apply to these turns.
- Stop interrupts Claude and ends the job's process tree. Bash and subagent background execution is disabled so native work stays within the current job. Incoming user messages run in the next resumed turn.
- Claude's structured questions appear as chat notices. Answer in the composer to continue. Scheduled turns decline requests needing user approval.
- A missing executable, expired login, invalid model, or unavailable native session produces a visible error. A failed resume never silently selects a different native session. Create a new chat for fresh context.

## Validation

The adapter is checked against Claude Code **2.1.220**. The SDK control transport is based on Anthropic's [Python SDK subprocess implementation](https://github.com/anthropics/claude-agent-sdk-python/tree/main/src/claude_agent_sdk/_internal), with [CLI stream JSON](https://code.claude.com/docs/en/headless) and the documented [background-task setting](https://code.claude.com/docs/en/env-vars).

Offline protocol coverage:

```sh
cargo test -p lorca --lib
```

Opt-in end-to-end test, using the Runner's existing login and model credits:

```sh
cargo test -p lorca --lib claude_live_ -- --ignored --nocapture
```

The live test creates a scratch workspace, reads a random marker with the native Read tool, calls the fixture's Lorca teammate catalog, and recalls the marker after restarting and resuming Claude. A second live test denies a native Write permission and verifies the target file is absent. It does not use a production Lorca chat. Claude retains the test sessions in its native transcript storage.
