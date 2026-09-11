mod hooks;

use async_trait::async_trait;
use clap::{CommandFactory, Parser};
use rig::completion::ToolDefinition;
use rig::tool::ToolDyn;
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use hooks::HookManager;
use hrns_core::{
    append_skills, bash_tool_definition, build_system_prompt, find_all_in_dirs, load_agent,
    load_config, parse_markdown_with_frontmatter, parse_permission_target, read_tool_definition,
    AgentFrontmatter, BeforeTool, CancellationToken, Kind, Observer, Permission, PermissionLevel,
    PermissionOverride, PermissionResolver, PermissionsConfig, RunConfig, SkillFrontmatter,
    ToolMiddleware, ToolsConfig,
};
use tracing_subscriber::EnvFilter;

/// when true, "ask" permissions are auto-accepted without prompting.
static AUTO_ACCEPT: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);

/// max bytes of tool-call output rendered to stderr; full content is spooled
/// to a per-process file under the cache dir when this limit is exceeded.
const DEFAULT_TOOL_OUTPUT_TRUNCATE: usize = 2000;
static TOOL_OUTPUT_TRUNCATE: AtomicUsize = AtomicUsize::new(DEFAULT_TOOL_OUTPUT_TRUNCATE);

/// monotonic counter for naming spooled tool-output files this process.
static SPOOL_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// returns the cache dir for spooled output: $XDG_CACHE_HOME/hrns/<pid>/
/// (falls back to $HOME/.cache, then /tmp).
fn spool_dir() -> PathBuf {
    let base = env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    base.join("hrns").join(process::id().to_string())
}

/// writes `content` to a fresh file under the spool dir, returns its path
/// (or None on failure; caller should degrade silently).
fn write_spool_file(content: &str) -> Option<PathBuf> {
    let dir = spool_dir();
    fs::create_dir_all(&dir).ok()?;
    let n = SPOOL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}.txt", n));
    fs::write(&path, content).ok()?;
    Some(path)
}

fn prompt_user_confirmation(tool_name: &str, input: &str) -> bool {
    if AUTO_ACCEPT.load(Ordering::Relaxed) {
        eprintln!("auto-accepting: {} \"{}\"", tool_name, input);
        return true;
    }
    let tty = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(f) => f,
        Err(_) => {
            eprintln!(
                "cannot open /dev/tty for confirmation, denying: {} {}",
                tool_name, input
            );
            return false;
        }
    };
    let mut reader = io::BufReader::new(&tty);
    let mut writer = &tty;
    let _ = write!(writer, "allow {} \"{}\"? [y/N] ", tool_name, input);
    let _ = writer.flush();
    let mut response = String::new();
    if reader.read_line(&mut response).is_err() {
        return false;
    }
    matches!(response.trim(), "y" | "Y" | "yes" | "YES")
}

fn tool_def_as_function(def: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": def.name,
            "description": def.description,
            "parameters": def.parameters,
        }
    })
}

