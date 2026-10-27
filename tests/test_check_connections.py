import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

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


class OpenCodeTests(unittest.TestCase):
    def test_plain_catalog_and_empty_credentials_are_not_auth_verdicts(self):
        self.assertEqual(checks.parse_opencode_models("provider/model\nprovider/model\nother/family/model\nwarning text\n/model"),
                         ["provider/model", "other/family/model"])
        self.assertEqual(checks.authentication("opencode", True, '{"logged_in":true}'), "unknown")
        outputs = [dict(exit_code=0, termination=None, stdout="opencode v2.0.32", stderr=""),
                   dict(exit_code=0, termination=None, stdout="No authenticated integrations", stderr=""),
                   dict(exit_code=0, termination=None, stdout="local/model", stderr="")]
        with patch.object(checks.shutil, "which", return_value="/bin/opencode"), patch.object(checks, "run", side_effect=outputs):
            result = checks.check("opencode", ["opencode"], "/tmp")
        self.assertTrue(result["runnable"])
        self.assertEqual(result["credential_listing"], "available")
        self.assertEqual(result["authentication"], "unknown")
        self.assertEqual(result["generation"], "not_tested")
        self.assertEqual(result["models"], ["local/model"])
        self.assertNotIn("No authenticated integrations", str(result))

    def test_failed_credential_listing_still_discovers_environment_models(self):
        version = dict(exit_code=0, termination=None, stdout="opencode v2.0.32", stderr="")
        catalog = dict(exit_code=0, termination=None, stdout="environment/model", stderr="")
        with patch.object(checks.shutil, "which", return_value="/bin/opencode"), patch.object(checks, "run", side_effect=[version, OSError(), catalog]):
            result = checks.check("opencode", ["opencode"], "/tmp")
        self.assertEqual(result["credential_listing"], "failed")
        self.assertEqual(result["models"], ["environment/model"])
