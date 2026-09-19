Data
====

See :doc:`Data Preparation <tutorials/Data Preparation>` for collecting and converting
feed data.

Format
------

Converted NPZ archives contain `data.npy` and `metadata.npy`. The metadata
records a price scale and a size scale, both fixed at eight decimal places.
The structured `data.npy` array has five fields, in this order:

* ev (u64): Event flags.
* exch_ts (i64): Timestamp when the event occurred at the exchange.
* local_ts (i64): Timestamp when the event was received locally.
* px (i64): Fixed-point price.
* qty (i64): Fixed-point quantity.

In memory, `Event` exposes `px` and `qty` as decimal values. Feed events
do not carry order IDs; backtest orders have their own separate IDs.

Validation
----------

Events marked `EXCH_EVENT` must be ordered by exchange timestamp, and events
marked `LOCAL_EVENT` must be ordered by local timestamp. An event can have
both flags. Exchange timestamps must not exceed local timestamps; if the
clocks are misaligned, correct the feed latency before backtesting.
