"""Run after `cargo build -p py-hftbacktest` from the vendor workspace."""

import gzip
import importlib.util
from pathlib import Path
import tempfile
import unittest

import numpy as np


LIBRARY = Path(__file__).resolve().parents[2] / "target/debug/libhftbacktest.so"
SPEC = importlib.util.spec_from_file_location("_hftbacktest", LIBRARY)
EXTENSION = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXTENSION)


class ConversionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.trades = self.root / "trades.csv.gz"
        self.depth = self.root / "depth.csv.gz"
        self.output = self.root / "events.npz"
        with gzip.open(self.depth, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,is_snapshot,side,price,amount\n"
                "binance,TEST,1,2,true,bid,100.00000001,0.00000001\n"
                "binance,TEST,1,2,true,ask,100.00000002,0.00000002\n"
            )

    def write_trade(self, price):
        with gzip.open(self.trades, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,id,side,price,amount\n"
                f"binance,TEST,3,4,id,buy,{price},0.00000001\n"
            )

    def convert(self):
        return EXTENSION.convert_fuse(
            trades_filename=self.trades,
            depth_filename=self.depth,
            output_filename=self.output,
        )

    def test_npz_contains_exact_integers_and_metadata(self):
        self.write_trade("92233720368.54775807")
        count = self.convert()
        with np.load(self.output, allow_pickle=False) as archive:
            self.assertEqual(set(archive.files), {"data", "metadata"})
            self.assertEqual(archive["metadata"].tolist(), [(8, 8)])
            data = archive["data"]
            self.assertEqual(len(data), count)
            self.assertEqual(data.dtype.names, ("ev", "exch_ts", "local_ts", "px", "qty"))
            self.assertEqual(data.dtype["px"], np.dtype("<i8"))
            self.assertEqual(data.dtype["qty"], np.dtype("<i8"))
            self.assertIn(np.iinfo(np.int64).max, data["px"])
            self.assertIn(10000000001, data["px"])
            self.assertIn(10000000002, data["px"])

    def test_bad_record_does_not_replace_existing_output(self):
        self.write_trade("1")
        self.convert()
        original = self.output.read_bytes()
        for price in ["1e-8", "0.000000001", "92233720368.54775808"]:
            self.write_trade(price)
            with self.assertRaisesRegex(ValueError, "record 2 field price"):
                self.convert()
            self.assertEqual(self.output.read_bytes(), original)
        self.assertEqual(sorted(path.name for path in self.root.iterdir()),
                         ["depth.csv.gz", "events.npz", "trades.csv.gz"])


if __name__ == "__main__":
    unittest.main()
