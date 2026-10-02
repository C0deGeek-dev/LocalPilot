import unittest

from roman import to_roman


class ToRomanTest(unittest.TestCase):
    def test_known_values(self):
        cases = {1: "I", 4: "IV", 9: "IX", 14: "XIV", 40: "XL", 90: "XC",
                 400: "CD", 900: "CM", 1994: "MCMXCIV", 3999: "MMMCMXCIX"}
        for n, expected in cases.items():
            self.assertEqual(to_roman(n), expected)

    def test_out_of_range(self):
        for n in (0, -1, 4000):
            with self.assertRaises(ValueError):
                to_roman(n)

    def test_non_integers(self):
        for n in (1.5, "3", None, True, False):
            with self.subTest(n=repr(n)):
                with self.assertRaises(ValueError):
                    to_roman(n)


if __name__ == "__main__":
    unittest.main()
