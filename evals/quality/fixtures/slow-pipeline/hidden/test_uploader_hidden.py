import unittest

from client.uploader import chunks


class ChunksHidden(unittest.TestCase):
    def test_every_length_and_size(self):
        for size in range(1, 9):
            for length in range(0, 30):
                items = list(range(length))
                batches = chunks(items, size)
                self.assertEqual([item for batch in batches for item in batch], items, (length, size))
                self.assertEqual(len(batches), -(-length // size), (length, size))
                self.assertTrue(all(1 <= len(batch) <= size for batch in batches), (length, size))
                self.assertTrue(all(len(batch) == size for batch in batches[:-1]), (length, size))

    def test_size_larger_than_items(self):
        self.assertEqual(chunks(["a", "b"], 5), [["a", "b"]])

    def test_empty(self):
        self.assertEqual(chunks([], 3), [])

    def test_invalid_size(self):
        with self.assertRaises(ValueError):
            chunks([1], 0)


if __name__ == "__main__":
    unittest.main()
