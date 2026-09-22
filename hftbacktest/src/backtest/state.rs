use crate::{
    backtest::{
        assettype::AssetType,
        models::{FeeModel, Fill},
    },
    types::{OrderFill, Side, StateValues},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};

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
                balance: 0.0,
                fee: 0.0,
                num_trades: 0,
                trading_volume: 0.0,
                trading_value: 0.0,
            },
            fee_model,
            asset_type,
        }
    }

    #[inline]
    pub fn apply_fill(&mut self, side: Side, order_fill: &OrderFill) {
        let exec_qty_f64 = order_fill
            .qty
            .to_f64()
            .expect("fill quantity should fit f64");
        let exec_price = order_fill
            .price
            .to_f64()
            .expect("fill price should fit f64");
        let amount = self.asset_type.amount(exec_price, exec_qty_f64);
        let fill = Fill {
            qty: exec_qty_f64,
            price: exec_price,
            value: amount,
            maker: order_fill.is_maker,
            side,
        };
        self.state_values.position += order_fill.qty * Decimal::from(side.sign());
        self.state_values.balance -= amount * side.as_f64();
        self.state_values.fee += self.fee_model.amount(&fill);
        self.state_values.num_trades += 1;
        self.state_values.trading_volume += exec_qty_f64;
        self.state_values.trading_value += amount;
    }

    #[inline]
    pub fn equity(&self, mid: f64) -> f64 {
        self.asset_type.equity(
            mid,
            self.state_values.balance,
            self.state_values
                .position
                .to_f64()
                .expect("position should fit f64"),
            self.state_values.fee,
        )
    }

    #[inline]
    pub fn values(&self) -> &StateValues {
        &self.state_values
    }
}
