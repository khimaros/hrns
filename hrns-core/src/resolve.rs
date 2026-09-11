//! agent/skill discovery and markdown+frontmatter parsing. walks up from cwd
//! to the git root searching `.opencode/`, `.claude/`, `.agents/` (and
//! `.pi/agent/` for skills), then falls back to the `$HOME`-anchored globals.

use crate::config::{SkillsConfig, ToolsConfig};
use crate::permission::PermissionsConfig;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Debug, Default)]
pub struct AgentFrontmatter {
    pub description: Option<String>,
    pub model: Option<String>,
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default, alias = "permission")]
    pub permissions: PermissionsConfig,
}

#[derive(Deserialize, Debug, Default)]
pub struct SkillFrontmatter {
    pub description: Option<String>,
}

pub struct ParsedDoc<T> {
    pub frontmatter: T,
    pub body: String,
}

pub fn parse_markdown_with_frontmatter<T: serde::de::DeserializeOwned + Default>(
    content: &str,
) -> Result<ParsedDoc<T>, Box<dyn std::error::Error>> {
    if content.starts_with("---\n") || content.starts_with("---\r\n") {
        if let Some(end_idx) = content[4..].find("\n---") {
            let frontmatter_str = &content[4..end_idx + 4];
            let body_start = end_idx + 4 + 4;
            let body_start = if content.len() > body_start
                && content[body_start..].starts_with('\n')
            {
                body_start + 1
            } else if content.len() > body_start + 1 && content[body_start..].starts_with("\r\n") {
                body_start + 2
            } else {
                body_start
            };

            let frontmatter: T = serde_yaml::from_str(frontmatter_str)?;
            let body = content[body_start..].to_string();
            return Ok(ParsedDoc { frontmatter, body });
        }
    }
    Ok(ParsedDoc {
        frontmatter: T::default(),
        body: content.to_string(),
    })
}

/// distinguishes agent vs. skill lookup so the search can pick the right
/// per-base subdirectories (e.g. `.pi/agent/prompts` for agents but
/// `.pi/agent/skills` for skills).
#[derive(Copy, Clone)]
pub enum Kind {
    Agent,
    Skill,
}

/// flat fallback bases tried at each cwd-walk level after kind-specific
/// subdirs. these allow loose layouts like `.agents/foo.md` to work for
/// `load_*` but they are NOT enumerated for `--list-*`.
const FLAT_LOCAL_BASES: &[&str] = &[".opencode", ".claude", ".agents"];

/// kind-specific subdir suffixes searched relative to the cwd-walk parent.
fn local_subdirs(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Agent => &[".opencode/agents", ".claude/agents", ".agents/agents"],
        Kind::Skill => &[
            ".opencode/skills",
            ".claude/skills",
            ".agents/skills",
            ".pi/agent/skills",
        ],
    }
}

/// kind-specific global search dirs (already absolute, $HOME-anchored).
fn global_subdirs(kind: Kind) -> Vec<PathBuf> {
    let Ok(home) = env::var("HOME") else {
        return Vec::new();
    };
    let h = PathBuf::from(home);
    match kind {
        Kind::Agent => vec![
            h.join(".config/opencode/agents"),
            h.join(".claude/agents"),
            h.join(".agents/agents"),
        ],
        Kind::Skill => vec![
            h.join(".config/opencode/skills"),
            h.join(".claude/skills"),
            h.join(".agents/skills"),
            h.join(".pi/agent/skills"),
        ],
    }
}

/// flat global bases tried after kind-specific globals (lookup only).
fn flat_global_bases() -> Vec<PathBuf> {
    let Ok(home) = env::var("HOME") else {
        return Vec::new();
    };
    let h = PathBuf::from(home);
    vec![
        h.join(".config/opencode"),
        h.join(".claude"),
        h.join(".agents"),
    ]
}

