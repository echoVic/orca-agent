import unittest

from summary import Summary, summarize


class SummaryTests(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(summarize([]), Summary(0, 0, 0, 0))

    def test_large_values(self):
        self.assertEqual(summarize([2**31 - 1, 2**31 - 1, 1]).sum, 2 * (2**31 - 1) + 1)

    def test_min_max_count(self):
        self.assertEqual(summarize([5, -3, 9, 5]), Summary(16, -3, 9, 4))

    def test_single(self):
        self.assertEqual(summarize([-7]), Summary(-7, -7, -7, 1))


if __name__ == "__main__":
    unittest.main()
