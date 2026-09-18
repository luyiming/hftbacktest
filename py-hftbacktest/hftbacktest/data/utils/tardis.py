"""Convert Tardis CSV inputs to versioned, exact fixed-point NPZ data in Rust.

``convert_fuse`` requires an output filename and returns the number of stored
events. Price and quantity scales are fixed at eight decimal places. Tick size,
lot size, floating-point conversion, and legacy data formats are not supported.
"""

from ..._hftbacktest import convert_fuse

__all__ = ["convert_fuse"]
