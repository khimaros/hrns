#!/usr/bin/env python3
"""end-to-end conformance test for hrns.

drives the shared hcp conformance suite (../hcp-spec/conformance) against the
real hrns binary: seed a temp workspace from the canonical hello hook, register
a mock-openai provider pointing at fake-openai, run hrns, and assert the
captured chat-completions request. the protocol-level assertions live in the
shared driver; this file is the hrns-specific seam plus hrns's one-shot extras.

set HRNS_BIN to override the hrns binary; default is ./target/debug/hrns.
set FAKE_OPENAI_BIN to override the mock binary. the run skips if the mock is
not built.

hrns is one-shot, so heartbeat/compaction scenarios do not apply. it also does
not currently forward ctx.prompts to mutate_request, so the hello hook emits
only its <env> block (no preamble/chat verbatim assertion).
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = PROJECT_ROOT / "tests" / ".artifacts"

HRNS_BIN = os.environ.get("HRNS_BIN") or str(PROJECT_ROOT / "target" / "debug" / "hrns")
if not Path(HRNS_BIN).exists():
    sys.exit(f"FAIL: hrns binary not found at {HRNS_BIN}; set HRNS_BIN or run `cargo build`")

sys.path.insert(0, str(PROJECT_ROOT.parent / "hcp-spec" / "conformance"))
import hcpconform as hc

SYSTEM_PROMPT = "you are a test agent"


def _clean_env(home):
    """hrns's own and xdg vars stripped, HOME pinned to a throwaway tree."""
    return {**{k: v for k, v in os.environ.items()
               if not k.startswith(("HRNS_", "XDG_"))},
            "HOME": str(home), "OPENAI_API_KEY": "test"}


class HrnsAdapter(hc.HostAdapter):
    name = "hrns"
    wants_heartbeat = False
    builtin_tools = {"read", "bash"}

    def __init__(self):
        self.list_hooks_output = ""

    def run_build(self, fixture):
        fake = hc.start_fake_openai()
        print(f"mock server on {fake.base_url}")
        workdir = Path(tempfile.mkdtemp(prefix="hrns-llm-test-"))
        try:
            project = hc.seed_workspace(workdir / "project", fixture, make_git=True)
            home = workdir / "home"
            home.mkdir()
            # hrns discovers hooks under .agents/hooks/ (not the fixture's hooks/).
            agents_hooks = project / ".agents" / "hooks"
            agents_hooks.mkdir(parents=True)
            shutil.copy(fixture.hook, agents_hooks / "hello.py")
            (agents_hooks / "hello.py").chmod(0o755)
            (project / "hrns.toml").write_text(
                f'default_max_turns = 1\n\n[hooks]\n"*" = true\n\n[tools]\n"*" = true\n\n'
                f'[[providers]]\nname = "mock-openai"\nclient = "openai_completions"\n'
                f'api_key = "test"\nbase_url = "{fake.base_url}"\n')
            env = _clean_env(home)

            # discovery sanity: hrns runs the hook's discover stage and surfaces
            # every namespaced tool. isolated here for a clearer failure mode.
            print("checking hook discovery...")
            listed = subprocess.run([HRNS_BIN, "--list-hooks"], cwd=str(project),
                                    env=env, capture_output=True, text=True, timeout=20)
            self.list_hooks_output = (listed.stdout or "") + "\n" + (listed.stderr or "")

            print("running hrns...")
            proc = subprocess.Popen(
                [HRNS_BIN, "--model", "mock-openai/fake-model", "-s", SYSTEM_PROMPT,
                 "-p", "hello", "-y", "-q"],
                cwd=str(project), env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            build = hc.poll_for(lambda: hc.find_build_request(fake.captures()), proc, 30)
            if proc.poll() is None:
                proc.terminate()
            try:
                stdout, stderr = proc.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                stdout, stderr = proc.communicate()
            caps = fake.captures()
        finally:
            fake.stop()
            shutil.rmtree(workdir, ignore_errors=True)
        return hc.RunResult(build, None, caps, stdout, stderr)

    def extra_build_checks(self, body, fixture, runner):
        runner.check("model is fake-model", body.get("model") == "fake-model",
                     f"model={body.get('model')}")
        runner.check("system prompt includes -s flag value verbatim",
                     SYSTEM_PROMPT in hc.system_text(body), hc.system_text(body)[:400])
        hc.assert_builtin_tools(body, self.builtin_tools, runner)
        hc.assert_note_tags_array(body, runner)  # hrns supports element-typed arrays
        hc.assert_every_tool_has_description(body, runner)
        hc.assert_param_descriptions(body, runner, prefixes=("hello_note_",))
        # hrns emits the enum as a string-typed json-schema `enum` (the shared
        # helper only checks the value set, which must hold across dialects).
        runner.check("note_write.priority is string",
                     hc.prop(body, "hello_note_write", "priority").get("type") == "string",
                     f"got: {hc.prop(body, 'hello_note_write', 'priority')}")
        # hrns is one-shot: exactly one user turn, the verbatim prompt, no heartbeat.
        users = [m for m in body.get("messages", []) if m.get("role") == "user"]
        runner.check("exactly one user message", len(users) == 1, f"count={len(users)}")
        u = hc.user_text(body)
        runner.check("user prompt is exactly 'hello'", u.strip() == "hello", f"user_text={u!r}")
        runner.check("user prompt does NOT have [heartbeat] prefix (hrns is one-shot)",
                     "[heartbeat]" not in u)


def check_list_hooks(adapter, runner):
    out = adapter.list_hooks_output
    runner.check("--list-hooks: hello hook discovered", "hello" in out, out[:400])
    for t in ("hello_note_list", "hello_note_read", "hello_note_write", "hello_note_delete"):
        runner.check(f"--list-hooks: {t} surfaced", t in out)


def main():
    adapter = HrnsAdapter()
    if not hc.preflight(adapter):
        return 0
    runner = hc.CheckRunner()
    result = hc.run_conformance(adapter, runner)
    hc.dump_artifacts(ARTIFACTS, "hrns_integration", result)
    check_list_hooks(adapter, runner)
    runner.summary()
    return runner.exit_code()


if __name__ == "__main__":
    sys.exit(main())
