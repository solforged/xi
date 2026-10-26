import hashlib
import importlib.util
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
EXPECTED=json.loads((Path(__file__).with_name('digests.json')).read_text())

class Catalog(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory=tempfile.TemporaryDirectory()
        cls.root=Path(cls.directory.name)
        for name in ['rules.py','catalog.json']: shutil.copyfile(ROOT/name,cls.root/name)
        spec=importlib.util.spec_from_file_location('standalone_rules',cls.root/'rules.py')
        cls.module=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)
    @classmethod
    def tearDownClass(cls): cls.directory.cleanup()

    def test_all_policies(self):
        catalog=json.loads((self.root/'catalog.json').read_text())
        self.assertEqual(set(catalog),set(EXPECTED))
        for region,operations in EXPECTED.items():
            self.assertEqual(set(catalog[region]),set(operations))
            for operation,digest in operations.items():
                with self.subTest(region=region,operation=operation):
                    policy=self.module.policy(region,operation)
                    self.assertEqual(set(policy),{'quota','burst','window','ttl','attempts','backoff','grace','audit','order','overflow','unit','enabled'})
                    for name in ['quota','burst','window','ttl','attempts','backoff','grace']: self.assertIs(type(policy[name]),int)
                    self.assertIs(type(policy['enabled']),bool)
                    actual=hashlib.sha256(json.dumps(policy,sort_keys=True,separators=(',',':')).encode()).hexdigest()
                    self.assertEqual(actual,digest)
                    self.assertEqual(policy,catalog[region][operation])
                    policy['quota']=-1
                    self.assertNotEqual(self.module.policy(region,operation)['quota'],-1)

    def test_unknown_names(self):
        for region,operation in [('region-16','archive'),('region-00','unknown'),('','')]:
            with self.assertRaises(KeyError): self.module.policy(region,operation)

    def test_orientation_sample(self):
        self.assertEqual(self.module.policy("region-00","archive"),{'quota': 82, 'burst': 20, 'window': 60, 'ttl': 120, 'attempts': 4, 'backoff': 3, 'grace': 20, 'audit': 'none', 'order': 'fifo', 'overflow': 'reject', 'unit': 'batch', 'enabled': True})
