import argparse
from decimal import Decimal as D
import unittest
from estimate_cost import estimate, nonnegative, percentage


class CostTests(unittest.TestCase):
    def cost(self, errors="1000", analyses="1", cache="0", input_rate="0.30", output_rate="2.50"):
        return estimate(*map(D, (errors, analyses, cache, "2000", "600", input_rate, output_rate)))

    def test_default_example(self):
        self.assertEqual(self.cost(), (D(1000), D("0.0021"), D("2.10")))

    def test_repeated_analysis_and_local_cache(self):
        self.assertEqual(self.cost(analyses="3", cache="40")[2], D("3.78"))

    def test_no_requests_cost_nothing(self):
        self.assertEqual(self.cost(errors="0")[2], 0)
        self.assertEqual(self.cost(cache="100")[2], 0)

    def test_editable_rates(self):
        self.assertEqual(self.cost(input_rate="1", output_rate="2")[2], D("3.20"))

    def test_rejects_invalid_inputs(self):
        for value in ("-1", "nan", "inf", "text"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                nonnegative(value)
        with self.assertRaises(argparse.ArgumentTypeError):
            percentage("101")


if __name__ == "__main__":
    unittest.main()
