# roadmap

- [x] implement toml configuration parsing
- [x] implement markdown agent/skill parsing from `.claude/` and `.opencode/`
- [x] implement openai api streaming client
- [x] implement cli main loop (stdin, api request, stdout/stderr streams)
- [x] add unified client type config (openai/openai_responses/anthropic/etc)
- [x] add tracing support for verbose logging
- [x] add bash tool with wildcard permission matching
- [x] consolidate permission checking across tools
- [x] add --system-prompt (-s) flag for overriding the system prompt
- [x] gitignore-style glob patterns for permissions (*, **, ?)
- [x] implement "ask" permission with /dev/tty user confirmation
- [x] bash metacharacter fallback to catch-all permission
- [x] --yes flag for auto-accepting ask prompts
- [x] --tools takes comma-separated list (exclusive override)
- [x] --skills flag for exclusive skill override
- [x] --list-agents, --list-skills, --list-tools, --list-providers (with descriptions)
- [x] path mode vs command mode for glob matching
- [x] --dry-run flag to inspect prompts without calling LLM
- [x] --skills flag for exclusive skill override
- [x] show usage on empty prompt (exit non-zero)
- [x] fix find_all_in_dirs double-nesting base directories for global paths
- [x] hook protocol (partial conformance, see the [hcp-spec](https://github.com/khimaros/hcp-spec/) and DESIGN.md):
  - [x] discover hooks from `hooks/` under existing base dirs (.claude/.opencode/.agents, plus globals)
  - [x] `discover` stage at startup: register hook tools (`<prefix>_<tool_name>`)
  - [x] `mutate_request` stage: merge returned `system` strings into the system prompt; payload includes finalized system/user/model/tools
  - [x] `execute_tool` stage: route invocations of hook-registered tools back to the owning script
  - [x] `before_tool` / `after_tool` stages around built-in `read` / `bash` and hook-registered tools, with `deny` / `args` / `result` mutation responses
  - [x] `before_stop` post-loop observational stage with `exit_reason` / `final` / `error`
  - [x] honor `permission.arg` for hook tools (route through existing `check_tool_permission`)
  - [x] `--list-hooks` flag
  - [x] hook tools appear in `--list-tools`
  - [x] typed parameters: validate args (required, type, enum) before `execute_tool`; normalize `any` type
  - [x] dry-run inclusion of hooks (own section + tools/permissions list)
  - [x] host capability payload (`{name, version, stages}`) on every hook invocation
- [ ] hook protocol, tier 1/2 follow-ups:
  - [ ] `before_stop.continue` re-entry into the agent loop (wire format honored; needs plumbing user-prompt re-injection back into `run_agent_stream`)
  - [ ] `max_turns` distinction in `before_stop.exit_reason` (today everything reports `"stop"` or `"error"`; needs rig surfacing the loop-exit cause)
  - [ ] `before_turn` / `after_turn` stages
  - [ ] `on_error` / `on_permission` stages
- [x] `--dump-request` (`-D`) flag emits exact OpenAI chat-completions JSON request body
- [x] `make precommit` target (lint + test + build)
- [x] configurable tool-call output truncation (stderr display); full content spooled to `$XDG_CACHE_HOME/hrns/<pid>/<seq>.txt` with a clear warning
- [x] `read` tool: optional `offset` (0-indexed line) and `count` (number of lines) parameters
- [x] config `default_system_prompt` used when no `-s` and no agent body

## hrns / hmux backend lift

lift hrns's agent loop into a reusable `hrns-core` library so hmux can drive it in-process as its
most capable backend (../hmux ROADMAP phase 34). the CLI keeps working as a second consumer; no
behavior change. the airun -> hrns rename was a SEPARATE atomic step done at the hmux 34e cutover -
the repo/crate/config/binary names all became `hrns` (a hard cutover, no old-path fallback).

- [x] convert to a cargo workspace: `hrns-core` (lib) + `hrns` (bin, a root package that is also
      the workspace). the bin keeps its src/ + tests/ + example toml in place. `make` targets now run
      `--workspace` so the core crate's tests + clippy run too.
- [x] move the HCP-agnostic, tty-agnostic core into the lib: config (config.rs), glob (glob.rs), the
      permission model (permission.rs), agent/skill/markdown resolution (resolve.rs), the built-in
      read/bash tools (tools.rs), and the rig agent loop (agent.rs) -- with all their unit tests. a
      `catalog()` of providers+models is deferred to hmux 34b (its shape follows the hmux manifest;
      hrns's config carries no per-provider model list).
- [x] core stays HCP-AGNOSTIC: the HCP host (src/hooks.rs, the v3 hcp-spec host) stays BIN-SIDE and
      imports the permission bits from `hrns_core`. hmux hosts HCP at the hub layer (its hcp face),
      so a core-embedded host would double-host an hmux-driven session.
- [x] seam `Observer` (agent.rs): the loop reports text/reasoning/tool events through it instead of
      printing; the CLI's `StdioObserver` reproduces the stdout/stderr rendering (incl. spool +
      truncation) verbatim. hmux maps each call to a normalized hub event.
- [x] seam `PermissionResolver`: the "ask" branch of check_tool_permission takes a `&dyn
      PermissionResolver`; the CLI supplies `TtyResolver` (/dev/tty + --yes). SYNC for now (the CLI
      blocks on tty, preserving behavior); async-ified when the hmux impl lands (34b).
- [x] seam `ToolMiddleware` (tools.rs): before/after-tool + the gated hook-registered tool set. the
      CLI's `HcpMiddleware` adapts src/hooks.rs onto it; hmux adapts its interceptor bridge. (system-
      prompt mutation stays a direct main->HCP `mutate_request`; before_stop stays a direct main->HCP
      call on the exit info `run` returns.)
- [x] seam: history in/out. `run` takes `history: Vec<ChatMessage>` in and returns
      `RunOutcome { exit_reason, exit_error, history }`, using rig `stream_chat(prompt, &history)` +
      `FinalResponse.history()` to accumulate; `ChatMessage` (= rig Message) + `RunOutcome` re-exported.
      the core stays sessionless; the hmux backend stores the Vec per session for follow-up turns.
- [x] the caller-supplied seams `PermissionResolver` + `ToolMiddleware` went ASYNC (`#[async_trait]`,
      new dep async-trait); `check_tool_permission` is async and the tools await it. the CLI's
      `TtyResolver`/`HcpMiddleware` are async but behavior-preserving (blocking tty/subprocess inside
      async, as before); the hmux `InterceptorResolver` awaits `interceptor.raise` for the native gate.
- [x] `resolve_provider` (a `<provider>/<model>` string + Config -> rig client wiring) added to
      hrns-core, shared by the CLI and the hmux backend (DRY).
- [ ] seam: a CancellationToken for abort + a steer channel (for hmux steering, 34c). still pending;
      best built alongside the hmux abort/steer work.
- [x] keep `make precommit` green: all rust tests pass -- 18 bin + 23 integration + 20 core = 61 --
      and 53/53 python conformance checks pass. the HCP host was upgraded v2 -> v3 (host.version=3 +
      the base payload carries the workspace `cwd`, echoed by hooks as `<hcp-cwd>`), closing the two
      previously-failing conformance checks.
