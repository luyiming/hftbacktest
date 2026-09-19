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
        for price in ["1e-9", "0.000000001", "92233720368.54775808"]:
            self.write_trade(price)
            with self.assertRaisesRegex(ValueError, "record 2 field price"):
                self.convert()
            self.assertEqual(self.output.read_bytes(), original)
        self.assertEqual(sorted(path.name for path in self.root.iterdir()),
                         ["depth.csv.gz", "events.npz", "trades.csv.gz"])

    def test_scientific_values_are_stored_exactly(self):
        with gzip.open(self.depth, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,is_snapshot,side,price,amount\n"
                "binance,TEST,1,2,true,bid,1e-7,2.5E-7\n"
                "binance,TEST,1,2,true,ask,2e-7,1e-7\n"
            )
        self.write_trade("1.5e-7")
        self.convert()
        with np.load(self.output, allow_pickle=False) as archive:
            rows = archive["data"]
            values = set(zip(rows["px"], rows["qty"]))
            self.assertTrue({(10, 25), (20, 10), (15, 1)}.issubset(values))

    def test_eod_seeds_next_day_bbo_backoff(self):
        with gzip.open(self.depth, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,is_snapshot,side,price,amount\n"
                "binance,TEST,1,2,true,bid,101,2\n"
                "binance,TEST,1,2,true,ask,103,3\n"
            )
        self.write_trade("102")
        eod = self.root / "previous.eod.npz"
        EXTENSION.convert_fuse(
            trades_filename=self.trades,
            depth_filename=self.depth,
            output_filename=self.output,
            eod_filename=eod,
            eod_timestamp=5_000,
        )
        with np.load(eod, allow_pickle=False) as archive:
            self.assertEqual(set(archive.files), {"data", "metadata"})
            self.assertEqual(len(archive["data"]), 2)

        with gzip.open(self.depth, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,is_snapshot,side,price,amount\n"
            )
        with gzip.open(self.trades, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,id,side,price,amount\n"
            )
        ticker = self.root / "book_ticker.csv.gz"
        with gzip.open(ticker, "wt") as stream:
            stream.write(
                "exchange,symbol,timestamp,local_timestamp,ask_amount,ask_price,bid_price,bid_amount\n"
                "binance,TEST,6,7,3,103,100,4\n"
            )
        next_output = self.root / "next.npz"
        EXTENSION.convert_fuse(
            trades_filename=self.trades,
            depth_filename=self.depth,
            book_ticker_filename=ticker,
            output_filename=next_output,
            initial_snapshot_filename=eod,
        )
        with np.load(next_output, allow_pickle=False) as archive:
            rows = archive["data"]
            deleted = rows[(rows["px"] == 10_100_000_000) & (rows["qty"] == 0)]
            self.assertEqual(len(deleted), 1)
            self.assertEqual(int(deleted["ev"][0] & 0xff), 1)


if __name__ == "__main__":
    unittest.main()
