"""Hidden check: src/ uses the identifiers each Zig 0.17.0 replacement needs.

The benchmark copies this file into tests/ only after the agent exits.
Expected identifiers are stored as salted hashes.
"""

import hashlib
import re
import unittest
from pathlib import Path

SRC = Path(__file__).resolve().parent.parent / "src"

# Salted hashes of identifiers each file's 0.17.0 forms need, with the form
# they replace.
NEW = {
    "mode.zig": [
        ("82848a682c86c2663b85", "std.builtin"),
        ("531fb6e73400fe42e8cd", "OptimizeMode"),
        ("8e16f98612940c52bdc1", ".Debug"),
        ("13475a8462a9bd1f841f", ".ReleaseSafe"),
        ("c94a9d1360d9103b1520", ".ReleaseFast"),
        ("a85efc5e9b204cd113e4", ".ReleaseSmall"),
        ("0466385a8a833b87c013", "builtin.cpu, builtin.os and builtin.abi"),
        ("106c3ab268309c3207ce", "builtin.object_format"),
    ],
    "flags.zig": [
        ("441be382f694952e76d1", "IntegerBitSet"),
        ("5f70fcdaa273d5a0ff9a", "ArrayBitSet"),
        ("3b123ba2925e93e9ff41", "DynamicBitSetUnmanaged"),
        ("cb4b6399e565657a8713", "initEmpty()"),
        ("4a09a26dfc9c1aadf11a", "initFull()"),
    ],
    "report.zig": [
        ("a9bfb795f32b094fd013", "std.fmt.allocPrint"),
        ("0014988618ea86622dcb", "array multiplication"),
        ("a706c01bd243306d9b05", "std.math.divCeil"),
        ("1159a668c08b2a17d391", "std.ascii.indexOfIgnoreCase"),
        ("8b1a9834420310f90086", "std.heap.stackFallback"),
    ],
    "config.zig": [
        ("e43f16d1cb2436f9752e", "field.name"),
        ("4096f689a5c357c58b7e", "field.type"),
    ],
}



def identifiers(text):
    code = re.sub(r"//[^\n]*", "", text)
    code = re.sub(r"\\\\[^\n]*", "", code)
    code = re.sub(r'(?<!@)"(?:[^"\\\n]|\\.)*"', '""', code)
    code = re.sub(r"'(?:[^'\\\n]|\\.)*'", "''", code)
    return {
        hashlib.sha256(f"zig017:{token}".encode()).hexdigest()[:20]
        for token in re.findall(r"@?[A-Za-z_][A-Za-z0-9_]*", code)
    }


class NewFormsTest(unittest.TestCase):
    def test_new_forms_are_used(self):
        for name, hashes in NEW.items():
            found = identifiers((SRC / name).read_text())
            for digest, form in hashes:
                with self.subTest(file=name, form=form):
                    self.assertIn(
                        digest,
                        found,
                        f"src/{name} lacks the Zig 0.17.0 replacement for {form}",
                    )


if __name__ == "__main__":
    unittest.main()
