use crate::{
    backtest::{
        assettype::AssetType,
        models::{FeeModel, Fill},
    },
    types::{Order, StateValues},
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
    pub fn apply_fill(&mut self, order: &Order) {
        self.apply_fill_qty_price(
            order,
            order.exec_qty,
            order
                .latest_exec_price()
                .to_f64()
                .expect("fill price should fit f64"),
        );
    }

    #[inline]
    pub(crate) fn apply_fill_qty_price(
        &mut self,
        order: &Order,
        exec_qty: Decimal,
        exec_price: f64,
    ) {
        let exec_qty_f64 = exec_qty.to_f64().expect("fill quantity should fit f64");
        let amount = self.asset_type.amount(exec_price, exec_qty_f64);
        let fill = Fill {
            qty: exec_qty_f64,
            price: exec_price,
            value: amount,
            maker: order.maker,
            side: order.side,
        };
        self.state_values.position +=
            exec_qty * Decimal::from(*AsRef::<f64>::as_ref(&order.side) as i64);
        self.state_values.balance -= amount * AsRef::<f64>::as_ref(&order.side);
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
