"""Adapter contract tests using a fake Harbor environment.

Harbor is intentionally optional in this repository, so these tests install a
small fake of the BaseInstalledAgent surface before importing the adapter.
They exercise the adapter's own command/env/evidence contract without starting
Docker or contacting a model provider.
"""

from __future__ import annotations

import importlib
import sys
import tempfile
import types
import unittest
from pathlib import Path
from types import SimpleNamespace


class _ExecResult:
    def __init__(self, return_code: int = 0, stdout: str = ""):
        self.return_code = return_code
        self.stdout = stdout
        self.stderr = ""


class _FakeEnvironment:
    def __init__(self, *, fail_run: bool = False):
        self.task_env_config = SimpleNamespace(workdir="/workspace/task")
        self.calls: list[dict[str, object]] = []
        self.uploads: list[tuple[Path, str]] = []
        self.fail_run = fail_run

    async def upload_file(self, source_path: Path, target_path: str) -> None:
        self.uploads.append((Path(source_path), target_path))

    async def exec(self, command: str, **kwargs: object) -> _ExecResult:
        self.calls.append({"command": command, **kwargs})
        if "--version" in command:
            return _ExecResult(stdout="mini-harness 0.1.0")
        if "mini-harness --config" in command or "/installed-agent/mini-harness --config" in command:
            if self.fail_run:
                return _ExecResult(return_code=17)
        return _ExecResult()


def _load_adapter_with_fake_harbor():
    """Load the adapter against only the Harbor methods it actually uses."""

    harbor = types.ModuleType("harbor")
    agents = types.ModuleType("harbor.agents")
    installed = types.ModuleType("harbor.agents.installed")
    installed_base = types.ModuleType("harbor.agents.installed.base")
    environments = types.ModuleType("harbor.environments")
    environments_base = types.ModuleType("harbor.environments.base")
    models = types.ModuleType("harbor.models")
    models_agent = types.ModuleType("harbor.models.agent")
    models_context = types.ModuleType("harbor.models.agent.context")

    class FakeBaseInstalledAgent:
        def __init__(self, *args, logs_dir=None, model_name=None, extra_env=None, **kwargs):
            self.logs_dir = Path(logs_dir or tempfile.mkdtemp())
            self.model_name = model_name
            self._extra_env = dict(extra_env or {})
            self._version = None

        def _get_env(self, key: str, *alternatives: str):
            for name in (key, *alternatives):
                if name in self._extra_env:
                    return self._extra_env[name]
            return None

        async def exec_as_agent(self, environment, command, env=None, cwd=None, timeout_sec=None):
            result = await environment.exec(
                command,
                env=env,
                cwd=cwd,
                timeout_sec=timeout_sec,
            )
            if result.return_code != 0:
                raise RuntimeError(f"command failed: {result.return_code}")
            return result

        async def exec_as_root(self, environment, command, env=None, cwd=None, timeout_sec=None):
            return await self.exec_as_agent(environment, command, env, cwd, timeout_sec)

        @staticmethod
        def name():
            return "fake"

    def with_prompt_template(fn):
        return fn

    installed_base.BaseInstalledAgent = FakeBaseInstalledAgent
    installed_base.with_prompt_template = with_prompt_template
    environments_base.BaseEnvironment = object
    models_context.AgentContext = object
    sys.modules.update(
        {
            "harbor": harbor,
            "harbor.agents": agents,
            "harbor.agents.installed": installed,
            "harbor.agents.installed.base": installed_base,
            "harbor.environments": environments,
            "harbor.environments.base": environments_base,
            "harbor.models": models,
            "harbor.models.agent": models_agent,
            "harbor.models.agent.context": models_context,
        }
    )
    return importlib.import_module("benchmark.harbor.mini_harness_agent")


class AdapterTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.adapter = _load_adapter_with_fake_harbor()

    async def test_install_uploads_local_binary_and_checks_version(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "mini-harness"
            source.write_bytes(b"binary")
            environment = _FakeEnvironment()
            agent = self.adapter.MiniHarnessAgent(
                binary=str(source),
                model_name="deepseek/deepseek-chat",
                logs_dir=Path(directory) / "logs",
            )
            await agent.install(environment)
            self.assertEqual(environment.uploads[0][1], "/installed-agent/mini-harness")
            self.assertTrue(any("--version" in call["command"] for call in environment.calls))

    async def test_run_passes_only_provider_env_and_writes_metadata(self):
        environment = _FakeEnvironment()
        agent = self.adapter.MiniHarnessAgent(
            binary="mini-harness",
            model_name="deepseek/deepseek-chat",
            logs_dir=Path(tempfile.mkdtemp()),
            extra_env={
                "DEEPSEEK_API_KEY": "test-key",
                "UNRELATED_SECRET": "must-not-be-copied",
            },
        )
        context = SimpleNamespace(metadata=None)
        await agent.run("Fix this; then run tests", environment, context)
        self.assertEqual(context.metadata["execution_status"], "succeeded")
        self.assertEqual(context.metadata["task_result"], "not_verified")
        self.assertEqual(
            context.metadata["execution_status_file"],
            "/logs/agent/mini-harness-execution.json",
        )
        run_call = next(call for call in environment.calls if "mini-harness --config" in call["command"])
        self.assertEqual(run_call["env"], {"DEEPSEEK_API_KEY": "test-key"})
        self.assertEqual(run_call["cwd"], "/workspace/task")
        metadata_call = next(call for call in environment.calls if "metadata.json" in call["command"])
        self.assertIn('"provider": "deepseek"', metadata_call["command"])
        self.assertIn('"network": "inherited"', metadata_call["command"])
        self.assertIn("mini-harness.stdout", run_call["command"])
        self.assertIn("mini-harness-execution.json", run_call["command"])

    async def test_workspace_root_override_is_used_for_isolated_benchmark(self):
        environment = _FakeEnvironment()
        agent = self.adapter.MiniHarnessAgent(
            binary="mini-harness",
            model_name="deepseek/deepseek-chat",
            workspace_root="/",
            logs_dir=Path(tempfile.mkdtemp()),
        )
        context = SimpleNamespace(metadata=None)
        await agent.run("Inspect system files", environment, context)
        run_call = next(call for call in environment.calls if "mini-harness --config" in call["command"])
        self.assertEqual(run_call["cwd"], "/")
        self.assertIn("\"workspace\": \"/\"", next(
            call for call in environment.calls if "metadata.json" in call["command"]
        )["command"])

    async def test_empty_workspace_root_override_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "workspace_root"):
            self.adapter.MiniHarnessAgent(workspace_root="   ")

    async def test_nonzero_agent_exit_is_propagated(self):
        environment = _FakeEnvironment(fail_run=True)
        agent = self.adapter.MiniHarnessAgent(
            binary="mini-harness",
            model_name="deepseek/deepseek-chat",
            logs_dir=Path(tempfile.mkdtemp()),
        )
        context = SimpleNamespace(metadata=None)
        with self.assertRaisesRegex(RuntimeError, "17"):
            await agent.run("fail", environment, context)
        self.assertEqual(context.metadata["execution_status"], "failed")
        self.assertEqual(context.metadata["task_result"], "not_verified")
        self.assertTrue(any("metadata.json" in call["command"] for call in environment.calls))


if __name__ == "__main__":
    unittest.main()
