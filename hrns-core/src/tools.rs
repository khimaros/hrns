//! the built-in `read` and `bash` tools, and the `ToolMiddleware` seam they
//! call for before/after-tool interception. the CLI backs the middleware with
//! its HCP host (src/hooks.rs); an embedding host (hmux) backs it with the hub
//! interceptor. the core owns no HCP.

use crate::config::ToolsConfig;
use crate::permission::{
    check_tool_permission, PermissionLevel, PermissionResolver, PermissionsConfig,
};
use async_trait::async_trait;
use rig::completion::ToolDefinition;
use rig::tool::{Tool, ToolDyn, ToolError};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// the mutation a `before_tool` interception can request. `args` REWRITES the
/// tool's arguments (tool_input) and the effective args -- not the model's
/// original -- are what the permission gate then evaluates. when `deny` is set it
/// wins (the call is refused); a `result` substitutes a synthetic output.
#[derive(Debug, Default)]
pub struct BeforeTool {
    pub deny: Option<String>,
    pub result: Option<String>,
    pub args: Option<serde_json::Value>,
}

/// per-tool interception around the built-in and hook-registered tools, plus
/// the extra (hook-registered) tools a consumer contributes to the loop. the
/// core stays HCP-agnostic: the CLI adapts its HCP host onto this; hmux adapts
/// the hub interceptor bridge.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// fired before a tool runs; may `deny` the call or substitute a `result`.
    async fn before_tool(&self, tool: &str, call_id: &str, args: &serde_json::Value) -> BeforeTool;
    /// fired after a tool runs; may replace the result text fed to the model.
    async fn after_tool(&self, tool: &str, call_id: &str, output: &str) -> Option<String>;
    /// extra model-callable tools the consumer contributes (already gated).
    fn extra_tools(&self) -> Vec<Box<dyn ToolDyn>>;
}

/// monotonic counter for `before_tool`/`after_tool` correlation ids.
static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_call_id() -> String {
    let n = CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("hrns-{}-{}", process::id(), n)
}

/// apply a `before_tool` args rewrite: re-parse the replacement value into the
/// tool's typed args, or keep the original when no rewrite was requested.
fn rewritten_args<T: serde::de::DeserializeOwned>(
    original: T,
    replacement: Option<serde_json::Value>,
) -> Result<T, ToolError> {
    match replacement {
        Some(v) => serde_json::from_value(v).map_err(|e| {
            ToolError::ToolCallError(format!("rewritten tool args invalid: {e}").into())
        }),
        None => Ok(original),
    }
}

fn resolve_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

// --- read tool ---

#[derive(Deserialize, Serialize)]
struct ReadArgs {
    path: String,
    /// 0-indexed line number to start reading from (default: 0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    /// number of lines to read (default: read to end)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
}

/// returns a slice of `content` covering `count` lines starting at the
/// 0-indexed `offset`. if both are None, returns the input unchanged.
fn slice_lines(content: &str, offset: Option<usize>, count: Option<usize>) -> String {
    if offset.is_none() && count.is_none() {
        return content.to_string();
    }
    let start = offset.unwrap_or(0);
    let lines: Vec<&str> = content.lines().collect();
    let end = match count {
        Some(n) => (start + n).min(lines.len()),
        None => lines.len(),
    };
    if start >= lines.len() {
        return String::new();
    }
    lines[start..end].join("\n")
}

pub fn read_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read".to_string(),
        description: "read the contents of a file".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "absolute or relative path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "0-indexed line number to start reading from (default: 0)",
                    "minimum": 0
                },
                "count": {
                    "type": "integer",
                    "description": "number of lines to read (default: read to end)",
                    "minimum": 1
                }
            },
            "required": ["path"]
        }),
    }
}

struct ReadTool {
    permissions: PermissionsConfig,
    resolver: Arc<dyn PermissionResolver>,
    middleware: Arc<dyn ToolMiddleware>,
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        read_tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let call_id = next_call_id();
        // tool_input interception runs first: a hook may deny, rewrite the args, or
        // synthesize a result. the permission gate then evaluates the EFFECTIVE args.
        let args_json = serde_json::to_value(&args).expect("serialize read args");
        let before = self
            .middleware
            .before_tool("read", &call_id, &args_json)
            .await;
        if let Some(reason) = before.deny {
            return Err(ToolError::ToolCallError(
                format!("denied by hook: {}", reason).into(),
            ));
        }
        let args = rewritten_args::<ReadArgs>(args, before.args)?;

        let resolved = resolve_path(&args.path);
        let path_str = resolved.to_string_lossy().into_owned();
        check_tool_permission(&self.permissions, &*self.resolver, "read", &path_str, true).await?;

        let result = if let Some(synthetic) = before.result {
            synthetic
        } else {
            let content = fs::read_to_string(&resolved)
                .map(|c| slice_lines(&c, args.offset, args.count))
                .map_err(|e| ToolError::ToolCallError(format!("{}: {}", path_str, e).into()))?;
            serde_json::to_string(&serde_json::json!({"content": content}))
                .expect("serialize read result")
        };
        let after = self.middleware.after_tool("read", &call_id, &result).await;
        Ok(after.unwrap_or(result))
    }
}

// --- bash tool ---

/// shell metacharacters that could chain or redirect commands.
const SHELL_METACHARACTERS: &[char] = &[
    ';', '|', '&', '`', '$', '(', ')', '{', '}', '<', '>', '\n', '\r', '!', '#',
];

/// returns true if the command is a simple command without shell
/// metacharacters that could bypass permission checks.
fn is_simple_bash_command(command: &str) -> bool {
    !command.contains('\\') && !command.chars().any(|c| SHELL_METACHARACTERS.contains(&c))
}

