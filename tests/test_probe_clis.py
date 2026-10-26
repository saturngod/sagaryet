import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("probe", Path(__file__).parents[1] / "scripts/probe_clis.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def test_terminal_failure_is_not_success(self):
        value = probe.summarize_stream('{"type":"result","subtype":"error_during_execution","is_error":true}')
        self.assertFalse(value["successful_terminal"])

    def test_only_answer_deltas_count(self):
        lines = [
            '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"private"}}}',
            '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"မင်္ဂလာပါ"}}}',
            '{"type":"result","subtype":"success","result":"မင်္ဂလာပါ"}',
        ]
        value = probe.summarize_stream('\n'.join(lines))
        self.assertEqual(value["text_delta_count"], 1)
        self.assertTrue(value["successful_terminal"])
        self.assertNotIn("private", str(value))
        self.assertNotIn("မင်္ဂလာပါ", str(value))

    def test_malformed_record(self):
        self.assertEqual(probe.summarize_stream('invalid\n[]')["invalid_records"], 2)

    def test_timeout_kills_process(self):
        with tempfile.TemporaryDirectory() as cwd:
            result = probe.run([sys.executable, "-c", "import time; time.sleep(30)"], cwd, timeout=.1)
            self.assertEqual(result["termination"], "timeout")
            self.assertNotEqual(result["exit_code"], 0)

    def test_split_utf8_stdout_and_stderr(self):
        with tempfile.TemporaryDirectory() as cwd:
            code = "import os; b='မင်္ဂလာပါ'.encode(); [os.write(1, bytes([c])) for c in b]; os.write(2,b'error')"
            result = probe.run([sys.executable, "-c", code], cwd)
            self.assertEqual(result["stdout"], "မင်္ဂလာပါ")
            self.assertEqual(result["stderr"], "error")

    def test_output_bound(self):
        with tempfile.TemporaryDirectory() as cwd:
            result = probe.run([sys.executable, "-c", "import os; os.write(1,b'x'*2000000)"], cwd)
            self.assertEqual(result["termination"], "output_limit")
            self.assertLessEqual(len(result["stdout"]), probe.LIMIT)


if __name__ == '__main__':
    unittest.main()
