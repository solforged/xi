import copy
import unittest
from packages.pointer import resolve
class Pointer(unittest.TestCase):
    def test_escaped_and_empty_keys(self):
        doc={'':{'a/b':[{'~1':7}]},'01':'key','-':'dash','~':'tilde'}; saved=copy.deepcopy(doc)
        self.assertIs(resolve(doc,''),doc)
        self.assertEqual(resolve(doc,'//a~1b/0/~01'),7)
        self.assertEqual(resolve(doc,'/01'),'key'); self.assertEqual(resolve(doc,'/-'),'dash'); self.assertEqual(resolve(doc,'/~0'),'tilde')
        self.assertEqual(doc,saved)
    def test_rejections(self):
        doc={'a':[3]}
        for pointer in ['a','/missing','/a/','/a/00','/a/01','/a/-','/a/-1','/a/+0','/a/١','/a/1','/a/0/x','/~','/~2','/a/~01',None]:
            with self.subTest(pointer=pointer), self.assertRaises(ValueError): resolve(doc,pointer)
