use rust_decimal::Decimal;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickSizeChange {
    pub effective_from: i64,
    pub tick_size: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickSizeSchedule {
    changes: Vec<TickSizeChange>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TickSizeError {
    #[error("tick size schedule is empty")]
    Empty,
    #[error("tick size must be positive at timestamp {0}")]
    NonPositive(i64),
    #[error("tick size timestamps must be strictly increasing")]
    Unordered,
    #[error("no tick size rule covers timestamp {timestamp}; first rule starts at {first}")]
    Uncovered { timestamp: i64, first: i64 },
}

impl TickSizeSchedule {
    pub fn new(changes: Vec<TickSizeChange>) -> Result<Self, TickSizeError> {
        if changes.is_empty() {
            return Err(TickSizeError::Empty);
        }
        for change in &changes {
            if change.tick_size <= Decimal::ZERO {
                return Err(TickSizeError::NonPositive(change.effective_from));
            }
        }
        if changes
            .windows(2)
            .any(|pair| pair[0].effective_from >= pair[1].effective_from)
        {
            return Err(TickSizeError::Unordered);
        }
        Ok(Self { changes })
    }

    /// Queries the exchange processing time; a change applies at its exact timestamp.
    pub fn at(&self, timestamp: i64) -> Result<Decimal, TickSizeError> {
        let index = self
            .changes
            .partition_point(|change| change.effective_from <= timestamp);
        if index == 0 {
            return Err(TickSizeError::Uncovered {
                timestamp,
                first: self.changes[0].effective_from,
            });
        }
        Ok(self.changes[index - 1].tick_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_is_left_closed_and_last_rule_does_not_expire() {
        let schedule = TickSizeSchedule::new(vec![
            TickSizeChange {
                effective_from: 10,
                tick_size: Decimal::new(1, 2),
            },
            TickSizeChange {
                effective_from: 20,
                tick_size: Decimal::new(1, 1),
            },
        ])
        .expect("valid schedule should build");
        assert_eq!(
            schedule.at(9),
            Err(TickSizeError::Uncovered {
                timestamp: 9,
                first: 10
            })
        );
        assert_eq!(schedule.at(10), Ok(Decimal::new(1, 2)));
        assert_eq!(schedule.at(19), Ok(Decimal::new(1, 2)));
        assert_eq!(schedule.at(20), Ok(Decimal::new(1, 1)));
        assert_eq!(schedule.at(i64::MAX), Ok(Decimal::new(1, 1)));
    }

    #[test]
    fn rejects_invalid_configuration() {
        assert_eq!(TickSizeSchedule::new(vec![]), Err(TickSizeError::Empty));
        assert_eq!(
            TickSizeSchedule::new(vec![TickSizeChange {
                effective_from: 0,
                tick_size: Decimal::ZERO
            }]),
            Err(TickSizeError::NonPositive(0))
        );
        for second in [-1, 0] {
            assert_eq!(
                TickSizeSchedule::new(vec![
                    TickSizeChange {
                        effective_from: 0,
                        tick_size: Decimal::ONE
                    },
                    TickSizeChange {
                        effective_from: second,
                        tick_size: Decimal::ONE
                    },
                ]),
                Err(TickSizeError::Unordered)
            );
        }
    }
}