#[derive(Deserialize, Serialize)]
struct BashArgs {
    command: String,
}

pub fn bash_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "bash".to_string(),
        description: "execute a bash command and return its output".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "the bash command to execute. must be simple: no pipes, redirects, chaining, or shell metacharacters"
                }
            },
            "required": ["command"]
        }),
    }
}

struct BashTool {
    permissions: PermissionsConfig,
    resolver: Arc<dyn PermissionResolver>,
    middleware: Arc<dyn ToolMiddleware>,
}

impl Tool for BashTool {
    const NAME: &'static str = "bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        bash_tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let call_id = next_call_id();
        // tool_input interception runs first: a hook may deny, rewrite the command, or
        // synthesize a result; the permission gate then evaluates the EFFECTIVE command.
        let args_json = serde_json::to_value(&args).expect("serialize bash args");
        let before = self
            .middleware
            .before_tool("bash", &call_id, &args_json)
            .await;
        if let Some(reason) = before.deny {
            return Err(ToolError::ToolCallError(
                format!("denied by hook: {}", reason).into(),
            ));
        }
        let args = rewritten_args::<BashArgs>(args, before.args)?;

        if is_simple_bash_command(&args.command) {
            check_tool_permission(
                &self.permissions,
                &*self.resolver,
                "bash",
                &args.command,
                false,
            )
            .await?;
        } else {
            // command contains shell metacharacters, so we can't trust pattern
            // matching on the full string, so fall back to the catch-all rule.
            // use an empty string to only match wildcard patterns.
            let level = self.permissions.check("bash", "", false);
            match level {
                PermissionLevel::Allow => {}
                PermissionLevel::Ask => {
                    if !self.resolver.confirm("bash (complex)", &args.command).await {
                        return Err(ToolError::ToolCallError(
                            format!("permission denied (user rejected): {}", args.command).into(),
                        ));
                    }
                }
                PermissionLevel::Deny => {
                    return Err(ToolError::ToolCallError(
                        format!("permission denied (shell metacharacters): {}", args.command)
                            .into(),
                    ));
                }
            }
        }

        let result = if let Some(synthetic) = before.result {
            synthetic
        } else {
            let output = process::Command::new("sh")
                .arg("-c")
                .arg(&args.command)
                .output()
                .map_err(|e| {
                    ToolError::ToolCallError(format!("failed to execute: {}", e).into())
                })?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut obj = serde_json::Map::new();
            obj.insert(
                "exit_code".into(),
                serde_json::json!(output.status.code().unwrap_or(-1)),
            );
            obj.insert("stdout".into(), serde_json::json!(stdout));
            if !stderr.is_empty() {
                obj.insert("stderr".into(), serde_json::json!(stderr));
            }
            serde_json::to_string(&serde_json::Value::Object(obj)).expect("serialize bash result")
        };
        let after = self.middleware.after_tool("bash", &call_id, &result).await;
        Ok(after.unwrap_or(result))
    }
}

/// assembles the enabled built-in tools (gated by the `[tools]` config). the
/// hook-registered tools come separately from `ToolMiddleware::extra_tools`.
pub(crate) fn builtin_tools(
    tools: &ToolsConfig,
    tools_override: &Option<Vec<String>>,
    permissions: &PermissionsConfig,
    resolver: &Arc<dyn PermissionResolver>,
    middleware: &Arc<dyn ToolMiddleware>,
) -> Vec<Box<dyn ToolDyn>> {
    let mut out: Vec<Box<dyn ToolDyn>> = Vec::new();
    if tools.is_active("read", tools_override) {
        out.push(Box::new(ReadTool {
            permissions: permissions.clone(),
            resolver: resolver.clone(),
            middleware: middleware.clone(),
        }));
    }
    if tools.is_active("bash", tools_override) {
        out.push(Box::new(BashTool {
            permissions: permissions.clone(),
            resolver: resolver.clone(),
            middleware: middleware.clone(),
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- read tool slicing ---

    #[test]
    fn test_slice_lines_no_args_returns_full() {
        let s = "a\nb\nc";
        assert_eq!(slice_lines(s, None, None), s);
    }

    #[test]
    fn test_slice_lines_offset_only() {
        let s = "a\nb\nc\nd";
        assert_eq!(slice_lines(s, Some(1), None), "b\nc\nd");
    }

    #[test]
    fn test_slice_lines_offset_and_count() {
        let s = "a\nb\nc\nd\ne";
        assert_eq!(slice_lines(s, Some(1), Some(2)), "b\nc");
    }

    #[test]
    fn test_slice_lines_count_past_end_is_clamped() {
        let s = "a\nb";
        assert_eq!(slice_lines(s, Some(0), Some(99)), "a\nb");
    }

    #[test]
    fn test_slice_lines_offset_past_end_returns_empty() {
        assert_eq!(slice_lines("a\nb", Some(99), None), "");
    }

    // --- bash command validation ---

    #[test]
    fn test_simple_commands_pass_validation() {
        assert!(is_simple_bash_command("apt update"));
        assert!(is_simple_bash_command("ls -la /home"));
        assert!(is_simple_bash_command("find /home -name '*.jpg'"));
    }

    #[test]
    fn test_metacharacters_fail_validation() {
        assert!(!is_simple_bash_command("ls | grep foo"));
        assert!(!is_simple_bash_command("apt update; rm -rf /"));
        assert!(!is_simple_bash_command("apt update && malicious"));
        assert!(!is_simple_bash_command("echo `whoami`"));
        assert!(!is_simple_bash_command("echo $(whoami)"));
        assert!(!is_simple_bash_command("echo hi > /etc/passwd"));
        assert!(!is_simple_bash_command("apt\\ update"));
        assert!(!is_simple_bash_command("apt update\nrm -rf /"));
    }
}
