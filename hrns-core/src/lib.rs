//! reusable core of hrns: config, glob matching, the permission model, and
//! agent/skill/markdown resolution, extracted from the CLI so hmux can drive
//! the same logic in-process (see ../hmux ROADMAP phase 34). the CLI is one
//! consumer; the core is HCP-agnostic and tty-agnostic (the "ask" permission
//! decision is a caller-supplied [`PermissionResolver`]).

pub mod agent;
pub mod config;
pub mod glob;
pub mod permission;
pub mod resolve;
pub mod tools;

pub use agent::{history_texts, run, Attachment, Observer, RunConfig, RunOutcome};
// the cancel handle a consumer triggers to abort an in-flight turn; re-exported
// so the backend needs no direct tokio-util dependency.
pub use config::{
    load_config, resolve_provider, Config, HooksConfig, ProviderConfig, Resolved, SkillsConfig,
    ToolsConfig,
};
pub use glob::{glob_lookup, glob_matches};
pub use permission::{
    check_tool_permission, parse_permission_target, Permission, PermissionLevel,
    PermissionOverride, PermissionResolver, PermissionsConfig,
};
pub use resolve::{
    append_skills, build_system_prompt, find_all_in_dirs, load_agent, load_skill,
    parse_markdown_with_frontmatter, AgentFrontmatter, Kind, ParsedDoc, SkillFrontmatter,
};
pub use tokio_util::sync::CancellationToken;
pub use tools::{bash_tool_definition, read_tool_definition, BeforeTool, ToolMiddleware};

// re-exported so a consumer (the hmux backend) can implement `ToolMiddleware`
// without taking a direct rig dependency; it is the tool type in `extra_tools`.
pub use rig::tool::ToolDyn;

// the opaque conversation-history message a consumer stores per session and
// threads back into `run` for a follow-up turn. re-exported so the backend needs
// no direct rig dependency; it never inspects it.
pub use rig::message::Message as ChatMessage;