const DEFAULT_MAX_TOKENS: u64 = 16384;
const DEFAULT_MAX_TURNS: usize = 32;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// the name of the agent to run (optional)
    agent_name: Option<String>,

    /// prompt to send (positional alternative to -p)
    #[arg(value_name = "PROMPT")]
    prompt_positional: Option<String>,

    /// initialize a default configuration file
    #[arg(long)]
    init: bool,

    /// list available agents
    #[arg(long)]
    list_agents: bool,

    /// list available skills
    #[arg(long)]
    list_skills: bool,

    /// list available tools
    #[arg(long)]
    list_tools: bool,

    /// list configured providers
    #[arg(long)]
    list_providers: bool,

    /// list discovered hook scripts and their registered tools
    #[arg(long)]
    list_hooks: bool,

    /// list resolved permissions (after merging config + agent + --permissions)
    #[arg(long)]
    list_permissions: bool,

    /// allow a tool or tool:pattern (repeatable, e.g. --permissions-allow 'bash:apt update')
    #[arg(long = "permissions-allow", value_name = "TOOL[:PATTERN]", value_parser = parse_permission_target)]
    permissions_allow: Vec<(String, Option<String>)>,

    /// require an "ask" prompt for a tool or tool:pattern (repeatable)
    #[arg(long = "permissions-ask", value_name = "TOOL[:PATTERN]", value_parser = parse_permission_target)]
    permissions_ask: Vec<(String, Option<String>)>,

    /// deny a tool or tool:pattern (repeatable)
    #[arg(long = "permissions-deny", value_name = "TOOL[:PATTERN]", value_parser = parse_permission_target)]
    permissions_deny: Vec<(String, Option<String>)>,

    /// prompt to send to the agent (if not provided via stdin)
    #[arg(short, long)]
    prompt: Option<String>,

    /// override the system prompt
    #[arg(short = 's', long)]
    system_prompt: Option<String>,

    /// override the model to use (format: <provider_name>/<model_name>)
    #[arg(short, long)]
    model: Option<String>,

    /// maximum output tokens (default: 16384)
    #[arg(short = 't', long)]
    max_tokens: Option<u64>,

    /// exclusive list of enabled tools (comma-separated, e.g. --tools read,bash)
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// exclusive list of skills to attach (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skills: Option<Vec<String>>,

    /// exclusive list of enabled hooks (comma-separated, by hook name)
    #[arg(long, value_delimiter = ',')]
    hooks: Option<Vec<String>>,

    /// print a human-readable summary of what would be sent and exit
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// dump the exact JSON request body (OpenAI chat-completions shape)
    /// that would be sent to the LLM, then exit. tool definitions and
    /// system/user messages are included verbatim.
    #[arg(short = 'D', long)]
    dump_request: bool,

    /// auto-accept "ask" permission prompts
    #[arg(short = 'y', long)]
    yes: bool,

    /// suppress thinking and tool call output on stderr
    #[arg(short, long)]
    quiet: bool,

    /// enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

/// prints rows as a padded table with 2-space column gaps.
/// shortens a filesystem path for display: relative to cwd when the
/// path is under it, else `~/...` when under $HOME, else the absolute
/// path verbatim.
fn shorten_path(path: &Path) -> String {
    if let Ok(cwd) = env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

fn print_table(header: Option<&[&str]>, rows: &[Vec<&str>]) {
    if rows.is_empty() && header.is_none() {
        return;
    }
    let cols = header
        .map(|h| h.len())
        .into_iter()
        .chain(rows.iter().map(|r| r.len()))
        .max()
        .unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            let header_w = header.and_then(|h| h.get(c)).map_or(0, |s| s.len());
            let rows_w = rows
                .iter()
                .map(|r| r.get(c).map_or(0, |s| s.len()))
                .max()
                .unwrap_or(0);
            header_w.max(rows_w)
        })
        .collect();
    let print_row = |row: &[&str]| {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, val)| {
                if i + 1 < row.len() {
                    format!("{:<width$}", val, width = widths[i])
                } else {
                    val.to_string()
                }
            })
            .collect();
        println!("{}", line.join("  "));
    };
    if let Some(h) = header {
        print_row(h);
        let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        let sep_refs: Vec<&str> = sep.iter().map(|s| s.as_str()).collect();
        print_row(&sep_refs);
    }
    for row in rows {
        print_row(row);
    }
}

fn permission_level_str(l: &PermissionLevel) -> &'static str {
    match l {
        PermissionLevel::Allow => "allow",
        PermissionLevel::Ask => "ask",
        PermissionLevel::Deny => "deny",
    }
}

/// counts wildcards in a glob pattern; used to sort patterns by specificity
/// for display (most specific first), matching the runtime tie-breaking.
fn pattern_wildcards(p: &str) -> usize {
    p.matches('*').count() + p.matches('?').count()
}

