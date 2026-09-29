import unittest

from client.uploader import chunks


class ChunksTest(unittest.TestCase):
    def test_exact_multiple(self):
        self.assertEqual(chunks([1, 2, 3, 4], 2), [[1, 2], [3, 4]])

    def test_last_batch_may_be_shorter(self):
        self.assertEqual(chunks([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]])


if __name__ == "__main__":
    unittest.main()
