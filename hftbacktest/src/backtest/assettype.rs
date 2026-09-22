use rust_decimal::Decimal;

/// Calculates the value amount and the equity according to the asset type.
pub trait AssetType {
    /// Calculates the value amount.
    fn amount(&self, price: Decimal, qty: Decimal) -> Decimal;

    /// Calculates the equity.
    fn equity(&self, price: Decimal, balance: Decimal, position: Decimal, fee: Decimal) -> Decimal;
}

/// The common type of asset where the contract's notional value is linear to the quote currency.
#[derive(Clone)]
pub struct LinearAsset {
    contract_size: Decimal,
}

impl LinearAsset {
    /// Constructs an instance of `LinearAsset`.
    pub fn new(contract_size: Decimal) -> Self {
        Self { contract_size }
    }
}

impl AssetType for LinearAsset {
    fn amount(&self, exec_price: Decimal, qty: Decimal) -> Decimal {
        self.contract_size * exec_price * qty
    }

    fn equity(&self, price: Decimal, balance: Decimal, position: Decimal, fee: Decimal) -> Decimal {
        balance + self.contract_size * position * price - fee
    }
}

/// The contract’s notional value is denominated in the quote currency.
#[derive(Clone)]
pub struct InverseAsset {
    contract_size: Decimal,
}

impl InverseAsset {
    /// Constructs an instance of `InverseAsset`.
    pub fn new(contract_size: Decimal) -> Self {
        Self { contract_size }
    }
}

impl AssetType for InverseAsset {
    fn amount(&self, exec_price: Decimal, qty: Decimal) -> Decimal {
        self.contract_size * qty / exec_price
    }

    fn equity(&self, price: Decimal, balance: Decimal, position: Decimal, fee: Decimal) -> Decimal {
        -balance - self.contract_size * position / price - fee
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::backtest::assettype::{AssetType, InverseAsset};

    #[test]
    fn inverse_asset_uses_decimal_division_for_amount_and_equity() {
        let asset = InverseAsset::new(Decimal::from(100));

        assert_eq!(
            asset.amount(Decimal::from(4), Decimal::from(3)),
            Decimal::from(75)
        );
        assert_eq!(
            asset.equity(
                Decimal::from(5),
                Decimal::from(-75),
                Decimal::from(3),
                Decimal::new(5, 1),
            ),
            Decimal::new(145, 1)
        );
    }
}
