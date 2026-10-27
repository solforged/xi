"""Hidden check: pins.toml names the releases current on 2026-10-27.

The benchmark copies this file into tests/ only after the agent exits.

Expected values are stored as salted hashes of the version, and of the
version with the artifact's SHA-256.
"""

import hashlib
import tomllib
import unittest
from pathlib import Path

PINS = Path(__file__).resolve().parent.parent / "pins.toml"

EXPECTED = {
    "zig": ("d91c2e799816b003e8cb", "18ea4978235fe2335f66"),
    "go": ("0ecd0d76743d4810a79c", "9b2ece1e6a7e46e86c30"),
    "rust": ("297191dc2409b507f355", "2cc73d0c8388d9f34cb5"),
    "python": ("4e56a818647cb1ce9605", "df0f01a2995832a57b94"),
}


def digest(tool, value):
    return hashlib.sha256(f"pins:{tool}:{value}".encode()).hexdigest()[:20]


class PinsTest(unittest.TestCase):
    def test_pins_are_current(self):
        pins = tomllib.loads(PINS.read_text())
        for tool, (version_digest, pin_digest) in EXPECTED.items():
            with self.subTest(tool=tool):
                pin = pins[tool]
                version = str(pin["version"]).strip()
                sha256 = str(pin["sha256"]).strip().lower()
                self.assertEqual(
                    digest(tool, version),
                    version_digest,
                    f"{tool}: {version} is not the latest stable release",
                )
                self.assertEqual(
                    digest(tool, f"{version}/{sha256}"),
                    pin_digest,
                    f"{tool}: sha256 does not match {pin['artifact']}",
                )


if __name__ == "__main__":
    unittest.main()
