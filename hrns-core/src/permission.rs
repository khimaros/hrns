//! the permission model (modeled after opencode.ai/docs/permissions): each
//! tool maps to a flat level (allow/ask/deny) or a glob-pattern map. the "ask"
//! decision is resolved by a caller-supplied `PermissionResolver` so the core
//! stays tty-agnostic: the CLI prompts on /dev/tty, an embedding host (hmux)
//! routes it to a hub interception.

use crate::glob::glob_matches;
use async_trait::async_trait;
use rig::tool::ToolError;
use serde::{de, Deserialize, Deserializer};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionLevel {
    Allow,
    Ask,
    Deny,
}

impl<'de> Deserialize<'de> for PermissionLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "allow" => Ok(PermissionLevel::Allow),
            "ask" => Ok(PermissionLevel::Ask),
            "deny" => Ok(PermissionLevel::Deny),
            _ => Err(de::Error::custom(format!(
                "invalid permission level '{}', expected 'allow', 'ask', or 'deny'",
                s
            ))),
        }
    }
}

/// a permission can be a single level or a map of glob patterns to levels.
/// patterns support gitignore-style globs: `*` (non-slash), `**` (any), `?`.
/// when multiple patterns match, the most specific (fewest wildcards) wins.
#[derive(Debug, Clone)]
pub enum Permission {
    Level(PermissionLevel),
    Patterns(HashMap<String, PermissionLevel>),
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        if let serde_yaml::Value::String(s) = &value {
            match s.as_str() {
                "allow" => return Ok(Permission::Level(PermissionLevel::Allow)),
                "ask" => return Ok(Permission::Level(PermissionLevel::Ask)),
                "deny" => return Ok(Permission::Level(PermissionLevel::Deny)),
                _ => {
                    return Err(de::Error::custom(format!(
                        "invalid permission level '{}', expected 'allow', 'ask', or 'deny'",
                        s
                    )))
                }
            }
        }
        if let serde_yaml::Value::Mapping(map) = &value {
            let mut patterns = HashMap::new();
            for (k, v) in map {
                let key = k.as_str().ok_or_else(|| {
                    de::Error::custom(format!("expected string key, got {:?}", k))
                })?;
                let level_str = v.as_str().ok_or_else(|| {
                    de::Error::custom(format!(
                        "pattern '{}': expected 'allow', 'ask', or 'deny', got {:?}",
                        key, v
                    ))
                })?;
                let level = match level_str {
                    "allow" => PermissionLevel::Allow,
                    "ask" => PermissionLevel::Ask,
                    "deny" => PermissionLevel::Deny,
                    _ => return Err(de::Error::custom(format!(
                        "pattern '{}': invalid permission level '{}', expected 'allow', 'ask', or 'deny'",
                        key, level_str
                    ))),
                };
                patterns.insert(key.to_string(), level);
            }
            return Ok(Permission::Patterns(patterns));
        }
        Err(de::Error::custom(format!(
            "expected a permission level string or a map of patterns, got {:?}",
            value
        )))
    }
}

