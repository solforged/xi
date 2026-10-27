"""Checks the shape of pins.toml.

The benchmark also runs hidden checks that each pin is the current release.
"""

import re
import tomllib
import unittest
from pathlib import Path

PINS = Path(__file__).resolve().parent.parent / "pins.toml"

ARTIFACTS = {
    "zig": "zig-x86_64-linux-{version}.tar.xz",
    "go": "go{version}.linux-amd64.tar.gz",
    "rust": "rust-{version}-x86_64-unknown-linux-gnu.tar.xz",
    "python": "Python-{version}.tar.xz",
}


class PinsTest(unittest.TestCase):
    def test_pins_are_well_formed(self):
        pins = tomllib.loads(PINS.read_text())
        self.assertEqual(set(pins), set(ARTIFACTS))
        for tool, artifact in ARTIFACTS.items():
            with self.subTest(tool=tool):
                pin = pins[tool]
                self.assertEqual(pin["artifact"], artifact)
                self.assertRegex(pin["version"], r"^\d+\.\d+\.\d+$")
                self.assertRegex(pin["sha256"], r"^[0-9a-f]{64}$")


if __name__ == "__main__":
    unittest.main()
