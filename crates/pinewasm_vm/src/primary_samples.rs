//! Decode OHLCV price and volume arrays from JSON, accepting null, Infinity and -Infinity.
//! Other numeric arrays use their own stricter deserializers.

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

#[derive(Deserialize)]
#[serde(untagged)]
enum Sample {
    Number(f64),
    Missing,
    Token(String),
}

struct Samples;

impl<'de> Visitor<'de> for Samples {
    type Value = Vec<f64>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a price or volume array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut samples = Vec::new();
        while let Some(sample) = sequence.next_element::<Sample>()? {
            samples.push(match sample {
                Sample::Number(number) => number,
                Sample::Missing => f64::NAN,
                Sample::Token(token) => match token.as_str() {
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    _ => {
                        return Err(de::Error::custom(
                            "a price or volume sample must be a number, null, Infinity, or -Infinity",
                        ));
                    }
                },
            });
        }
        Ok(samples)
    }
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<f64>, D::Error> {
    // Non-human-readable formats keep the plain f64-array contract.
    if !deserializer.is_human_readable() {
        return Vec::<f64>::deserialize(deserializer);
    }
    deserializer.deserialize_seq(Samples)
}