impl Permission {
    /// checks the permission level for the given input.
    /// `path_mode`: when true, `*` stops at `/` boundaries (for file paths).
    /// when false, `*` matches any character (for commands).
    pub fn check(&self, input: &str, path_mode: bool) -> PermissionLevel {
        match self {
            Permission::Level(level) => level.clone(),
            Permission::Patterns(patterns) => {
                let mut result = PermissionLevel::Deny;
                let mut best_specificity = 0usize;
                for (pattern, level) in patterns {
                    // try the pattern as-is, and also with trailing ` *`/` **`
                    // stripped so "ls *" also matches "ls" (no args)
                    let candidates = [
                        glob_matches(pattern, input, path_mode),
                        pattern
                            .strip_suffix(" **")
                            .or_else(|| pattern.strip_suffix(" *"))
                            .and_then(|prefix| glob_matches(prefix, input, path_mode)),
                    ];
                    for specificity in candidates.into_iter().flatten() {
                        if specificity >= best_specificity {
                            best_specificity = specificity;
                            result = level.clone();
                        }
                    }
                }
                result
            }
        }
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct PermissionsConfig {
    #[serde(flatten)]
    pub tools: HashMap<String, Permission>,
}

impl PermissionsConfig {
    pub fn merge(self, other: PermissionsConfig) -> PermissionsConfig {
        let mut merged = self.tools;
        merged.extend(other.tools);
        PermissionsConfig { tools: merged }
    }

    /// applies CLI-style overrides on top of self with pattern-level
    /// granularity. for each override:
    ///   - a bare `TOOL=LEVEL` replaces the tool entry entirely.
    ///   - a `TOOL:PATTERN=LEVEL` merges into the tool's pattern map; if the
    ///     existing entry was a single level, it is first promoted to a
    ///     pattern map with `**` mapped to the old level.
    pub fn apply_overrides(mut self, overrides: &[PermissionOverride]) -> Self {
        for ov in overrides {
            match &ov.pattern {
                None => {
                    self.tools
                        .insert(ov.tool.clone(), Permission::Level(ov.level.clone()));
                }
                Some(pat) => {
                    let existing = self.tools.remove(&ov.tool);
                    let mut patterns = match existing {
                        Some(Permission::Patterns(map)) => map,
                        Some(Permission::Level(lvl)) => {
                            let mut m = HashMap::new();
                            m.insert("**".to_string(), lvl);
                            m
                        }
                        None => HashMap::new(),
                    };
                    patterns.insert(pat.clone(), ov.level.clone());
                    self.tools
                        .insert(ov.tool.clone(), Permission::Patterns(patterns));
                }
            }
        }
        self
    }

    pub fn check(&self, tool_name: &str, input: &str, path_mode: bool) -> PermissionLevel {
        // outer lookup: glob match on tool name (so `"*" = "deny"` works
        // as a default). most-specific match wins; missing entries default
        // to deny. inner pattern matching is delegated to `Permission::check`.
        let mut best: Option<(usize, &Permission)> = None;
        for (pattern, perm) in &self.tools {
            if let Some(specificity) = glob_matches(pattern, tool_name, false) {
                if best.as_ref().is_none_or(|(s, _)| specificity >= *s) {
                    best = Some((specificity, perm));
                }
            }
        }
        best.map(|(_, p)| p.check(input, path_mode))
            .unwrap_or(PermissionLevel::Deny)
    }
}

/// a single CLI permission override, built from `--permissions-{allow,ask,deny}`.
/// shapes for the flag value:
///   `TOOL`                   -> pattern is None (replaces tool entry)
///   `TOOL:PATTERN`           -> pattern merges into the tool's map
#[derive(Debug, Clone)]
pub struct PermissionOverride {
    pub tool: String,
    pub pattern: Option<String>,
    pub level: PermissionLevel,
}

/// parses the `TOOL` or `TOOL:PATTERN` value (without the level, which is
/// supplied by which flag was used).
pub fn parse_permission_target(s: &str) -> Result<(String, Option<String>), String> {
    let (tool, pattern) = match s.split_once(':') {
        Some((t, p)) => (t.trim().to_string(), Some(p.to_string())),
        None => (s.trim().to_string(), None),
    };
    if tool.is_empty() {
        return Err(format!(
            "invalid permission target '{}': empty tool name",
            s
        ));
    }
    Ok((tool, pattern))
}

/// resolves an "ask"-level permission for a tool invocation. the CLI prompts
/// on /dev/tty (bypassable with --yes); an embedding host (hmux) routes it to
/// a hub interception. async so the hmux impl can round-trip to the hub; the
/// CLI impl is ready-synchronous. returns true to allow the call, false to deny.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn confirm(&self, tool_name: &str, input: &str) -> bool;
}

/// checks permission for a tool invocation, returning Ok(()) or a ToolError.
/// `path_mode`: true for file-path tools (read), false for command tools (bash).
/// an "ask" verdict is delegated to `resolver`.
pub async fn check_tool_permission(
    permissions: &PermissionsConfig,
    resolver: &dyn PermissionResolver,
    tool_name: &str,
    input: &str,
    path_mode: bool,
) -> Result<(), ToolError> {
    match permissions.check(tool_name, input, path_mode) {
        PermissionLevel::Allow => Ok(()),
        PermissionLevel::Ask => {
            if resolver.confirm(tool_name, input).await {
                Ok(())
            } else {
                Err(ToolError::ToolCallError(
                    format!("permission denied (user rejected): {}", input).into(),
                ))
            }
        }
        PermissionLevel::Deny => Err(ToolError::ToolCallError(
            format!("permission denied: {}", input).into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- permission resolution (path mode for read) ---

    #[test]
    fn test_permission_path_patterns() {
        let mut patterns = HashMap::new();
        patterns.insert("**".to_string(), PermissionLevel::Deny);
        patterns.insert("/etc/os-release".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(perm.check("/etc/os-release", true), PermissionLevel::Allow);
        assert_eq!(perm.check("/etc/shadow", true), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_doublestar_dir_pattern() {
        let mut patterns = HashMap::new();
        patterns.insert("**".to_string(), PermissionLevel::Deny);
        patterns.insert("/home/**".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(
            perm.check("/home/user/file.txt", true),
            PermissionLevel::Allow
        );
        assert_eq!(perm.check("/etc/passwd", true), PermissionLevel::Deny);
    }

    // --- permission resolution (command mode for bash) ---

    #[test]
    fn test_permission_command_star_matches_slashes() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("ls *".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        // in command mode, "ls *" matches args with slashes
        assert_eq!(
            perm.check("ls -la /home/user", false),
            PermissionLevel::Allow
        );
        assert_eq!(perm.check("rm -rf /", false), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_command_most_specific_wins() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("apt update".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(perm.check("apt update", false), PermissionLevel::Allow);
        assert_eq!(perm.check("rm -rf /", false), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_trailing_wildcard_matches_no_args() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("ls *".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        // "ls *" should also match bare "ls" (no args)
        assert_eq!(perm.check("ls", false), PermissionLevel::Allow);
        assert_eq!(perm.check("ls -la /home", false), PermissionLevel::Allow);
        assert_eq!(perm.check("rm", false), PermissionLevel::Deny);
    }
}
