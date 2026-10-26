import unittest
from packages.retry import schedule
class Retry(unittest.TestCase):
    def test_backoff_server_minimum_and_boundary(self):
        self.assertEqual(schedule(iter([(2,None),(1,20),(0,None),(0,None)]),base=3,cap=10,budget=36),[0,5,26,36])
        self.assertEqual(schedule([(0,100),(0,0)],base=2,cap=5,budget=100),[0,100])
        self.assertEqual(schedule([],base=10,cap=2,budget=0),[0])
    def test_validation_even_after_cutoff(self):
        for kwargs in [dict(base=0,cap=1,budget=1),dict(base=True,cap=1,budget=1),dict(base=1,cap=0,budget=1),dict(base=1,cap=1,budget=-1)]:
            with self.assertRaises(ValueError): schedule([],**kwargs)
        for item in [(-1,None),(1,-1),(True,None),(1,False),(1.5,None),(1,),None]:
            with self.assertRaises(ValueError): schedule([(100,None),item],base=1,cap=2,budget=0)
    def test_long_capped_stream(self):
        self.assertEqual(schedule(((0,None) for _ in range(20000)),base=1,cap=2,budget=4),[0,1,3])
