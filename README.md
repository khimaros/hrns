# hrns

built on the unix philosophy: pipe a prompt in, get a streaming response out.

`hrns` is a one-shot agent runner compatible with `.claude/` and `.opencode/` directory structures. agents and skills are plain markdown files with yaml frontmatter.

supports OpenAI, Anthropic, Gemini, Cohere, xAI, and OpenAI-compatible endpoints. written in Rust with minimal dependencies.

## quick start

prebuilt binaries for linux and macos (x86_64 and arm64) are attached to each
[release](../../releases), along with `SHA256SUMS`.
extract the tarball and put `hrns` on your `PATH`, or build from source:

```bash
# install
cargo install --path .

# create a config with your API key
hrns --init
$EDITOR ~/.config/hrns/config.toml

# ask a question (bare model query)
echo "explain quicksort in one sentence" | hrns

# use an agent with an inline prompt
hrns admin "update the debian system"

# pipe input to an agent
echo "what time is it?" | hrns my-agent

# pipe agent output to a shell
hrns admin "list installed packages" | bash
```

## demo

```
$ hrns admin "check disk usage"
df -h --output=target,pcent,avail /

$ hrns admin "check disk usage" | bash
Filesystem  Use% Avail
/           42%  120G
```

agents have scoped tools and permissions, so `admin` can run `bash` commands while other agents cannot. the response streams to stdout, tool calls log to stderr, composable with standard unix pipes and redirects.

## agents and skills

agents live in `.agents/agents/`, `.claude/agents/`, or `.opencode/agents/` as markdown with yaml frontmatter:

`.agents/agents/admin.md`
```markdown
---
description: "perform system administration tasks"
tools:
  read: true
  bash: true
permissions:
  read:
    "**": deny
    "/etc/os-release": allow
  bash:
    "*": deny
    "apt update": allow
    "apt clean": allow
skills:
  - system-maintenance
---

you are an expert systems administrator.
```

agent frontmatter supports: `description`, `model`, `tools`, `permissions`, `skills`. agent settings override global config.

skills are stored as `<skill_name>/SKILL.md` or `<skill_name>.md` in the `skills/` subdirectory of `.opencode/`, `.claude/`, or `.agents/`. they can also use yaml frontmatter.

### discovery

`hrns` walks up from your current working directory to the git worktree root, searching for agents and skills in `.opencode/`, `.claude/`, or `.agents/` directories along the way.

if no local definitions are found, it falls back to global definitions at `~/.config/opencode/`, `~/.claude/`, or `~/.agents/`.

## configuration

initialize a default configuration file:

```bash
hrns --init
```

this creates `~/.config/hrns/config.toml`. config files are searched in order:
1. `hrns.toml` (project root)
2. `.hrns.toml` (project root)
3. `.config/hrns.toml` (project root)
4. `~/.config/hrns/config.toml` (global)

example configuration:

```toml
default_model = "anthropic/claude-sonnet-4-20250514"
default_max_tokens = 16384

# fallback when no -s flag and no agent body sets one
default_system_prompt = "you are a helpful assistant."

# stderr tool-output cap in bytes; full output is spooled to
# $XDG_CACHE_HOME/hrns/<pid>/<seq>.txt when exceeded. 0 disables.
tool_output_truncate = 2000

[[providers]]
name = "anthropic"
client = "anthropic"
api_key = "sk-ant-..."

[[providers]]
name = "openai"
client = "openai"
api_key = "sk-..."

[[providers]]
name = "openai-compatible"
client = "openai_completions"
api_key = "fake"
base_url = "http://localhost:7860/v1"
```

supported client types: `openai` (responses API), `openai_completions`, `anthropic`, `gemini`, `cohere`, `xai`.

if no API key is set in the config, `hrns` falls back to the corresponding environment variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `COHERE_API_KEY`, `XAI_API_KEY`). if neither is present, requests are sent without auth; this is fine for local openai-compatible servers that don't require it, while hosted providers will return an upstream auth error.

### tools and permissions

tools can be registered with the LLM via config or the `--tools` flag. supported tools:
- `read`: read file contents at a given path; optional `offset` (0-indexed start line) and `count` (number of lines) for slicing
- `bash`: execute shell commands via `sh -c`

