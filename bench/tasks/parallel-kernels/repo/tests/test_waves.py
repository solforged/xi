import unittest
from packages.waves import plan
class Waves(unittest.TestCase):
    def test_diamond_disconnected_duplicates_and_generators(self):
        graph={'deploy':iter(['test','lint','test']),'test':iter(['compile']),'lint':iter(['compile']),'compile':iter([]),'docs':iter([])}
        self.assertEqual(plan(graph),[['compile','docs'],['lint','test'],['deploy']])
        original={'a':[],'b':['a']}; self.assertEqual(plan(original),[['a'],['b']]); self.assertEqual(original,{'a':[],'b':['a']})
        self.assertEqual(plan({}),[])
    def test_errors(self):
        for graph in [{'a':['missing']},{'a':['a']},{'a':['b'],'b':['a'],'c':[]},{'a':[],'b':['c'],'c':['b']}]:
            with self.assertRaises(ValueError): plan(graph)
    def test_long_chain(self):
        graph={f'job{i:03}':([f'job{i-1:03}'] if i else []) for i in range(150)}
        self.assertEqual(plan(graph),[[f'job{i:03}'] for i in range(150)])
