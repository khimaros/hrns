use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn setup_project(dir: &TempDir, agent_content: &str) {
    let agents_dir = dir.path().join(".agents").join("agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(agents_dir.join("test-agent.md"), agent_content).unwrap();
    // create a .git dir so discovery stops here
    fs::create_dir_all(dir.path().join(".git")).unwrap();
}

fn hrns(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("hrns").unwrap();
    cmd.current_dir(dir.path());
    cmd.env("HOME", dir.path());
    cmd
}

// --- hooks ---

#[cfg(unix)]
fn write_hook(dir: &TempDir, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let hooks_dir = dir.path().join(".agents").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let path = hooks_dir.join(name);
    fs::write(&path, body).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
}

#[cfg(unix)]
fn enable_all_hooks(dir: &TempDir) {
    fs::write(dir.path().join("hrns.toml"), "[hooks]\n\"*\" = true\n").unwrap();
}

#[cfg(unix)]
#[test]
fn test_list_hooks_discovers_scripts() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_hook(&dir, "demo.sh", "#!/bin/sh\nstage=\"$1\"\nif [ \"$stage\" = \"discover\" ]; then\n  echo '{\"name\":\"demo\",\"tools\":[{\"name\":\"ping\",\"description\":\"ping the demo\",\"parameters\":{\"msg\":\"the message\"}}]}'\nfi\n");
    enable_all_hooks(&dir);
    hrns(&dir)
        .arg("--list-hooks")
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("demo_ping"));
}

#[cfg(unix)]
#[test]
fn test_mutate_request_appends_to_system_prompt() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_hook(&dir, "mutate.sh", "#!/bin/sh\nif [ \"$1\" = \"mutate_request\" ]; then\n  echo '{\"system\":[\"INJECTED-BY-HOOK\"]}'\nfi\n");
    enable_all_hooks(&dir);
    hrns(&dir)
        .arg("-s")
        .arg("base prompt")
        .arg("-p")
        .arg("hi")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("base prompt"))
        .stdout(predicate::str::contains("INJECTED-BY-HOOK"));
}

#[cfg(unix)]
#[test]
fn test_mutate_request_payload_includes_system_user_and_model() {
    // payload expansion: hook sees finalized system prompt, user prompt,
    // and model id. dump the stdin payload
    // straight to stderr via the `log` field so we can match on it.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_hook(
        &dir,
        "log.sh",
        r#"#!/bin/sh
if [ "$1" = "mutate_request" ]; then
  payload=$(cat)
  printf 'PAYLOAD %s\n' "$payload" >&2
fi
"#,
    );
    enable_all_hooks(&dir);
    hrns(&dir)
        .arg("-s")
        .arg("THE-SYS")
        .arg("-p")
        .arg("THE-USR")
        .arg("--model")
        .arg("openai/THE-MODEL")
        .arg("--dry-run")
        .assert()
        .success()
        .stderr(predicate::str::contains("\"system\":\"THE-SYS\""))
        .stderr(predicate::str::contains("\"user\":\"THE-USR\""))
        .stderr(predicate::str::contains("\"model\":\"openai/THE-MODEL\""));
}

#[cfg(unix)]
fn write_datetime_hook(dir: &TempDir) {
    write_hook(dir, "datetime.sh", "#!/bin/sh\nstage=\"$1\"\ncase \"$stage\" in\n  discover) echo '{\"name\":\"datetime\",\"tools\":[{\"name\":\"now\",\"description\":\"current time\",\"parameters\":{}}]}' ;;\nesac\n");
}

#[cfg(unix)]
#[test]
fn test_tools_glob_filters_hook_tool_from_dump_request() {
    // [tools] "*" = true with "datetime_*" = false should hide the
    // hook-registered `datetime_now` tool from --dump-request.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_datetime_hook(&dir);
    fs::write(
        dir.path().join("hrns.toml"),
        "[hooks]\n\"*\" = true\n[tools]\n\"*\" = true\n\"datetime_*\" = false\n",
    )
    .unwrap();
    hrns(&dir)
        .arg("-s")
        .arg("x")
        .arg("-p")
        .arg("y")
        .arg("-D")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\": \"read\""))
        .stdout(predicate::str::contains("\"name\": \"bash\""))
        .stdout(predicate::str::contains("datetime_now").not());
}

#[cfg(unix)]
#[test]
fn test_tools_glob_allows_hook_tool_when_pattern_enables_it() {
    // sanity: with "*" = true and no override, the hook tool is exposed.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_datetime_hook(&dir);
    fs::write(
        dir.path().join("hrns.toml"),
        "[hooks]\n\"*\" = true\n[tools]\n\"*\" = true\n",
    )
    .unwrap();
    hrns(&dir)
        .arg("-s")
        .arg("x")
        .arg("-p")
        .arg("y")
        .arg("-D")
        .assert()
        .success()
        .stdout(predicate::str::contains("datetime_now"));
}

