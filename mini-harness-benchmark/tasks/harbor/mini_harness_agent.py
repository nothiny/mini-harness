"""Harbor adapter for the mini-harness binary.

The adapter is intentionally thin: Harbor owns task environments and
verification; this class only installs/locates the Rust binary, disables
interactive approvals for an isolated trial, starts ``mini-harness run``, and
leaves the native JSONL event log under Harbor's agent logs directory.

Harbor is an optional dependency.  Import this module from a Harbor job using
``--agent-import-path``; normal mini-harness development does not import it.
"""

from __future__ import annotations

import json
import shlex
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .command import benchmark_config, run_command, split_model_name


class MiniHarnessAgent(BaseInstalledAgent):
    """Run one mini-harness trial inside Harbor's task environment.

    ``binary`` may be an executable already present in the image (for example
    ``mini-harness``), or a local path supplied to Harbor on the host.  Local
    binaries are uploaded to ``/installed-agent/mini-harness`` during setup.
    """

    def __init__(
        self,
        binary: str = "mini-harness",
        provider: str | None = None,
        network: str = "inherited",
        max_steps: int = 20,
        max_turn_time_ms: int = 600_000,
        run_timeout_sec: int = 1_800,
        *args: Any,
        workspace_root: str | None = None,
        **kwargs: Any,
    ) -> None:
        if not isinstance(binary, str) or not binary.strip():
            raise ValueError("binary must be a non-empty command or local path")
        if network != "inherited":
            raise ValueError(
                "network must be inherited; this adapter does not install "
                "network isolation"
            )
        if workspace_root is not None and not workspace_root.strip():
            raise ValueError("workspace_root must be a non-empty path when set")
        self.binary_source = Path(binary).expanduser() if _looks_like_path(binary) else None
        self.binary_name = binary
        self.provider_override = provider
        self.network = network
        self.workspace_root_override = workspace_root.strip() if workspace_root else None
        self.max_steps = int(max_steps)
        self.max_turn_time_ms = int(max_turn_time_ms)
        self.run_timeout_sec = int(run_timeout_sec)
        if self.max_steps <= 0 or self.max_turn_time_ms <= 0 or self.run_timeout_sec <= 0:
            raise ValueError("benchmark budgets must be positive")
        super().__init__(*args, **kwargs)

    @staticmethod
    def name() -> str:
        return "mini-harness"

    def version(self) -> str | None:
        return self._version or "development"

    def get_version_command(self) -> str | None:
        command = self._remote_binary()
        return f"{shlex.quote(command)} --version"

    async def install(self, environment: BaseEnvironment) -> None:
        remote = self._remote_binary()
        if self.binary_source is not None:
            source = self.binary_source.resolve()
            if not source.is_file():
                raise FileNotFoundError(f"mini-harness binary not found: {source}")
            await environment.upload_file(source, remote)
            await self.exec_as_root(
                environment,
                command=f"chmod 0755 {shlex.quote(remote)}",
            )
        else:
            # The image is expected to provide a pinned binary.  Do not fetch
            # arbitrary code during benchmark setup.
            await self.exec_as_agent(
                environment,
                command=f"command -v {shlex.quote(self.binary_name)}",
            )
        await self.exec_as_agent(environment, command=f"{shlex.quote(remote)} --version")

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        selection = split_model_name(self.model_name, self.provider_override)
        workspace = await self._workspace(environment)
        event_log = "/logs/agent/mini-harness-events.jsonl"
        config_path = "/tmp/mini-harness-benchmark.toml"
        config = benchmark_config(
            max_steps=self.max_steps,
            max_turn_time_ms=self.max_turn_time_ms,
            network=self.network,
        )
        marker = "MINI_HARNESS_CONFIG_EOF"
        metadata = json.dumps(
            {
                "agent": self.name(),
                "model": selection.model,
                "provider": selection.provider,
                "network": self.network,
                "max_steps": self.max_steps,
                "max_turn_time_ms": self.max_turn_time_ms,
                "permissions": {
                    "read_workspace": "allow",
                    "edit_workspace": "allow",
                    "bash": "allow",
                },
                "workspace": workspace,
                "event_log": event_log,
                "config_path": config_path,
                "binary": self._remote_binary(),
            },
            sort_keys=True,
        )
        metadata_marker = "MINI_HARNESS_METADATA_EOF"
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p /logs/agent && "
                f"cat > {shlex.quote(config_path)} <<'{marker}'\n"
                f"{config}"
                f"{marker}\n"
                f"cat > /logs/agent/mini-harness-metadata.json <<'{metadata_marker}'\n"
                f"{metadata}\n"
                f"{metadata_marker}\n"
            ),
        )

        command = run_command(
            binary=self._remote_binary(),
            config_path=config_path,
            event_log=event_log,
            workspace=workspace,
            selection=selection,
            instruction=instruction,
        )
        # Harbor's --agent-env values are held in BaseInstalledAgent's extra
        # environment.  Pass only provider/model connection variables rather
        # than copying unrelated host secrets into the trial command.
        env = _provider_environment(self)
        # Keep status separate from Harbor's verifier reward. This metadata is
        # also visible in ATIF/job records when Harbor syncs AgentContext.
        _update_context_metadata(
            context,
            {
                "execution_status": "running",
                "task_result": "not_verified",
                "event_log": event_log,
                "stdout": "/logs/agent/mini-harness.stdout",
                "execution_status_file": "/logs/agent/mini-harness-execution.json",
            },
        )
        # Keep a structured harness status even when the command exits before
        # it can emit its aggregate JSON. PIPESTATUS is read inside an
        # explicit bash so the adapter preserves the command's exit code while
        # teeing output for Harbor.
        execution_file = "/logs/agent/mini-harness-execution.json"
        wrapped = (
            f"{command} 2>&1 | tee /logs/agent/mini-harness.stdout; "
            f"status=${{PIPESTATUS[0]}}; "
            "if [ \"$status\" -eq 0 ]; then outcome=succeeded; "
            "else outcome=failed; fi; "
            f"printf '{{\"execution_status\":\"%s\",\"exit_code\":%s}}\\n' "
            f'\"$outcome\" \"$status\" > {shlex.quote(execution_file)}; '
            "exit \"$status\""
        )
        try:
            await self.exec_as_agent(
                environment,
                command=f"bash -o pipefail -c {shlex.quote(wrapped)}",
                env=env or None,
                cwd=workspace,
                timeout_sec=self.run_timeout_sec,
            )
        except Exception:
            _update_context_metadata(context, {"execution_status": "failed"})
            raise
        _update_context_metadata(context, {"execution_status": "succeeded"})

    async def _workspace(self, environment: BaseEnvironment) -> str:
        if self.workspace_root_override is not None:
            return self.workspace_root_override
        configured = getattr(environment.task_env_config, "workdir", None)
        if configured:
            return str(configured)
        result = await environment.exec("pwd", timeout_sec=10)
        workspace = (result.stdout or "").strip()
        if not workspace:
            raise RuntimeError("Harbor environment did not report a workspace path")
        return workspace

    def _remote_binary(self) -> str:
        if self.binary_source is not None:
            return "/installed-agent/mini-harness"
        return self.binary_name


def _update_context_metadata(context: Any, updates: dict[str, str]) -> None:
    """Record harness execution facts without pretending to verify the task."""

    if not hasattr(context, "metadata"):
        return
    current = getattr(context, "metadata", None)
    if not isinstance(current, dict):
        current = {}
    current.update(updates)
    context.metadata = current


def _looks_like_path(value: str) -> bool:
    return value.startswith(("/", "./", "../", "~")) or "/" in value


def _provider_environment(agent: MiniHarnessAgent) -> dict[str, str]:
    """Copy only model endpoint credentials configured for this agent."""

    names = (
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
    )
    values: dict[str, str] = {}
    for name in names:
        value = agent._get_env(name)
        if value is not None:
            values[name] = value
    return values


# A small import-time smoke helper for environments that discover classes by
# module import and do not instantiate them.  It has no runtime side effects.
__all__ = ["MiniHarnessAgent"]
