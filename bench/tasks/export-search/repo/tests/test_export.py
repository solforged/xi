import unittest
from exporter import export_ids

DOCS = [
    dict(id="live", archived=False, deleted=False, favorite=True, draft=False, shared=True, size=200, tags=["x"]),
    dict(id="old", archived=True, deleted=False, favorite=False, draft=True, shared=False, size=10, tags=[]),
    dict(id="gone", archived=True, deleted=True, favorite=True, draft=False, shared=True, size=200, tags=["x"]),
    dict(id="trash", archived=False, deleted=True, favorite=False, draft=True, shared=False, size=10, tags=[]),
]


class ExportTests(unittest.TestCase):
    def test_archived_only(self):
        self.assertEqual(export_ids(DOCS, "archived-only"), ["old"])

    def test_all_other_modes(self):
        expected = {
            "all": ["live", "old"], "active-only": ["live"], "deleted-only": ["gone", "trash"],
            "favorites": ["live"], "drafts": ["old"], "published": ["live"], "shared": ["live"],
            "private": ["old"], "large": ["live"], "small": ["old"], "tagged": ["live"],
        }
        for mode, ids in expected.items():
            with self.subTest(mode=mode):
                self.assertEqual(export_ids(DOCS, mode), ids)

    def test_unknown_mode(self):
        with self.assertRaises(KeyError):
            export_ids(DOCS, "unknown")
