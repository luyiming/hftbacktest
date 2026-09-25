use rust_decimal::{
    Decimal,
    prelude::{FromPrimitive, ToPrimitive},
};
use std::marker::PhantomData;

use crate::{
    depth::MarketDepth,
    types::{Order, Side},
};

/// Estimates the queue position of each resting order.
///
/// The exchange owns one [`State`](QueueModel::State) per resting order and passes it back to the
/// same model for every transition. Implementations must return a nonnegative executable quantity
/// from [`trade`](QueueModel::trade).
pub trait QueueModel<MD>
where
    MD: MarketDepth,
{
    /// Per-order state maintained while an order rests in the book.
    type State: Clone + Send;

    /// Initialize the queue position and other necessary values for estimation.
    /// This function is called when the exchange model accepts the new order.
    fn new_order(&self, order: &Order, depth: &MD) -> Self::State;

    /// Adjusts the estimation values when market trades occur at the same price and returns the
    /// quantity available to execute after the queue ahead has been consumed.
    fn trade(&self, order: &Order, state: &mut Self::State, qty: Decimal, depth: &MD) -> Decimal;

    /// Adjusts the estimation values after market depth changes at the same price.
    fn depth(
        &self,
        order: &Order,
        state: &mut Self::State,
        prev_qty: Decimal,
        new_qty: Decimal,
        depth: &MD,
    );
}

/// Provides a conservative queue position model, where your order's queue position advances only
/// when trades occur at the same price level.
#[derive(Clone)]
pub struct RiskAdverseQueueModel<MD>(PhantomData<MD>);

impl<MD> RiskAdverseQueueModel<MD> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<MD> QueueModel<MD> for RiskAdverseQueueModel<MD>
where
    MD: MarketDepth,
{
    type State = Decimal;

    fn new_order(&self, order: &Order, depth: &MD) -> Self::State {
        if order.side == Side::Buy {
            depth.bid_qty_at_price(order.price)
        } else {
            depth.ask_qty_at_price(order.price)
        }
    }

    fn trade(
        &self,
        _order: &Order,
        front_q_qty: &mut Self::State,
        qty: Decimal,
        _depth: &MD,
    ) -> Decimal {
        *front_q_qty -= qty;
        if *front_q_qty < Decimal::ZERO {
            let exec = -*front_q_qty;
            *front_q_qty = Decimal::ZERO;
            exec
        } else {
            Decimal::ZERO
        }
    }

    fn depth(
        &self,
        _order: &Order,
        front_q_qty: &mut Self::State,
        _prev_qty: Decimal,
        new_qty: Decimal,
        _depth: &MD,
    ) {
        *front_q_qty = (*front_q_qty).min(new_qty);
    }
}

/// Stores the values needed for queue position estimation and adjustment for [`ProbQueueModel`].
#[derive(Clone, Debug)]
pub struct QueuePos {
    front_q_qty: Decimal,
    cum_trade_qty: Decimal,
}

impl Default for QueuePos {
    fn default() -> Self {
        Self {
            front_q_qty: Decimal::ZERO,
            cum_trade_qty: Decimal::ZERO,
        }
    }
}

/// Provides the probability of a decrease behind the order's queue position.
pub trait Probability {
    /// Returns the probability based on the quantity ahead and behind the order.
    fn prob(&self, front: f64, back: f64) -> f64;
}

/// Provides a probability-based queue position model as described in
/// * `<https://quant.stackexchange.com/questions/3782/how-do-we-estimate-position-of-our-order-in-order-book>`
/// * `<https://rigtorp.se/2013/06/08/estimating-order-queue-position.html>`
///
/// Your order's queue position advances when a trade occurs at the same price level or the quantity
/// at the level decreases. The advancement in queue position depends on the probability based on
/// the relative queue position. To avoid double counting the quantity decrease caused by trades,
/// all trade quantities occurring at the level before the book quantity changes will be subtracted
/// from the book quantity changes.
pub struct ProbQueueModel<P, MD>
where
    P: Probability,
{
    prob: P,
    _md_marker: PhantomData<MD>,
}

impl<P, MD> ProbQueueModel<P, MD>
where
    P: Probability,
{
    /// Constructs an instance of `ProbQueueModel` with a [`Probability`] model.
    pub fn new(prob: P) -> Self {
        Self {
            prob,
            _md_marker: Default::default(),
        }
    }
}

