"""Convert Tardis CSV inputs to versioned, exact fixed-point NPZ data in Rust.

``convert_fuse`` requires an output filename and returns the number of stored
events. Price and quantity scales are fixed at eight decimal places. Tick size,
lot size, floating-point conversion, and legacy data formats are not supported.

Set ``eod_filename`` and ``eod_timestamp`` together to write the final fused
order book as a replayable NPZ snapshot at the given nanosecond timestamp.
``initial_snapshot_filename`` restores such a snapshot before fusing a day.
"""

from ..._hftbacktest import convert_fuse

__all__ = ["convert_fuse"]
