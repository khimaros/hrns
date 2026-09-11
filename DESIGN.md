# architecture and design

## overview

`hrns` is built as a minimalist rust application using `rig-core` for LLM provider abstractions and `tokio` for async streaming.

## components

- **configuration management:** cascading config from `hrns.toml` (project) to `~/.config/hrns/config.toml` (global). supports multiple named providers with per-provider client type, API key, and base URL.
- **markdown parser:** extracts yaml frontmatter (via `serde_yaml`) from markdown agent and skill documents, separating config metadata from prompt body.
- **file locator:** walks up from cwd to git root searching kind-specific subdirs for agents (`.opencode/agents`, `.claude/agents`, `.agents/agents`) and skills (`.opencode/skills`, `.claude/skills`, `.agents/skills`, `.pi/agent/skills`). loose layouts like `.agents/foo.md` also resolve via flat fallback. falls back to globals under `~/.config/opencode/`, `~/.claude/`, `~/.agents/`, and `~/.pi/agent/skills/`.
- **tools:** pluggable tool system using the `rig` `Tool` trait. tools are enabled per-agent via frontmatter, globally via config, or exclusively via `--tools` CLI flag.
  - `read`: reads file contents, optionally a slice via `offset` (0-indexed line) and `count` (number of lines). permissions use path-mode glob matching (`*` stops at `/`). returns `{"content": "..."}`.
  - `bash`: executes commands via `sh -c`. permissions use command-mode glob matching (`*` matches anything). commands with shell metacharacters fall back to the catch-all permission. returns `{"exit_code": N, "stdout": "...", "stderr": "..."}` (the `stderr` key is omitted when empty).
- **permissions:** unified model shared across all tools. each tool maps to either a flat level (`allow`/`ask`/`deny`) or a glob pattern map. `ask` prompts the user via `/dev/tty`, bypassable with `--yes`. `check_tool_permission()` is the single entry point.
- **glob matching:** supports `*`, `**`, `?` with two modes: path mode (read tool, `*` stops at `/`) and command mode (bash tool, `*` matches anything). most specific pattern wins.
- **streaming client:** uses `rig-core` provider abstractions (openai, anthropic, gemini, cohere, xai) to stream chat completions. response text goes to stdout, reasoning tokens to stderr (dim italic), tool calls/results logged to stderr. tool-result rendering is truncated to `tool_output_truncate` bytes (default 2000); when truncated, the full content is spooled to `$XDG_CACHE_HOME/hrns/<pid>/<seq>.txt` and a yellow hint pointing at the file is emitted.
- **system prompt resolution:** `-s` flag overrides everything; otherwise the agent body (with skills appended) is used; if both are absent, falls back to `default_system_prompt` from config.
- **hooks (partial conformance):** external executables under `hooks/` in the same base directories used for agents/skills (`.opencode`, `.claude`, `.agents`, plus globals). each script is invoked once per event with the stage as `argv[1]`, a single JSON object on stdin (terminated by EOF), and JSONL on stdout. lines are merged per the protocol's composability rules (arrays concat, scalars join with newline). implemented stages: `discover` (registers tools as `<prefix>_<short>`), `mutate_request` (system strings appended to the agent's system prompt; payload includes finalized system/user/model/tools), `execute_tool` (routes calls to hook-registered tools), `before_tool` / `after_tool` (observation plus mutation: `deny`, `args`, `result`), `before_stop` (post-loop observational with `exit_reason`/`final`/`error`). non-executable files and basenames starting with `.` or `__` are skipped. hook tools flow through the same `check_tool_permission()` pipeline as built-ins, with `permission.arg` selecting the pattern key. tier 1/2 stages are not implemented; see the [hcp-spec](https://github.com/khimaros/hcp-spec/) for the full protocol and `ROADMAP.md` for follow-ups.

## dependencies

kept minimal:
- `rig-core`: LLM provider abstractions and tool framework
- `tokio`: async runtime
- `serde`, `serde_json`, `serde_yaml`: parsing structs
- `toml`: configuration parsing
- `clap`: CLI argument parsing
- `futures-util`: stream combinators
- `tracing-subscriber`: debug logging
