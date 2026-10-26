import unittest
from batches import chunks
from measures import weighted_mean


class Batches(unittest.TestCase):
    def test_partial_generator(self):
        self.assertEqual(list(chunks(iter(range(5)), 2)), [[0, 1], [2, 3], [4]])

    def test_empty_and_exact(self):
        self.assertEqual(list(chunks([], 3)), [])
        self.assertEqual(list(chunks([1, 2], 1)), [[1], [2]])

    def test_invalid_sizes(self):
        for size in (0, -1):
            with self.subTest(size=size), self.assertRaises(ValueError):
                list(chunks([], size))


class Measures(unittest.TestCase):
    def test_weighted_generator(self):
        self.assertAlmostEqual(weighted_mean(iter([(10, 1), (20, 3)])), 17.5)

    def test_zero_weight_is_ignored(self):
        self.assertEqual(weighted_mean([(999, 0), (8, 2)]), 8)

    def test_invalid_weights(self):
        for pairs in ([], [(4, 0)], [(4, -1), (8, 2)]):
            with self.subTest(pairs=pairs), self.assertRaises(ValueError):
                weighted_mean(pairs)