fn print_permissions(perms: &PermissionsConfig) {
    let mut tools: Vec<&String> = perms.tools.keys().collect();
    tools.sort();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for tool in tools {
        match &perms.tools[tool] {
            Permission::Level(lvl) => {
                rows.push(vec![
                    tool.clone(),
                    String::new(),
                    permission_level_str(lvl).to_string(),
                ]);
            }
            Permission::Patterns(map) => {
                let mut entries: Vec<(&String, &PermissionLevel)> = map.iter().collect();
                // most specific first (fewest wildcards, then longer pattern,
                // then alphabetical for stable output)
                entries.sort_by(|a, b| {
                    pattern_wildcards(a.0)
                        .cmp(&pattern_wildcards(b.0))
                        .then(b.0.len().cmp(&a.0.len()))
                        .then(a.0.cmp(b.0))
                });
                for (i, (pat, lvl)) in entries.iter().enumerate() {
                    let name = if i == 0 { tool.clone() } else { String::new() };
                    rows.push(vec![
                        name,
                        (*pat).clone(),
                        permission_level_str(lvl).to_string(),
                    ]);
                }
            }
        }
    }
    let row_refs: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.iter().map(|s| s.as_str()).collect())
        .collect();
    print_table(Some(&["tool", "pattern", "level"]), &row_refs);
}

fn init_config() -> Result<(), Box<dyn std::error::Error>> {
    let home = env::var("HOME").map_err(|_| "HOME environment variable not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("hrns");

    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
        println!("created directory: {}", config_dir.display());
    }

    let config_path = config_dir.join("config.toml");

    if config_path.exists() {
        eprintln!(
            "configuration file already exists at: {}",
            config_path.display()
        );
        return Ok(());
    }

    let default_config = include_str!("../hrns.example.toml");

    fs::write(&config_path, default_config)?;
    println!("initialized configuration at: {}", config_path.display());
    Ok(())
}

fn get_user_prompt(args: &Args) -> Result<String, Box<dyn std::error::Error>> {
    let mut user_prompt = args
        .prompt
        .clone()
        .or_else(|| args.prompt_positional.clone())
        .unwrap_or_default();
    if user_prompt.is_empty() && !io::stdin().is_terminal() {
        io::stdin().read_to_string(&mut user_prompt)?;
    }

    if user_prompt.trim().is_empty() {
        return Err("one of --prompt or stdin must be non-empty".into());
    }

    Ok(user_prompt)
}

/// the CLI's permission resolver: prompts on /dev/tty via
/// `prompt_user_confirmation` (bypassable with --yes).
struct TtyResolver;

#[async_trait]
impl PermissionResolver for TtyResolver {
    async fn confirm(&self, tool_name: &str, input: &str) -> bool {
        prompt_user_confirmation(tool_name, input)
    }
}

/// the CLI's stream renderer: assistant text to stdout, reasoning + tool
/// activity to stderr (suppressed by --quiet), reproducing hrns's output.
struct StdioObserver {
    in_reasoning: bool,
}

impl StdioObserver {
    fn new() -> Self {
        Self {
            in_reasoning: false,
        }
    }

    /// closes an open reasoning run with a newline before other output.
    fn end_reasoning(&mut self) {
        if self.in_reasoning {
            let mut stderr = io::stderr();
            let _ = stderr.write_all(b"\n");
            let _ = stderr.flush();
            self.in_reasoning = false;
        }
    }
}

impl Observer for StdioObserver {
    fn text(&mut self, delta: &str) {
        self.end_reasoning();
        let mut stdout = io::stdout();
        let _ = stdout.write_all(delta.as_bytes());
        let _ = stdout.flush();
    }

    fn reasoning(&mut self, delta: &str) {
        if QUIET.load(Ordering::Relaxed) {
            return;
        }
        let mut stderr = io::stderr();
        let _ = stderr.write_all(b"\x1b[2;3m");
        let _ = stderr.write_all(delta.as_bytes());
        let _ = stderr.write_all(b"\x1b[0m");
        let _ = stderr.flush();
        self.in_reasoning = true;
    }

