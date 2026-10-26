import random
import unittest
from packages.overlay import paint
class Overlay(unittest.TestCase):
    def test_splits_coalesces_and_huge_bounds(self):
        self.assertEqual(paint([(0,10,'a'),(3,7,'b'),(4,6,'a'),(10,10,'x')]),[(0,3,'a'),(3,4,'b'),(4,6,'a'),(6,7,'b'),(7,10,'a')])
        self.assertEqual(paint(iter([(0,10**19,None),(5,10**19,''),(5,10**19,None)])),[(0,10**19,None)])
        for item in [(-1,2,'a'),(4,3,'a')]:
            with self.assertRaises(ValueError): paint([item])
    def test_deterministic_point_oracle(self):
        rng=random.Random(418)
        for _ in range(100):
            items=[]; points={}
            for _ in range(20):
                a,b=sorted([rng.randrange(21),rng.randrange(21)]); label=rng.choice(['a','b',None,''])
                items.append((a,b,label))
                for p in range(a,b): points[p]=label
            result=paint(iter(items))
            actual={p:label for a,b,label in result for p in range(a,b)}
            self.assertEqual(actual,points)
            for a,b in zip(result,result[1:]): self.assertTrue(a[1]<=b[0] and not (a[1]==b[0] and a[2]==b[2]))