impl<P, MD> QueueModel<MD> for ProbQueueModel<P, MD>
where
    P: Probability,
    MD: MarketDepth,
{
    type State = QueuePos;

    fn new_order(&self, order: &Order, depth: &MD) -> Self::State {
        let mut q = QueuePos::default();
        if order.side == Side::Buy {
            q.front_q_qty = depth.bid_qty_at_price(order.price);
        } else {
            q.front_q_qty = depth.ask_qty_at_price(order.price);
        }
        q
    }

    fn trade(&self, _order: &Order, q: &mut Self::State, qty: Decimal, _depth: &MD) -> Decimal {
        q.front_q_qty -= qty;
        q.cum_trade_qty += qty;
        if q.front_q_qty < Decimal::ZERO {
            let exec = -q.front_q_qty;
            q.front_q_qty = Decimal::ZERO;
            exec
        } else {
            Decimal::ZERO
        }
    }

    fn depth(
        &self,
        _order: &Order,
        q: &mut Self::State,
        prev_qty: Decimal,
        new_qty: Decimal,
        _depth: &MD,
    ) {
        let mut chg = prev_qty - new_qty;
        // In order to avoid duplicate order queue position adjustment, subtract queue position
        // change by trades.
        chg -= q.cum_trade_qty;
        // Reset, as quantity change by trade should be already reflected in qty.
        q.cum_trade_qty = Decimal::ZERO;
        // For an increase of the quantity, front queue doesn't change by the quantity change.
        if chg < Decimal::ZERO {
            q.front_q_qty = q.front_q_qty.min(new_qty);
            return;
        }

        let front = q.front_q_qty;
        let back = prev_qty - front;

        let mut prob = self.prob.prob(
            front.to_f64().expect("queue quantity should fit f64"),
            back.to_f64().expect("queue quantity should fit f64"),
        );
        if !prob.is_finite() {
            prob = 1.0;
        }
        prob = prob.clamp(0.0, 1.0);

        let prob = Decimal::from_f64(prob).expect("queue probability should be finite");
        let est_front =
            front - (Decimal::ONE - prob) * chg + (back - prob * chg).min(Decimal::ZERO);
        q.front_q_qty = est_front.min(new_qty);
    }
}

/// This probability model uses a power function `f(x) = x ** n` to adjust the probability which is
/// calculated as `f(back) / (f(back) + f(front))`.
pub struct PowerProbQueueFunc {
    n: f64,
}

impl PowerProbQueueFunc {
    /// Constructs an instance of `PowerProbQueueFunc`.
    pub fn new(n: f64) -> Self {
        Self { n }
    }

    fn f(&self, x: f64) -> f64 {
        x.powf(self.n)
    }
}

impl Probability for PowerProbQueueFunc {
    fn prob(&self, front: f64, back: f64) -> f64 {
        self.f(back) / (self.f(back) + self.f(front))
    }
}

/// This probability model uses a logarithmic function `f(x) = log(1 + x)` to adjust the
/// probability which is calculated as `f(back) / (f(back) + f(front))`.
#[derive(Default)]
pub struct LogProbQueueFunc(());

impl LogProbQueueFunc {
    /// Constructs an instance of `LogProbQueueFunc`.
    pub fn new() -> Self {
        Default::default()
    }

    fn f(&self, x: f64) -> f64 {
        (1.0 + x).ln()
    }
}

impl Probability for LogProbQueueFunc {
    fn prob(&self, front: f64, back: f64) -> f64 {
        self.f(back) / (self.f(back) + self.f(front))
    }
}

/// This probability model uses a logarithmic function `f(x) = log(1 + x)` to adjust the
/// probability which is calculated as `f(back) / f(back + front)`.
#[derive(Default)]
pub struct LogProbQueueFunc2(());

impl LogProbQueueFunc2 {
    /// Constructs an instance of `LogProbQueueFunc2`.
    pub fn new() -> Self {
        Default::default()
    }

    fn f(&self, x: f64) -> f64 {
        (1.0 + x).ln()
    }
}

impl Probability for LogProbQueueFunc2 {
    fn prob(&self, front: f64, back: f64) -> f64 {
        self.f(back) / self.f(back + front)
    }
}

/// This probability model uses a power function `f(x) = x ** n` to adjust the probability which is
/// calculated as `f(back) / f(back + front)`.
pub struct PowerProbQueueFunc2 {
    n: f64,
}

impl PowerProbQueueFunc2 {
    /// Constructs an instance of `PowerProbQueueFunc2`.
    pub fn new(n: f64) -> Self {
        Self { n }
    }

    fn f(&self, x: f64) -> f64 {
        x.powf(self.n)
    }
}

impl Probability for PowerProbQueueFunc2 {
    fn prob(&self, front: f64, back: f64) -> f64 {
        self.f(back) / self.f(back + front)
    }
}

/// This probability model uses a power function `f(x) = x ** n` to adjust the probability which is
/// calculated as `1 - f(front / (front + back))`.
pub struct PowerProbQueueFunc3 {
    n: f64,
}

impl PowerProbQueueFunc3 {
    /// Constructs an instance of `PowerProbQueueFunc3`.
    pub fn new(n: f64) -> Self {
        Self { n }
    }

    fn f(&self, x: f64) -> f64 {
        x.powf(self.n)
    }
}

impl Probability for PowerProbQueueFunc3 {
    fn prob(&self, front: f64, back: f64) -> f64 {
        1.0 - self.f(front / (front + back))
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::{QueueModel, RiskAdverseQueueModel};
    use crate::{
        depth::{BTreeMarketDepth, L2MarketDepth},
        types::{OrdType, Order, Side, TimeInForce},
    };

    #[test]
    fn risk_adverse_fill_uses_exact_quantity_without_lot_rounding() {
        let mut depth = BTreeMarketDepth::new();
        let price = Decimal::new(10025, 2);
        depth.update_bid_depth(price, Decimal::new(125, 2), 0);
        let order = Order::new(
            1,
            price,
            Decimal::ONE,
            Side::Buy,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        let model = RiskAdverseQueueModel::<BTreeMarketDepth>::new();
        let mut state = model.new_order(&order, &depth);
        assert_eq!(
            model.trade(&order, &mut state, Decimal::new(130, 2), &depth),
            Decimal::new(5, 2)
        );
    }
}