fn find_file_in_dirs(kind: Kind, filename: &str) -> Option<PathBuf> {
    // walk up from current dir, retrying kind-specific + flat fallbacks
    let mut current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        for sub in local_subdirs(kind) {
            let path = current_dir.join(sub).join(filename);
            if path.exists() {
                return Some(path);
            }
        }
        for base in FLAT_LOCAL_BASES {
            let path = current_dir.join(base).join(filename);
            if path.exists() {
                return Some(path);
            }
        }
        if current_dir.join(".git").exists() {
            break;
        }
        if !current_dir.pop() {
            break;
        }
    }

    for d in global_subdirs(kind) {
        let path = d.join(filename);
        if path.exists() {
            return Some(path);
        }
    }
    for d in flat_global_bases() {
        let path = d.join(filename);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// finds all .md files in kind-specific subdirs across all base directories.
/// returns (name, path) pairs with the `.md` extension stripped. flat
/// fallback bases are not enumerated here, only structured subdirs.
pub fn find_all_in_dirs(kind: Kind) -> Vec<(String, PathBuf)> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut collect_from = |search_dir: &Path| {
        if let Ok(entries) = fs::read_dir(search_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("SKILL.md").exists() {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        let name = name.to_string();
                        if seen.insert(name.clone()) {
                            results.push((name, path.join("SKILL.md")));
                        }
                    }
                } else if path.extension().is_some_and(|e| e == "md") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let name = stem.to_string();
                        if seen.insert(name.clone()) {
                            results.push((name, path));
                        }
                    }
                }
            }
        }
    };

    let mut current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        for sub in local_subdirs(kind) {
            collect_from(&current_dir.join(sub));
        }
        if current_dir.join(".git").exists() {
            break;
        }
        if !current_dir.pop() {
            break;
        }
    }

    for d in global_subdirs(kind) {
        collect_from(&d);
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

pub fn load_agent(
    agent_name: &str,
) -> Result<ParsedDoc<AgentFrontmatter>, Box<dyn std::error::Error>> {
    let filename = format!("{}.md", agent_name);
    let path = find_file_in_dirs(Kind::Agent, &filename).ok_or_else(|| {
        format!(
            "agent '{}' not found in .opencode/, .claude/, or .agents/ directories",
            agent_name
        )
    })?;

    let content = fs::read_to_string(&path)?;
    parse_markdown_with_frontmatter(&content)
        .map_err(|e| format!("agent '{}' ({}): {}", agent_name, path.display(), e).into())
}

pub fn load_skill(
    skill_name: &str,
) -> Result<ParsedDoc<SkillFrontmatter>, Box<dyn std::error::Error>> {
    let filename_md = format!("{}.md", skill_name);
    let filename_skill_md = format!("{}/SKILL.md", skill_name);

    let path = find_file_in_dirs(Kind::Skill, &filename_skill_md)
        .or_else(|| find_file_in_dirs(Kind::Skill, &filename_md))
        .ok_or_else(|| {
            format!(
                "skill '{}' not found in .opencode/, .claude/, .agents/, or .pi/agent/ directories",
                skill_name
            )
        })?;

    let content = fs::read_to_string(&path)?;
    parse_markdown_with_frontmatter(&content)
        .map_err(|e| format!("skill '{}' ({}): {}", skill_name, path.display(), e).into())
}

/// appends skill contents to a system prompt. skills not enabled by
/// `config` are skipped with a warning.
pub fn append_skills(system_prompt: &mut String, skill_names: &[String], config: &SkillsConfig) {
    let allowed: Vec<&String> = skill_names
        .iter()
        .filter(|n| {
            if config.is_active(n) {
                true
            } else {
                eprintln!("warning: skill '{}' disabled by config", n);
                false
            }
        })
        .collect();
    if allowed.is_empty() {
        return;
    }
    system_prompt.push_str("\n\n# skills\n");
    for skill_name in allowed {
        match load_skill(skill_name) {
            Ok(skill) => {
                system_prompt.push_str(&format!("\n## {}\n", skill_name));
                if let Some(desc) = skill.frontmatter.description {
                    system_prompt.push_str(&format!("description: {}\n", desc));
                }
                system_prompt.push_str(&format!("{}\n", skill.body));
            }
            Err(e) => {
                eprintln!("warning: failed to load skill '{}': {}", skill_name, e);
            }
        }
    }
}

pub fn build_system_prompt(
    agent: &ParsedDoc<AgentFrontmatter>,
    skills_config: &SkillsConfig,
) -> String {
    let mut system_prompt = agent.body.clone();
    if let Some(skills) = &agent.frontmatter.skills {
        append_skills(&mut system_prompt, skills, skills_config);
    }
    system_prompt
}
