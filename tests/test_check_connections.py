import importlib.util
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / "scripts"))
spec = importlib.util.spec_from_file_location("checks", Path(__file__).parents[1] / "scripts/check_connections.py")
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)


class AuthenticationTests(unittest.TestCase):
    def test_login_reports_are_not_generation_checks(self):
        self.assertEqual(checks.authentication("codex", True, ""), "reported_authenticated")
        self.assertEqual(checks.authentication("codex", False, ""), "unknown")
        self.assertEqual(checks.authentication("claude", True, '{"loggedIn":true}'), "reported_authenticated")
        self.assertEqual(checks.authentication("claude", False, '{"loggedIn":false}'), "reported_signed_out")
        self.assertEqual(checks.authentication("qoder", True, '{"logged_in":false}'), "reported_signed_out")

    def test_unsupported_or_conflicting_auth_output_is_unknown(self):
        for provider, success, output in [("claude", True, 'invalid'), ("claude", True, '[]'),
                                           ("claude", True, '{"loggedIn":"true"}'),
                                           ("claude", False, '{"loggedIn":true}'),
                                           ("qoder", True, '{}'), ("qoder", False, '{"logged_in":false}')]:
            self.assertEqual(checks.authentication(provider, success, output), "unknown")
