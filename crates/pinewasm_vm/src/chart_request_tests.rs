use super::{chart_request_json, MarketData, RunOptions};
use serde_json::{json, Value};

fn feed() -> MarketData {
    MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        ..MarketData::default()
    }
}

#[test]
fn chart_primary_samples_round_trip_non_finite_values_and_signed_zero() {
    let samples = vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 0.0, 1.25];
    let data = MarketData {
        open: samples.clone(),
        high: samples.clone(),
        low: samples.clone(),
        close: samples.clone(),
        volume: samples.clone(),
        ..MarketData::default()
    };
    let request: Value =
        serde_json::from_str(&chart_request_json(&data, &RunOptions::default(), 6)).unwrap();
    let decoded: MarketData = serde_json::from_value(request["data"].clone()).unwrap();
    decoded.validate().unwrap();
    for lane in [
        decoded.open,
        decoded.high,
        decoded.low,
        decoded.close,
        decoded.volume,
    ] {
        for (actual, expected) in lane.into_iter().zip(&samples) {
            if expected.is_nan() {
                assert!(actual.is_nan());
            } else {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
    }
}

#[test]
fn chart_primary_samples_reject_other_tokens_and_missing_lanes() {
    let data = serde_json::to_value(feed()).unwrap();
    for name in ["open", "high", "low", "close", "volume"] {
        for invalid in [
            json!("NaN"),
            json!("inf"),
            json!("1"),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let mut changed = data.clone();
            changed[name][0] = invalid;
            assert!(
                serde_json::from_value::<MarketData>(changed).is_err(),
                "{name}"
            );
        }
        let mut missing = data.clone();
        missing.as_object_mut().unwrap().remove(name);
        assert!(
            serde_json::from_value::<MarketData>(missing).is_err(),
            "{name}"
        );
    }
}

#[test]
fn chart_primary_samples_do_not_relax_auxiliary_lane_decoding() {
    for invalid in [Value::Null, json!("Infinity"), json!("-Infinity")] {
        let mut data = serde_json::to_value(feed()).unwrap();
        data["bid"] = json!([invalid]);
        assert!(serde_json::from_value::<MarketData>(data).is_err());
    }
}