    fn tool_call(&mut self, name: &str, args: &str) {
        self.end_reasoning();
        if QUIET.load(Ordering::Relaxed) {
            return;
        }
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "\x1b[1;36m{}\x1b[0m\x1b[2m({})\x1b[0m", name, args);
        let _ = stderr.flush();
    }

    fn tool_result(&mut self, result: &str) {
        self.end_reasoning();
        if QUIET.load(Ordering::Relaxed) {
            return;
        }
        let mut stderr = io::stderr();
        let limit = TOOL_OUTPUT_TRUNCATE.load(Ordering::Relaxed);
        if limit > 0 && result.len() > limit {
            // slice on a char boundary <= limit
            let mut cut = limit.min(result.len());
            while !result.is_char_boundary(cut) {
                cut -= 1;
            }
            let head = &result[..cut];
            let total = result.len();
            let spool = write_spool_file(result);
            let _ = writeln!(stderr, "\x1b[2m  -> {}\x1b[0m", head.replace('\n', "\\n"));
            match spool {
                Some(p) => {
                    let _ = writeln!(stderr,
                        "\x1b[33m  ! truncated tool output: {} of {} bytes shown. full content at {} (read with: cat {})\x1b[0m",
                        cut, total, p.display(), p.display());
                }
                None => {
                    let _ = writeln!(stderr,
                        "\x1b[33m  ! truncated tool output: {} of {} bytes shown (failed to spool full content)\x1b[0m",
                        cut, total);
                }
            }
        } else {
            let _ = writeln!(stderr, "\x1b[2m  -> {}\x1b[0m", result.replace('\n', "\\n"));
        }
        let _ = stderr.flush();
    }

    fn error(&mut self, message: &str) {
        eprintln!("stream error: {}", message);
    }

    fn restart(&mut self) {
        // the CLI streams continuously; just close any open reasoning run.
        self.end_reasoning();
    }

    fn end_stream(&mut self) {
        self.end_reasoning();
    }
}

/// bridges hrns's HCP host (src/hooks.rs) onto the core's tool-middleware seam:
/// per-tool before/after interception and the gated hook-registered tool set.
struct HcpMiddleware {
    hooks: Arc<HookManager>,
    permissions: PermissionsConfig,
    resolver: Arc<dyn PermissionResolver>,
    tools: ToolsConfig,
    tools_override: Option<Vec<String>>,
}

#[async_trait]
impl ToolMiddleware for HcpMiddleware {
    async fn before_tool(&self, tool: &str, call_id: &str, args: &serde_json::Value) -> BeforeTool {
        let r = self.hooks.before_tool(tool, call_id, args);
        // the v2 HCP host has no args-rewrite hook, so tool_input passes through unchanged.
        BeforeTool {
            deny: r.deny,
            result: r.result,
            args: None,
        }
    }

    async fn after_tool(&self, tool: &str, call_id: &str, output: &str) -> Option<String> {
        self.hooks.after_tool(tool, call_id, output).result
    }

