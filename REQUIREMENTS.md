# requirements

1. execute agents and skills from `.claude/` or `.opencode/` directory structure.
2. support markdown agent configs and skills.
3. read prompt from standard input (stdin).
4. write streaming response to standard output (stdout).
5. write reasoning tokens to standard error (stderr).
6. written in rust with minimal external dependencies.
7. support openai compatible endpoints, configurable via toml.
8. support external hook scripts via the shared [harness control protocol](https://github.com/khimaros/hcp-spec/)
   (subprocess + JSONL), covering at minimum `discover`, `mutate_request`,
   `execute_tool`, `before_tool`, `after_tool`, and `before_stop`.
   workspace-shaped session-bound stages (`heartbeat`, `compacting`,
   `recover`, `format_notification`, `observe_message`, `actions`) are
   explicitly out of scope; hrns declares partial conformance.