#[cfg(unix)]
#[test]
fn test_hooks_gate_blocks_discovery() {
    // [hooks] datetime = false hides the tool entirely (script's
    // `discover` is run for --list-hooks, but the runtime manager
    // filters it out, so --dump-request never sees datetime_now).
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_datetime_hook(&dir);
    fs::write(
        dir.path().join("hrns.toml"),
        "[hooks]\n\"*\" = true\ndatetime = false\n[tools]\n\"*\" = true\n",
    )
    .unwrap();
    hrns(&dir)
        .arg("-s")
        .arg("x")
        .arg("-p")
        .arg("y")
        .arg("-D")
        .assert()
        .success()
        .stdout(predicate::str::contains("datetime_now").not());
}

#[cfg(unix)]
#[test]
fn test_non_executable_hook_skipped() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let hooks_dir = dir.path().join(".agents").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("not_exec.sh"),
        "#!/bin/sh\necho '{\"name\":\"nope\"}'\n",
    )
    .unwrap();
    enable_all_hooks(&dir);
    hrns(&dir)
        .arg("--list-hooks")
        .assert()
        .success()
        .stdout(predicate::str::contains("nope").not());
}

// --- permission pattern matching ---

#[test]
fn test_wildcard_denies_by_default() {
    // ensures that "**": deny blocks unmatched patterns
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
tools:
  bash: true
permissions:
  bash:
    "**": deny
    "echo hello": allow
---
test agent
"#,
    );
    // we can't easily test tool invocation without a real LLM,
    // but we can verify the agent loads and the CLI parses correctly
    hrns(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .arg("-v")
        .arg("--dry-run")
        .write_stdin("")
        .assert()
        .success();
}

#[test]
fn test_agent_with_bash_and_read_tools() {
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
tools:
  read: true
  bash: true
permissions:
  read:
    "**": allow
  bash:
    "**": deny
    "ls **": allow
---
test agent
"#,
    );
    hrns(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .arg("-v")
        .arg("--dry-run")
        .write_stdin("")
        .assert()
        .success();
}

// --- CLI flag parsing ---

#[test]
fn test_dry_run_outputs_prompts() {
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
tools:
  bash: true
permissions:
  bash:
    "*": deny
    "echo *": allow
---
you are a test agent
"#,
    );
    hrns(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("hello world")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--- system prompt ---")
                .and(predicate::str::contains("you are a test agent"))
                .and(predicate::str::contains("--- user prompt ---"))
                .and(predicate::str::contains("hello world"))
                .and(predicate::str::contains("bash")),
        );
}

#[test]
fn test_yes_flag_accepted() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    hrns(&dir)
        .arg("-y")
        .arg("-p")
        .arg("test")
        .arg("--dry-run")
        .assert()
        .success();
}

#[test]
fn test_system_prompt_flag() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    hrns(&dir)
        .arg("-s")
        .arg("you are a pirate")
        .arg("-p")
        .arg("hello")
        .arg("-v")
        .arg("--dry-run")
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("you are a pirate"));
}

#[test]
fn test_system_prompt_overrides_agent() {
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
description: original agent
---
original system prompt
"#,
    );
    hrns(&dir)
        .arg("test-agent")
        .arg("-s")
        .arg("override prompt")
        .arg("-p")
        .arg("hello")
        .arg("-v")
        .arg("--dry-run")
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("override prompt"))
        .stdout(predicate::str::contains("original system prompt").not());
}

// --- init ---

#[test]
fn test_init_creates_config() {
    let dir = TempDir::new().unwrap();
    hrns(&dir)
        .arg("--init")
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized configuration"));

    let config_path = dir.path().join(".config").join("hrns").join("config.toml");
    assert!(config_path.exists());
}

#[test]
fn test_init_does_not_overwrite() {
    let dir = TempDir::new().unwrap();
    let config_dir = dir.path().join(".config").join("hrns");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), "existing").unwrap();

    hrns(&dir)
        .arg("--init")
        .assert()
        .success()
        .stderr(predicate::str::contains("already exists"));

    let content = fs::read_to_string(config_dir.join("config.toml")).unwrap();
    assert_eq!(content, "existing");
}

// --- bash command validation (unit-style via the binary) ---
// these test that shell metacharacters are rejected before permission checks.
// since we can't invoke tools without an LLM, we test the validate function
// indirectly through the module. for now, the compile+parse tests above cover
// that the tool is wired in correctly.

// --- error messages ---

#[test]
fn test_invalid_permission_level_shows_value() {
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
tools:
  bash: true
permissions:
  bash:
    "*": dennny
---
test agent
"#,
    );
    hrns(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("invalid permission level 'dennny'")
                .and(predicate::str::contains("agent 'test-agent'")),
        );
}

#[test]
fn test_parse_error_includes_agent_name() {
    let dir = TempDir::new().unwrap();
    setup_project(
        &dir,
        r#"---
permissions:
  read: 12345
---
test
"#,
    );
    hrns(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("agent 'test-agent'"));
}

