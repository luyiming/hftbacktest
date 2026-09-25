use crate::{
    backtest::{assettype::AssetType, models::FeeModel},
    types::{OrderFill, Side, StateValues},
};
use rust_decimal::Decimal;

#[derive(Clone, Debug)]
pub struct State<AT, FM>
where
    AT: AssetType,
    FM: FeeModel,
{
    pub state_values: StateValues,
    pub asset_type: AT,
    pub fee_model: FM,
}

impl<AT, FM> State<AT, FM>
where
    AT: AssetType,
    FM: FeeModel,
{
    pub fn new(asset_type: AT, fee_model: FM) -> Self {
        Self {
            state_values: StateValues {
                position: Decimal::ZERO,
                balance: Decimal::ZERO,
                fee: Decimal::ZERO,
                num_trades: 0,
                trading_volume: Decimal::ZERO,
                trading_value: Decimal::ZERO,
            },
            fee_model,
            asset_type,
        }
    }

    #[inline]
    pub fn apply_fill(&mut self, side: Side, order_fill: &OrderFill) {
        let trading_value = self.asset_type.amount(order_fill.price, order_fill.qty);
        self.state_values.position += order_fill.qty * Decimal::from(side.sign());
        self.state_values.balance -= trading_value * Decimal::from(side.sign());
        self.state_values.fee += self.fee_model.amount(order_fill, side, trading_value);
        self.state_values.num_trades += 1;
        self.state_values.trading_volume += order_fill.qty;
        self.state_values.trading_value += trading_value;
    }

    #[inline]
    pub fn equity(&self, mid: Decimal) -> Decimal {
        self.asset_type.equity(
            mid,
            self.state_values.balance,
            self.state_values.position,
            self.state_values.fee,
        )
    }

    #[inline]
    pub fn values(&self) -> &StateValues {
        &self.state_values
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::{
        backtest::{
            assettype::LinearAsset,
            models::{CommonFees, TradingValueFeeModel},
            state::State,
        },
        types::{OrderFill, Side, StateValues},
    };

    #[test]
    fn accumulates_account_values_without_losing_decimal_precision() {
        let mut state = State::new(
            LinearAsset::new(Decimal::ONE),
            TradingValueFeeModel::new(CommonFees::new(Decimal::new(1, 3), Decimal::new(1, 3))),
        );

        state.apply_fill(
            Side::Buy,
            &OrderFill {
                price: Decimal::new(10025, 2),
                qty: Decimal::new(3, 1),
                exch_timestamp: 1,
                is_maker: true,
            },
        );
        state.apply_fill(
            Side::Sell,
            &OrderFill {
                price: Decimal::new(10125, 2),
                qty: Decimal::new(1, 1),
                exch_timestamp: 2,
                is_maker: false,
            },
        );

        assert_eq!(
            state.values(),
            &StateValues {
                position: Decimal::new(2, 1),
                balance: Decimal::new(-1995, 2),
                fee: Decimal::new(402, 4),
                num_trades: 2,
                trading_volume: Decimal::new(4, 1),
                trading_value: Decimal::new(402, 1),
            }
        );
        assert_eq!(state.equity(Decimal::from(102)), Decimal::new(4098, 4));
    }
}
