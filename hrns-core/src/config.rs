//! cascading toml configuration: `hrns.toml` (project) down to
//! `~/.config/hrns/config.toml` (global), plus the per-tool/hook/skill enable
//! gates. all gates share the glob semantics in [`crate::glob`].

use crate::glob::glob_lookup;
use crate::permission::PermissionsConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Default, Clone)]
pub struct ToolsConfig {
    #[serde(flatten)]
    pub tools: HashMap<String, bool>,
}

impl ToolsConfig {
    pub fn merge(self, other: ToolsConfig) -> ToolsConfig {
        let mut merged = self.tools;
        merged.extend(other.tools);
        ToolsConfig { tools: merged }
    }

    /// checks if a tool is active. if `tools_override` is Some, only
    /// the listed tools are enabled (ignoring config). otherwise, looks up
    /// the tool in the config map using gitignore-style glob matching
    /// (most specific wins, `"*"` acts as a default). default: false.
    pub fn is_active(&self, tool_name: &str, tools_override: &Option<Vec<String>>) -> bool {
        match tools_override {
            Some(list) => list.iter().any(|t| t == tool_name),
            None => glob_lookup(&self.tools, tool_name, false),
        }
    }
}

/// gates which hook scripts are loaded, keyed by the hook's declared
/// `name` (returned in `discover`, defaulting to the file stem). same
/// glob semantics as `[tools]`: `"*" = false` is the default.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct HooksConfig {
    #[serde(flatten)]
    pub hooks: HashMap<String, bool>,
}

impl HooksConfig {
    /// checks if a hook is active. if `hooks_override` is Some, only the
    /// listed hooks are enabled (ignoring config). otherwise, looks up
    /// the hook in config with glob semantics.
    pub fn is_active(&self, hook_name: &str, hooks_override: &Option<Vec<String>>) -> bool {
        match hooks_override {
            Some(list) => list.iter().any(|h| h == hook_name),
            None => glob_lookup(&self.hooks, hook_name, false),
        }
    }
}

/// gates which skills are loadable. keyed by skill name (the file stem
/// passed to `load_skill`). same glob semantics as `[tools]`.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct SkillsConfig {
    #[serde(flatten)]
    pub skills: HashMap<String, bool>,
}

impl SkillsConfig {
    pub fn is_active(&self, skill_name: &str) -> bool {
        glob_lookup(&self.skills, skill_name, false)
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct ProviderConfig {
    pub name: String,
    /// openai (responses API), openai_completions, anthropic, gemini, cohere, xai
    pub client: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct Config {
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u64>,
    pub default_max_turns: Option<usize>,
    /// fallback system prompt used when no `-s` flag and no agent body
    pub default_system_prompt: Option<String>,
    /// max bytes of tool-call output rendered to stderr (default: 2000)
    pub tool_output_truncate: Option<usize>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default, alias = "permission")]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

pub fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_paths = [
        "hrns.toml",
        ".hrns.toml",
        ".config/hrns.toml",
        "~/.config/hrns/config.toml",
    ];

    let mut config_content = String::new();
    for path_str in config_paths {
        let path = if let Some(stripped) = path_str.strip_prefix("~/") {
            if let Ok(home) = env::var("HOME") {
                PathBuf::from(home).join(stripped)
            } else {
                continue;
            }
        } else {
            PathBuf::from(path_str)
        };

        if path.exists() {
            config_content = fs::read_to_string(path)?;
            break;
        }
    }

    if config_content.is_empty() {
        return Ok(Config::default());
    }

    let config: Config = toml::from_str(&config_content)?;
    Ok(config)
}

/// the provider wiring a `<provider>/<model>` string resolves to against the
/// config: the rig client type, the bare model id, and the credentials. shared
/// by the CLI and the hmux backend so both resolve a model the same way.
pub struct Resolved {
    pub provider_name: String,
    pub client_type: String,
    pub model_name: String,
    pub api_key: String,
    pub base_url: Option<String>,
}

/// resolves `full_model_name` (`<provider>/<model>`, provider defaulting to
/// openai) against the config's providers. the api key falls back to the
/// conventional env var per client type (OPENAI_API_KEY, ANTHROPIC_API_KEY, ...)
/// so a keyless local server still works and a hosted one errors upstream.
pub fn resolve_provider(config: &Config, full_model_name: &str) -> Resolved {
    let (provider_name, model_name) = full_model_name
        .split_once('/')
        .unwrap_or(("openai", full_model_name));
    let default_provider_config = ProviderConfig {
        name: provider_name.to_string(),
        client: Some(provider_name.to_string()),
        ..Default::default()
    };
    let provider_config = config
        .providers
        .iter()
        .find(|p| p.name == provider_name)
        .unwrap_or(&default_provider_config);
    let client_type = provider_config
        .client
        .as_deref()
        .unwrap_or(provider_name)
        .to_string();
    let api_key = provider_config.api_key.clone().unwrap_or_else(|| {
        let env_var_name = match client_type.as_str() {
            "openai" | "openai_completions" | "openai_responses" => "OPENAI_API_KEY",
            "anthropic" => "ANTHROPIC_API_KEY",
            "gemini" => "GEMINI_API_KEY",
            "cohere" => "COHERE_API_KEY",
            "xai" => "XAI_API_KEY",
            _ => "",
        };
        env::var(env_var_name).unwrap_or_default()
    });
    Resolved {
        provider_name: provider_name.to_string(),
        client_type,
        model_name: model_name.to_string(),
        api_key,
        base_url: provider_config.base_url.clone(),
    }
}
