import unittest
import labels
from report import report, total_line


class ReportTests(unittest.TestCase):
    def test_public_name(self):
        self.assertTrue(callable(getattr(labels, "format_label", None)))
        self.assertFalse(hasattr(labels, "render_label"))

    def test_rows(self):
        self.assertEqual(report([(" Ada ", 2), ("", 0), ("Élodie", 1)]), "Ada: 2\n(unnamed): 0\nÉlodie: 1")

    def test_empty(self):
        self.assertEqual(report([]), "")

    def test_total(self):
        self.assertEqual(total_line(5), "Total: 5")
