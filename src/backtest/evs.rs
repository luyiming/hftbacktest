#[derive(Clone, Copy)]
#[repr(C, align(32))]
pub struct EventIntent {
    pub timestamp: i64,
    pub asset_no: usize,
    pub kind: EventIntentKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
#[repr(usize)]
pub enum EventIntentKind {
    LocalData = 0,
    LocalOrder = 1,
    ExchData = 2,
    ExchOrder = 3,
}

impl EventIntentKind {
    const ALL: [Self; 4] = [
        Self::LocalData,
        Self::LocalOrder,
        Self::ExchData,
        Self::ExchOrder,
    ];
}

/// Manages the event timestamps to determine the next event to be processed.
pub struct EventSet {
    timestamps: Vec<[i64; 4]>,
}

impl EventSet {
    pub(crate) fn snapshot(&self) -> Self {
        Self {
            timestamps: self.timestamps.clone(),
        }
    }

    /// Constructs an instance of `EventSet`.
    pub fn new(num_assets: usize) -> Self {
        if num_assets == 0 {
            panic!();
        }
        Self {
            timestamps: vec![[i64::MAX; 4]; num_assets],
        }
    }

    /// Returns the next event to be processed, which has the earliest timestamp.
    pub fn next(&self) -> Option<EventIntent> {
        let mut next = None;
        for (asset_no, timestamps) in self.timestamps.iter().enumerate() {
            for kind in EventIntentKind::ALL {
                let timestamp = timestamps[kind as usize];
                if timestamp != i64::MAX
                    && next
                        .as_ref()
                        .is_none_or(|next: &EventIntent| timestamp < next.timestamp)
                {
                    next = Some(EventIntent {
                        timestamp,
                        asset_no,
                        kind,
                    });
                }
            }
        }
        next
    }

    #[inline]
    fn update(&mut self, asset_no: usize, kind: EventIntentKind, timestamp: i64) {
        self.timestamps[asset_no][kind as usize] = timestamp;
    }

    #[inline]
    pub fn update_local_data(&mut self, asset_no: usize, timestamp: i64) {
        self.update(asset_no, EventIntentKind::LocalData, timestamp);
    }

    #[inline]
    pub fn update_local_order(&mut self, asset_no: usize, timestamp: i64) {
        self.update(asset_no, EventIntentKind::LocalOrder, timestamp);
    }

    #[inline]
    pub fn update_exch_data(&mut self, asset_no: usize, timestamp: i64) {
        self.update(asset_no, EventIntentKind::ExchData, timestamp);
    }

    #[inline]
    pub fn update_exch_order(&mut self, asset_no: usize, timestamp: i64) {
        self.update(asset_no, EventIntentKind::ExchOrder, timestamp);
    }

    #[inline]
    fn invalidate(&mut self, asset_no: usize, kind: EventIntentKind) {
        self.update(asset_no, kind, i64::MAX);
    }

    #[inline]
    pub fn invalidate_local_data(&mut self, asset_no: usize) {
        self.invalidate(asset_no, EventIntentKind::LocalData);
    }

    #[inline]
    pub fn invalidate_exch_data(&mut self, asset_no: usize) {
        self.invalidate(asset_no, EventIntentKind::ExchData);
    }
}

#[cfg(test)]
mod tests {
    use super::{EventIntentKind, EventSet};

    #[test]
    fn next_preserves_order_and_snapshot_after_updates() {
        let mut events = EventSet::new(2);
        assert!(events.next().is_none());

        events.update_exch_order(1, 12);
        events.update_local_data(0, 12);
        let snapshot = events.snapshot();

        assert_eq!(
            events
                .next()
                .map(|event| (event.timestamp, event.asset_no, event.kind)),
            Some((12, 0, EventIntentKind::LocalData))
        );

        events.invalidate_local_data(0);
        events.update_exch_data(1, 8);
        assert_eq!(
            events
                .next()
                .map(|event| (event.timestamp, event.asset_no, event.kind)),
            Some((8, 1, EventIntentKind::ExchData))
        );
        assert_eq!(
            snapshot
                .next()
                .map(|event| (event.timestamp, event.asset_no, event.kind)),
            Some((12, 0, EventIntentKind::LocalData))
        );

        events.update_local_order(0, 7);
        assert_eq!(
            events
                .next()
                .map(|event| (event.timestamp, event.asset_no, event.kind)),
            Some((7, 0, EventIntentKind::LocalOrder))
        );
    }
}