    fn extra_tools(&self) -> Vec<Box<dyn ToolDyn>> {
        // hook tools are gated by [tools] on their full_name (e.g. datetime_now)
        // in addition to the [hooks] gate that decided which scripts to load.
        self.hooks
            .into_dyn_tools(&self.permissions, &self.resolver)
            .into_iter()
            .filter(|t| {
                self.tools
                    .is_active(&ToolDyn::name(t.as_ref()), &self.tools_override)
            })
            .collect()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.yes {
        AUTO_ACCEPT.store(true, Ordering::Relaxed);
    }
    if args.quiet {
        QUIET.store(true, Ordering::Relaxed);
    }

    if args.init {
        if let Err(e) = init_config() {
            eprintln!("error initializing config: {}", e);
            process::exit(1);
        }
        process::exit(0);
    }

    let config = load_config()?;

    if let Some(limit) = config.tool_output_truncate {
        TOOL_OUTPUT_TRUNCATE.store(limit, Ordering::Relaxed);
    }

    if args.list_agents {
        let items: Vec<(String, String)> = find_all_in_dirs(Kind::Agent)
            .into_iter()
            .map(|(name, path)| {
                let desc = fs::read_to_string(&path)
                    .ok()
                    .and_then(|c| parse_markdown_with_frontmatter::<AgentFrontmatter>(&c).ok())
                    .and_then(|doc| doc.frontmatter.description)
                    .unwrap_or_default();
                (name, desc)
            })
            .collect();
        let rows: Vec<Vec<&str>> = items
            .iter()
            .map(|(n, d)| vec![n.as_str(), d.as_str()])
            .collect();
        print_table(Some(&["name", "description"]), &rows);
        process::exit(0);
    }

    if args.list_skills {
        let items: Vec<(String, String, &'static str)> = find_all_in_dirs(Kind::Skill)
            .into_iter()
            .map(|(name, path)| {
                let desc = fs::read_to_string(&path)
                    .ok()
                    .and_then(|c| parse_markdown_with_frontmatter::<SkillFrontmatter>(&c).ok())
                    .and_then(|doc| doc.frontmatter.description)
                    .unwrap_or_default();
                let status = if config.skills.is_active(&name) {
                    "true"
                } else {
                    "false"
                };
                (name, desc, status)
            })
            .collect();
        let rows: Vec<Vec<&str>> = items
            .iter()
            .map(|(n, d, s)| vec![n.as_str(), d.as_str(), *s])
            .collect();
        print_table(Some(&["name", "description", "enabled"]), &rows);
        process::exit(0);
    }

    // discover all hooks first; the `[hooks]` config gate is applied
    // only to the runtime manager used during agent execution. list
    // commands show everything that's available, regardless of config,
    // matching how --list-tools and --list-skills behave.
    let all_hooks = HookManager::discover();

    if args.list_tools {
        let read_status = if config.tools.is_active("read", &args.tools) {
            "true"
        } else {
            "false"
        };
        let bash_status = if config.tools.is_active("bash", &args.tools) {
            "true"
        } else {
            "false"
        };
        let mut rows: Vec<Vec<String>> = vec![
            vec![
                "read".into(),
                "read the contents of a file".into(),
                read_status.into(),
            ],
            vec![
                "bash".into(),
                "execute a bash command".into(),
                bash_status.into(),
            ],
        ];
        for script in all_hooks.scripts() {
            let hook_active = config.hooks.is_active(&script.name, &args.hooks);
            for tool in &script.tools {
                let active = hook_active && config.tools.is_active(&tool.full_name, &args.tools);
                let status = if active { "true" } else { "false" };
                rows.push(vec![
                    tool.full_name.clone(),
                    tool.description.clone(),
                    status.into(),
                ]);
            }
        }
        let row_refs: Vec<Vec<&str>> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.as_str()).collect())
            .collect();
        print_table(Some(&["name", "description", "enabled"]), &row_refs);
        process::exit(0);
    }