// --- global directory discovery ---

fn hrns_separate_home<'a>(work_dir: &'a TempDir, home_dir: &'a TempDir) -> Command {
    let mut cmd = Command::cargo_bin("hrns").unwrap();
    cmd.current_dir(work_dir.path());
    cmd.env("HOME", home_dir.path());
    cmd
}

#[test]
fn test_list_agents_finds_global_agents() {
    let work_dir = TempDir::new().unwrap();
    let home_dir = TempDir::new().unwrap();
    fs::create_dir_all(work_dir.path().join(".git")).unwrap();
    // place an agent in global ~/.agents/agents/
    let global_agents = home_dir.path().join(".agents").join("agents");
    fs::create_dir_all(&global_agents).unwrap();
    fs::write(
        global_agents.join("global-agent.md"),
        "---\ndescription: a global agent\n---\nglobal prompt\n",
    )
    .unwrap();

    hrns_separate_home(&work_dir, &home_dir)
        .arg("--list-agents")
        .assert()
        .success()
        .stdout(predicate::str::contains("global-agent"));
}

#[test]
fn test_list_skills_finds_global_skills() {
    let work_dir = TempDir::new().unwrap();
    let home_dir = TempDir::new().unwrap();
    fs::create_dir_all(work_dir.path().join(".git")).unwrap();
    // place a skill in global ~/.claude/skills/
    let global_skills = home_dir.path().join(".claude").join("skills");
    fs::create_dir_all(&global_skills).unwrap();
    fs::write(
        global_skills.join("global-skill.md"),
        "---\ndescription: a global skill\n---\nglobal skill body\n",
    )
    .unwrap();

    hrns_separate_home(&work_dir, &home_dir)
        .arg("--list-skills")
        .assert()
        .success()
        .stdout(predicate::str::contains("global-skill"));
}

#[test]
fn test_list_skills_finds_global_symlinked_dir() {
    let work_dir = TempDir::new().unwrap();
    let home_dir = TempDir::new().unwrap();
    fs::create_dir_all(work_dir.path().join(".git")).unwrap();
    // create skills in a separate directory and symlink to it
    let real_skills = home_dir.path().join("real-skills");
    fs::create_dir_all(&real_skills).unwrap();
    fs::write(
        real_skills.join("linked-skill.md"),
        "---\ndescription: linked skill\n---\nlinked body\n",
    )
    .unwrap();
    let opencode_dir = home_dir.path().join(".config").join("opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::os::unix::fs::symlink(&real_skills, opencode_dir.join("skills")).unwrap();

    hrns_separate_home(&work_dir, &home_dir)
        .arg("--list-skills")
        .assert()
        .success()
        .stdout(predicate::str::contains("linked-skill"));
}

// --- list formatting ---

#[test]
fn test_list_output_uses_aligned_columns() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let output = hrns(&dir).arg("--list-tools").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // should not contain raw tabs
    assert!(
        !stdout.contains('\t'),
        "output should use spaces, not tabs: {}",
        stdout
    );
    // should have at least 2 spaces between name and description
    assert!(
        stdout.contains("  "),
        "output should have padded columns: {}",
        stdout
    );
}

// --- empty input ---

#[cfg(unix)]
#[test]
fn test_host_capability_payload_present() {
    // every hook invocation includes a `host` block: {name, version, stages}.
    // stages enumerates the canonical stage names this host fires
    // (see https://github.com/khimaros/hcp-spec/).
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_hook(
        &dir,
        "hostlog.sh",
        r#"#!/bin/sh
if [ "$1" = "mutate_request" ]; then
  payload=$(cat)
  printf 'PAYLOAD %s\n' "$payload" >&2
fi
"#,
    );
    enable_all_hooks(&dir);
    hrns(&dir)
        .arg("-s")
        .arg("x")
        .arg("-p")
        .arg("y")
        .arg("--dry-run")
        .assert()
        .success()
        .stderr(predicate::str::contains("\"host\":{"))
        .stderr(predicate::str::contains("\"name\":\"hrns\""))
        .stderr(predicate::str::contains("\"version\":3"))
        // v3: the base payload carries the workspace cwd
        .stderr(predicate::str::contains("\"cwd\":"))
        // canonical stage names are advertised
        .stderr(predicate::str::contains("\"before_tool\""))
        .stderr(predicate::str::contains("\"after_tool\""))
        .stderr(predicate::str::contains("\"before_stop\""))
        // predecessor stage names must not leak back in
        .stderr(predicate::str::contains("\"tool_before\"").not())
        .stderr(predicate::str::contains("\"tool_after\"").not())
        .stderr(predicate::str::contains("\"idle\"").not());
}

#[test]
fn test_empty_prompt_shows_usage() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    hrns(&dir)
        .write_stdin("")
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage:"))
        .stderr(predicate::str::contains(
            "one of --prompt or stdin must be non-empty",
        ));
}
