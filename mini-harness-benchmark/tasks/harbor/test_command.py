import unittest

from benchmark.harbor.command import (
    benchmark_config,
    run_command,
    split_model_name,
)


class CommandTests(unittest.TestCase):
    def test_model_provider_is_parsed(self):
        self.assertEqual(split_model_name("deepseek/deepseek-chat").provider, "deepseek")
        self.assertEqual(split_model_name("deepseek/deepseek-chat").model, "deepseek-chat")

    def test_provider_can_be_explicit_for_bare_model(self):
        self.assertEqual(
            split_model_name("deepseek-chat", "deepseek").provider,
            "deepseek",
        )

    def test_conflicting_provider_is_rejected(self):
        with self.assertRaises(ValueError):
            split_model_name("openai/gpt-5", "deepseek")

    def test_command_quotes_prompt_and_keeps_prompt_last(self):
        command = run_command(
            binary="/installed-agent/mini-harness",
            config_path="/tmp/config.toml",
            event_log="/logs/agent/events.jsonl",
            workspace="/workspace/task",
            selection=split_model_name("deepseek/deepseek-chat"),
            instruction="Fix the issue; then run `pytest -q`",
        )
        self.assertIn("--provider deepseek", command)
        self.assertTrue(command.endswith("'Fix the issue; then run `pytest -q`'"))

    def test_config_is_non_interactive_but_network_is_explicit(self):
        config = benchmark_config(
            max_steps=10,
            max_turn_time_ms=30_000,
            network="inherited",
        )
        self.assertIn('bash = "allow"', config)
        with self.assertRaises(ValueError):
            benchmark_config(
                max_steps=10,
                max_turn_time_ms=30_000,
                network="offline_strict",
            )
        self.assertNotIn("API_KEY", config)


if __name__ == "__main__":
    unittest.main()