```toml
[tools]
read = false
bash = false

[permissions]
read = "allow"
bash = "deny"

# or with gitignore-style glob patterns (most specific match wins):
# [permissions.bash]
# "**" = "deny"
# "apt update" = "allow"
# "apt clean" = "allow"

# [permissions.read]
# "**" = "deny"
# "/home/**" = "allow"
# "/etc/shadow" = "deny"
```

permission levels:
- `allow`: permitted without prompting
- `ask`: prompts the user for confirmation via `/dev/tty`
- `deny`: blocked silently

patterns support glob syntax: `**` matches any characters (including `/`), `?` matches a single character. for file-path tools (read), `*` matches within a single path segment (stops at `/`). for command tools (bash), `*` matches any characters including `/`. the most specific (fewest wildcards) matching pattern wins.

a trailing ` *` or ` **` in a pattern also matches the command with no arguments (e.g. `"ls *"` matches both `ls` and `ls -la /home`).

when a bash command contains shell metacharacters (pipes, redirects, chaining, etc.), it cannot be safely matched against patterns, so the catch-all (`*`) permission level is used instead.

### hooks

hooks are external executables that extend hrns without recompiling. they live under `hooks/` in the same base directories as agents and skills (`.opencode/hooks/`, `.claude/hooks/`, `.agents/hooks/`, plus the global equivalents).

each script is invoked as `<script> <stage>`, with a single JSON object on stdin and JSONL on stdout. supported stages:

- `discover`: called once at startup to register the hook and its custom tools.
- `mutate_request`: returned `system` strings are appended to the agent's system prompt.
- `execute_tool`: called when the LLM invokes a hook-registered tool.
- `before_tool` / `after_tool`: observational, fired around every tool call.

```sh
#!/bin/sh
# .agents/hooks/persona.sh
case "$1" in
  discover)
    cat <<'EOF'
{"name":"persona","tools":[{"name":"trait","description":"read a persona trait","parameters":{"key":"trait name"}}]}
EOF
    ;;
  mutate_request)
    echo '{"system":["respond as a terse rust hacker."]}'
    ;;
  execute_tool)
    echo '{"result":"persona response"}'
    ;;
esac
```

the script must be executable. files starting with `.` or `__` are ignored. tool names are namespaced as `<prefix>_<short>` (here, `persona_trait`).

hrns implements the [harness control protocol](https://github.com/khimaros/hcp-spec/) with partial conformance (tier 0 and `before_stop` from tier 1). see [DESIGN.md](DESIGN.md) for the rationale.

## usage

```
hrns [OPTIONS] [AGENT_NAME] [PROMPT]
```

the prompt can be provided as a positional argument after the agent name, via `-p`/`--prompt`, or piped through stdin.

### options

| flag | description |
|------|-------------|
| `--init` | initialize a default configuration file |
| `-p, --prompt <PROMPT>` | prompt text (alternative to positional arg or stdin) |
| `-s, --system-prompt <PROMPT>` | override the system prompt |
| `-m, --model <MODEL>` | override model (`<provider>/<model>`) |
| `-t, --max-tokens <N>` | maximum output tokens (default: 16384) |
| `--tools <LIST>` | enable specific tools exclusively (comma-separated, e.g. `read,bash`) |
| `--skills <LIST>` | use specific skills exclusively (comma-separated, overrides agent) |
| `--permissions-allow <TOOL[:PATTERN]>` | override permission to `allow` (repeatable, e.g. `'bash:apt update'`) |
| `--permissions-ask <TOOL[:PATTERN]>` | override permission to `ask` (repeatable) |
| `--permissions-deny <TOOL[:PATTERN]>` | override permission to `deny` (repeatable) |
| `-n, --dry-run` | print a human-readable summary of what would be sent and exit |
| `-D, --dump-request` | dump the exact JSON request body (OpenAI chat-completions shape) and exit |
| `-q, --quiet` | suppress thinking and tool call output on stderr |
| `-y, --yes` | auto-accept "ask" permission prompts |
| `-v, --verbose` | enable verbose/debug logging |
| `--list-agents` | list discovered agents |
| `--list-skills` | list discovered skills |
| `--list-tools` | list available tools |
| `--list-providers` | list configured providers |
| `--list-hooks` | list discovered hook scripts and their registered tools |
| `--list-permissions` | print resolved permissions (after merging config + agent + `--permissions`) |

the response stream is written to stdout. reasoning tokens (from compatible models) are written to stderr in dim italic. tool calls and results are logged to stderr.

## license

GPLv3
