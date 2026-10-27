"""Static checks that src/ no longer uses forms Zig 0.17.0 removed.

Zig is not installed, so these tests read the source. The benchmark also
runs hidden checks that each 0.17.0 replacement is in place.
"""

import re
import unittest
from pathlib import Path

SRC = Path(__file__).resolve().parent.parent / "src"

# Forms that 0.17.0 removed or deprecated, per file.
OLD = {
    "mode.zig": [
        (r"OptimizeMode", "std.builtin.OptimizeMode"),
        (r"std\.builtin\b", "std.builtin"),
        (r"\.(Debug|ReleaseSafe|ReleaseFast|ReleaseSmall)\b", "OptimizeMode tag names"),
        (r"builtin\.(cpu|os|abi|object_format)\b", "builtin.cpu/os/abi/object_format"),
    ],
    "flags.zig": [
        (r"IntegerBitSet", "std.bit_set.IntegerBitSet"),
        (r"ArrayBitSet", "std.bit_set.ArrayBitSet"),
        (r"DynamicBitSetUnmanaged", "std.DynamicBitSetUnmanaged"),
        (r"\binit(Empty|Full)\(\)", "initEmpty() and initFull()"),
    ],
    "report.zig": [
        (r"allocPrint", "std.fmt.allocPrint"),
        (r"\*\*", "array multiplication"),
        (r"math\.divCeil", "std.math.divCeil"),
        (r"indexOfIgnoreCase", "std.ascii.indexOfIgnoreCase"),
        (r"stackFallback|StackFallbackAllocator", "std.heap.stackFallback"),
        (r"\.get\(\)", "the stack fallback's get()"),
        (r"errdefer\s*\|", "errdefer capture"),
        (r"void\s*\{\s*\}", "void{}"),
    ],
    "config.zig": [
        (r"@\"struct\"\.fields", "@typeInfo(T).@\"struct\".fields"),
    ],
}


def without_comments(text):
    return re.sub(r"//[^\n]*", "", text)


class PortTest(unittest.TestCase):
    def test_old_forms_are_gone(self):
        for name, forms in OLD.items():
            code = without_comments((SRC / name).read_text())
            for pattern, form in forms:
                with self.subTest(file=name, form=form):
                    self.assertIsNone(
                        re.search(pattern, code), f"src/{name} still uses {form}"
                    )


if __name__ == "__main__":
    unittest.main()
