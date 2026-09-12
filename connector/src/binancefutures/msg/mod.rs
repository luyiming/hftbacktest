use hftbacktest::types::{OrdType, PriceMatch, Side, Status, TimeInForce};
use serde::{
    Deserialize,
    Deserializer,
    de::{Error, Unexpected},
};

#[allow(dead_code)]
pub mod rest;
#[allow(dead_code)]
pub mod stream;

fn from_str_to_side<'de, D>(deserializer: D) -> Result<Side, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    match s {
        "BUY" => Ok(Side::Buy),
        "SELL" => Ok(Side::Sell),
        s => Err(Error::invalid_value(Unexpected::Other(s), &"BUY or SELL")),
    }
}

fn from_str_to_status<'de, D>(deserializer: D) -> Result<Status, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    match s {
        "NEW" => Ok(Status::New),
        "PARTIALLY_FILLED" => Ok(Status::PartiallyFilled),
        "FILLED" => Ok(Status::Filled),
        "CANCELED" => Ok(Status::Canceled),
        // "REJECTED" => Ok(Status::Rejected),
        "EXPIRED" => Ok(Status::Expired),
        // "EXPIRED_IN_MATCH" => Ok(Status::ExpiredInMatch),
        s => Err(Error::invalid_value(
            Unexpected::Other(s),
            &"NEW,PARTIALLY_FILLED,FILLED,CANCELED,EXPIRED",
        )),
    }
}

fn from_str_to_type<'de, D>(deserializer: D) -> Result<OrdType, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    match s {
        "LIMIT" => Ok(OrdType::Limit),
        "MARKET" => Ok(OrdType::Market),
        // "STOP" => Ok(OrdType::StopLimit),
        // "TAKE_PROFIT" => Ok(OrdType::TakeProfitLimit),
        // "STOP_MARKET" => Ok(OrdType::StopMarket),
        // "TAKE_PROFIT_MARKET" => Ok(OrdType::TakeProfitMarket),
        // "TRAILING_STOP_MARKET" => Ok(OrdType::TrailingStopMarket),
        s => Err(Error::invalid_value(Unexpected::Other(s), &"LIMIT,MARKET")),
    }
}

fn from_str_to_tif<'de, D>(deserializer: D) -> Result<TimeInForce, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    match s {
        "GTC" => Ok(TimeInForce::GTC),
        "IOC" => Ok(TimeInForce::IOC),
        "FOK" => Ok(TimeInForce::FOK),
        "GTX" => Ok(TimeInForce::GTX),
        // "GTD" => Ok(TimeInForce::GTD),
        s => Err(Error::invalid_value(
            Unexpected::Other(s),
            &"GTC,IOC,FOK,GTX",
        )),
    }
}

fn from_str_to_price_match<'de, D>(deserializer: D) -> Result<PriceMatch, D::Error>
where
    D: Deserializer<'de>,
{
    let value: &str = Deserialize::deserialize(deserializer)?;
    match value {
        "NONE" => Ok(PriceMatch::None),
        "OPPONENT" => Ok(PriceMatch::Opponent),
        "OPPONENT_5" => Ok(PriceMatch::Opponent5),
        "OPPONENT_10" => Ok(PriceMatch::Opponent10),
        "OPPONENT_20" => Ok(PriceMatch::Opponent20),
        "QUEUE" => Ok(PriceMatch::Queue),
        "QUEUE_5" => Ok(PriceMatch::Queue5),
        "QUEUE_10" => Ok(PriceMatch::Queue10),
        "QUEUE_20" => Ok(PriceMatch::Queue20),
        value => Err(Error::invalid_value(
            Unexpected::Other(value),
            &"NONE,OPPONENT,OPPONENT_5,OPPONENT_10,OPPONENT_20,QUEUE,QUEUE_5,QUEUE_10,QUEUE_20",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct PriceMatchField {
        #[serde(deserialize_with = "from_str_to_price_match")]
        value: PriceMatch,
    }

    #[test]
    fn parses_all_price_match_values() {
        for (value, expected) in [
            ("NONE", PriceMatch::None),
            ("OPPONENT", PriceMatch::Opponent),
            ("OPPONENT_5", PriceMatch::Opponent5),
            ("OPPONENT_10", PriceMatch::Opponent10),
            ("OPPONENT_20", PriceMatch::Opponent20),
            ("QUEUE", PriceMatch::Queue),
            ("QUEUE_5", PriceMatch::Queue5),
            ("QUEUE_10", PriceMatch::Queue10),
            ("QUEUE_20", PriceMatch::Queue20),
        ] {
            let parsed: PriceMatchField =
                serde_json::from_str(&format!(r#"{{"value":"{value}"}}"#)).unwrap();
            assert_eq!(parsed.value, expected);
        }
    }
}
