use crate::prelude::{OrderFill, Side};
use rust_decimal::Decimal;

/// Common transaction fees
/// Fee calculation is determined by the fee model.
#[derive(Clone)]
pub struct CommonFees {
    /// Fee for adding liquidity (maker order).
    maker_fee: Decimal,
    /// Fee for removing liquidity (taker order).
    taker_fee: Decimal,
}

impl CommonFees {
    /// Constructs `CommonFees`.
    pub fn new(maker_fee: Decimal, taker_fee: Decimal) -> Self {
        Self {
            maker_fee,
            taker_fee,
        }
    }
}

/// Directional fees, such as stamp duty, are typically charged based on the transaction value in
/// addition to the common transaction fees.
#[derive(Clone)]
pub struct DirectionalFees {
    /// The common transaction fees
    common_fees: CommonFees,
    /// Buyer fee based on the transaction value
    buyer_fee: Decimal,
    /// Seller fee based on the transaction value
    seller_fee: Decimal,
}

impl DirectionalFees {
    /// Constructs `DirectionalFees`.
    pub fn new(common_fees: CommonFees, buyer_fee: Decimal, seller_fee: Decimal) -> Self {
        Self {
            common_fees,
            buyer_fee,
            seller_fee,
        }
    }
}

/// Provides the fee.
pub trait FeeModel {
    /// Calculates the fee for an applied fill using its order side and asset-denominated trading
    /// value.
    fn amount(&self, fill: &OrderFill, side: Side, trading_value: Decimal) -> Decimal;
}

/// Fee based on the transaction value,
/// with the rate depending on whether the order is a maker or taker.
#[derive(Clone)]
pub struct TradingValueFeeModel<Fees> {
    fees: Fees,
}

impl<Fees> TradingValueFeeModel<Fees> {
    /// Constructs `TradingValueFeeModel`.
    pub fn new(fees: Fees) -> Self {
        Self { fees }
    }
}

impl FeeModel for TradingValueFeeModel<CommonFees> {
    fn amount(&self, fill: &OrderFill, _side: Side, trading_value: Decimal) -> Decimal {
        if fill.is_maker {
            self.fees.maker_fee * trading_value
        } else {
            self.fees.taker_fee * trading_value
        }
    }
}

impl FeeModel for TradingValueFeeModel<DirectionalFees> {
    fn amount(&self, fill: &OrderFill, side: Side, trading_value: Decimal) -> Decimal {
        match (fill.is_maker, side) {
            (true, Side::Buy) => {
                (self.fees.common_fees.maker_fee + self.fees.buyer_fee) * trading_value
            }
            (false, Side::Buy) => {
                (self.fees.common_fees.taker_fee + self.fees.buyer_fee) * trading_value
            }
            (true, Side::Sell) => {
                (self.fees.common_fees.maker_fee + self.fees.seller_fee) * trading_value
            }
            (false, Side::Sell) => {
                (self.fees.common_fees.taker_fee + self.fees.seller_fee) * trading_value
            }
        }
    }
}

/// Fee based on the transaction quantity,
/// with the rate depending on whether the order is a maker or taker.
#[derive(Clone)]
pub struct TradingQtyFeeModel<Fees> {
    fees: Fees,
}

impl<Fees> TradingQtyFeeModel<Fees> {
    /// Constructs `TradingQtyFeeModel`.
    pub fn new(fees: Fees) -> Self {
        Self { fees }
    }
}
impl FeeModel for TradingQtyFeeModel<CommonFees> {
    fn amount(&self, fill: &OrderFill, _side: Side, _trading_value: Decimal) -> Decimal {
        if fill.is_maker {
            self.fees.maker_fee * fill.qty
        } else {
            self.fees.taker_fee * fill.qty
        }
    }
}

impl FeeModel for TradingQtyFeeModel<DirectionalFees> {
    fn amount(&self, fill: &OrderFill, side: Side, trading_value: Decimal) -> Decimal {
        match (fill.is_maker, side) {
            (true, Side::Buy) => {
                self.fees.common_fees.maker_fee * fill.qty + self.fees.buyer_fee * trading_value
            }
            (false, Side::Buy) => {
                self.fees.common_fees.taker_fee * fill.qty + self.fees.buyer_fee * trading_value
            }
            (true, Side::Sell) => {
                self.fees.common_fees.maker_fee * fill.qty + self.fees.seller_fee * trading_value
            }
            (false, Side::Sell) => {
                self.fees.common_fees.taker_fee * fill.qty + self.fees.seller_fee * trading_value
            }
        }
    }
}

/// Flat fee per trade
#[derive(Clone)]
pub struct FlatPerTradeFeeModel<Fees> {
    fees: Fees,
}
impl<Fees> FlatPerTradeFeeModel<Fees> {
    /// Constructs `FlatPerTradeFeeModel`.
    pub fn new(fees: Fees) -> Self {
        Self { fees }
    }
}

impl FeeModel for FlatPerTradeFeeModel<CommonFees> {
    fn amount(&self, fill: &OrderFill, _side: Side, _trading_value: Decimal) -> Decimal {
        if fill.is_maker {
            self.fees.maker_fee
        } else {
            self.fees.taker_fee
        }
    }
}