    if args.list_hooks {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for script in all_hooks.scripts() {
            let status = if config.hooks.is_active(&script.name, &args.hooks) {
                "true"
            } else {
                "false"
            };
            rows.push(vec![
                script.name.clone(),
                shorten_path(&script.path),
                script
                    .tools
                    .iter()
                    .map(|t| t.full_name.clone())
                    .collect::<Vec<_>>()
                    .join(","),
                status.into(),
            ]);
        }
        let row_refs: Vec<Vec<&str>> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.as_str()).collect())
            .collect();
        print_table(Some(&["name", "path", "tools", "enabled"]), &row_refs);
        process::exit(0);
    }

    let hooks_config = config.hooks.clone();
    let hooks_override = args.hooks.clone();
    let hooks = Arc::new(all_hooks.retain(|name| hooks_config.is_active(name, &hooks_override)));

    if args.list_providers {
        let items: Vec<(String, String, String)> = config
            .providers
            .iter()
            .map(|p| {
                let client = p.client.as_deref().unwrap_or(&p.name).to_string();
                let url = p.base_url.as_deref().unwrap_or("-").to_string();
                (p.name.clone(), client, url)
            })
            .collect();
        let rows: Vec<Vec<&str>> = items
            .iter()
            .map(|(n, c, u)| vec![n.as_str(), c.as_str(), u.as_str()])
            .collect();
        print_table(Some(&["name", "client", "base_url"]), &rows);
        process::exit(0);
    }

    let default_sp = config.default_system_prompt.clone().unwrap_or_default();
    let (mut system_prompt, agent_model, agent_tools, agent_permissions) =
        if let Some(agent_name) = &args.agent_name {
            let agent = load_agent(agent_name)?;
            if let Some(ref override_prompt) = args.system_prompt {
                // -s overrides the entire system prompt (no agent body or skills)
                (
                    override_prompt.clone(),
                    agent.frontmatter.model,
                    agent.frontmatter.tools,
                    agent.frontmatter.permissions,
                )
            } else if let Some(skills) = &args.skills {
                // --skills overrides agent skills exclusively
                let mut prompt = agent.body.clone();
                append_skills(&mut prompt, skills, &config.skills);
                (
                    prompt,
                    agent.frontmatter.model,
                    agent.frontmatter.tools,
                    agent.frontmatter.permissions,
                )
            } else {
                let prompt = build_system_prompt(&agent, &config.skills);
                (
                    prompt,
                    agent.frontmatter.model,
                    agent.frontmatter.tools,
                    agent.frontmatter.permissions,
                )
            }
        } else {
            let mut prompt = args.system_prompt.clone().unwrap_or_default();
            if let Some(ref skill_list) = args.skills {
                append_skills(&mut prompt, skill_list, &config.skills);
            }
            (
                prompt,
                None,
                ToolsConfig::default(),
                PermissionsConfig::default(),
            )
        };

    // fall back to config.default_system_prompt when nothing else set one
    if system_prompt.is_empty() && !default_sp.is_empty() {
        system_prompt = default_sp;
    }

    // agent frontmatter overrides config; --permissions-* override both.
    // applied in order: allow, ask, deny, so within a single invocation,
    // deny wins over ask wins over allow for the same target.
    let tools = config.tools.clone().merge(agent_tools);
    let mut perm_overrides: Vec<PermissionOverride> = Vec::new();
    for (tool, pattern) in &args.permissions_allow {
        perm_overrides.push(PermissionOverride {
            tool: tool.clone(),
            pattern: pattern.clone(),
            level: PermissionLevel::Allow,
        });
    }
    for (tool, pattern) in &args.permissions_ask {
        perm_overrides.push(PermissionOverride {
            tool: tool.clone(),
            pattern: pattern.clone(),
            level: PermissionLevel::Ask,
        });
    }
    for (tool, pattern) in &args.permissions_deny {
        perm_overrides.push(PermissionOverride {
            tool: tool.clone(),
            pattern: pattern.clone(),
            level: PermissionLevel::Deny,
        });
    }
    let permissions = config
        .permissions
        .clone()
        .merge(agent_permissions)
        .apply_overrides(&perm_overrides);

    if args.list_permissions {
        print_permissions(&permissions);
        process::exit(0);
    }

    let user_prompt = match get_user_prompt(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}\n", e);
            let mut cmd = Args::command();
            cmd.print_help().ok();
            println!();
            process::exit(1);
        }
    };

    let full_model_name = args
        .model
        .clone()
        .or(agent_model)
        .or(config.default_model.clone())
        .unwrap_or_else(|| "openai/gpt-4o".to_string());

    // hook protocol: append `mutate_request` system prompt fragments
    // (joined with blank lines) to whatever the agent already composed.
    // hooks see the finalized system + user prompts and model id in the
    // payload so observational hooks can log them.
    for fragment in hooks.mutate_request(&system_prompt, &user_prompt, &full_model_name) {
        if !system_prompt.is_empty() {
            system_prompt.push_str("\n\n");
        }
        system_prompt.push_str(&fragment);
    }

    // markdown agent bodies and skills routinely leave trailing/leading
    // blank lines from the frontmatter split; strip them so the prompt
    // sent to the model is tidy.
    let system_prompt = system_prompt.trim().to_string();

    // no preflight on api_key: providers that don't require auth (e.g. local
    // openai-compatible servers) work keyless; hosted providers return a clear
    // auth error from the upstream API at request time.
    let hrns_core::Resolved {
        provider_name,
        client_type,
        model_name,
        api_key,
        base_url,
    } = hrns_core::resolve_provider(&config, &full_model_name);

    if args.verbose {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rig=debug")),
            )
            .with_writer(io::stderr)
            .init();
        eprintln!("debug: using client type '{}'", client_type);
        eprintln!("debug: using model '{}'", model_name);
        if let Some(ref url) = base_url {
            eprintln!("debug: using base url '{}'", url);
        }
        eprintln!("debug: tools {:?}", tools);
        eprintln!("debug: permissions {:?}", permissions);
    }

    let max_tokens = args
        .max_tokens
        .or(config.default_max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let max_turns = config.default_max_turns.unwrap_or(DEFAULT_MAX_TURNS);

    if args.dry_run {
        println!("--- model ---");
        println!("{}/{}", provider_name, model_name);
        println!("max_tokens: {}", max_tokens);
        println!("max_turns: {}", max_turns);

        let mut active_tools: Vec<String> = ["read", "bash"]
            .iter()
            .filter(|t| tools.is_active(t, &args.tools))
            .map(|s| s.to_string())
            .collect();
        for script in hooks.scripts() {
            for tool in &script.tools {
                active_tools.push(tool.full_name.clone());
            }
        }
        println!("\n--- tools ---");
        if active_tools.is_empty() {
            println!("(none)");
        } else {
            for name in &active_tools {
                println!("{}", name);
            }
        }

        println!("\n--- permissions ---");
        for name in &active_tools {
            match permissions.tools.get(name) {
                Some(perm) => println!("{}: {:?}", name, perm),
                None => println!("{}: (unset, defaults to deny)", name),
            }
        }

        if !hooks.scripts().is_empty() {
            println!("\n--- hooks ---");
            for script in hooks.scripts() {
                let tool_list: Vec<&str> =
                    script.tools.iter().map(|t| t.full_name.as_str()).collect();
                println!(
                    "{}  {}  [{}]",
                    script.name,
                    script.path.display(),
                    tool_list.join(",")
                );
            }
        }

        println!("\n--- system prompt ---");
        println!("{}", system_prompt);

        println!("\n--- user prompt ---");
        println!("{}", user_prompt);

        return Ok(());
    }

    if args.dump_request {
        let mut tool_defs: Vec<serde_json::Value> = Vec::new();
        if tools.is_active("read", &args.tools) {
            tool_defs.push(tool_def_as_function(&read_tool_definition()));
        }
        if tools.is_active("bash", &args.tools) {
            tool_defs.push(tool_def_as_function(&bash_tool_definition()));
        }
        for script in hooks.scripts() {
            for t in &script.tools {
                if !tools.is_active(&t.full_name, &args.tools) {
                    continue;
                }
                tool_defs.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.full_name,
                        "description": t.description,
                        "parameters": t.parameters_schema,
                    }
                }));
            }
        }
        let mut messages = Vec::new();
        if !system_prompt.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": system_prompt}));
        }
        messages.push(serde_json::json!({"role": "user", "content": user_prompt}));
        let mut request = serde_json::json!({
            "model": model_name,
            "max_tokens": max_tokens,
            "messages": messages,
        });
        if !tool_defs.is_empty() {
            request["tools"] = serde_json::Value::Array(tool_defs);
        }
        println!("{}", serde_json::to_string_pretty(&request)?);
        return Ok(());
    }

    let resolver: Arc<dyn PermissionResolver> = Arc::new(TtyResolver);
    let middleware: Arc<dyn ToolMiddleware> = Arc::new(HcpMiddleware {
        hooks: hooks.clone(),
        permissions: permissions.clone(),
        resolver: resolver.clone(),
        tools: tools.clone(),
        tools_override: args.tools.clone(),
    });
    let mut observer = StdioObserver::new();
    // the CLI is one-shot: no prior history in, the returned history unused, and no
    // steering (the steer channel closes at once so its select branch stays disabled).
    let (_, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let outcome = hrns_core::run(
        RunConfig {
            client_type: &client_type,
            model_name: &model_name,
            api_key: &api_key,
            base_url: base_url.as_deref(),
            max_tokens,
            max_turns,
            tools: &tools,
            tools_override: &args.tools,
            permissions: &permissions,
            system_prompt: &system_prompt,
            user_prompt: &user_prompt,
            attachments: &[],
            additional_params: None,
        },
        resolver,
        middleware,
        &mut observer,
        Vec::new(),
        // the CLI runs one turn to completion; a never-cancelled token.
        CancellationToken::new(),
        steer_rx,
    )
    .await?;

    // tier 1: notify hooks that the loop terminated. observational only;
    // `continue` responses are accepted in the wire format but not honored, so
    // `final` is always true.
    hooks.before_stop(outcome.exit_reason, outcome.exit_error.as_deref());
    println!();
    Ok(())
}
