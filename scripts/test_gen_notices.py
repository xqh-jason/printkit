#!/usr/bin/env python3
"""Unit tests for the first-party classification in gen-notices.py."""

import importlib.util
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("gen-notices.py")
SPEC = importlib.util.spec_from_file_location("gen_notices", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FirstPartyTest(unittest.TestCase):
    def test_cli_application_is_first_party(self):
        self.assertTrue(MODULE.first_party({"name": "printkit"}))

    def test_workspace_crates_are_first_party(self):
        for name in (
            "printkit-core",
            "printkit-platform-win",
            "printkit-barcode",
        ):
            self.assertTrue(MODULE.first_party({"name": name}), name)

    def test_workspace_npm_packages_are_first_party(self):
        for name in ("@printkit/i18n", "@printkit/sdk"):
            self.assertTrue(MODULE.first_party({"name": name}), name)

    def test_similarly_named_third_parties_stay_third_party(self):
        for name in ("printkitten", "printkitx", "not-printkit"):
            self.assertFalse(MODULE.first_party({"name": name}), name)


if __name__ == "__main__":
    unittest.main()
