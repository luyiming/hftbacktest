"""Exact market-data conversion. Backtesting is provided by the Rust crate."""

from ._hftbacktest import convert_fuse

__all__ = ["convert_fuse"]
