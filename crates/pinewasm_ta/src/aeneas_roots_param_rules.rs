//! Free-function root for the engine parameter-rule table.
//! The extraction tool cannot select an inherent method directly.

use crate::{TaFunc, TaParamRule};

pub fn engine_param_rule_at(function: TaFunc, index: u32) -> Option<TaParamRule> {
    function.engine_param_rule(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous_rules(function: TaFunc) -> &'static [TaParamRule] {
        const LEN: TaParamRule = TaParamRule::Length { min: 1 };
        match function {
            TaFunc::Change | TaFunc::Sma => &[],
            TaFunc::Fixnan
            | TaFunc::FixnanInt
            | TaFunc::Swma
            | TaFunc::Cum
            | TaFunc::Max
            | TaFunc::Min
            | TaFunc::Crossover
            | TaFunc::Crossunder
            | TaFunc::Cross
            | TaFunc::BarsSince
            | TaFunc::Obv
            | TaFunc::AccDist
            | TaFunc::Nvi
            | TaFunc::Pvi
            | TaFunc::Pvt
            | TaFunc::Wad
            | TaFunc::Wvad
            | TaFunc::Iii
            | TaFunc::Vwap
            | TaFunc::VwapSource
            | TaFunc::VwapAnchored
            | TaFunc::VwapBands => &[],
            TaFunc::MathSum
            | TaFunc::Ema
            | TaFunc::Rma
            | TaFunc::Wma
            | TaFunc::Vwma
            | TaFunc::Rsi
            | TaFunc::Atr
            | TaFunc::Dev
            | TaFunc::Highest
            | TaFunc::Lowest
            | TaFunc::HighestBars
            | TaFunc::LowestBars
            | TaFunc::Mom
            | TaFunc::Roc
            | TaFunc::Rising
            | TaFunc::Falling
            | TaFunc::Cci
            | TaFunc::Cmo
            | TaFunc::Cog
            | TaFunc::Correlation
            | TaFunc::Median
            | TaFunc::MedianInt
            | TaFunc::Mode
            | TaFunc::ModeInt
            | TaFunc::PercentRank
            | TaFunc::Range
            | TaFunc::RangeInt
            | TaFunc::Mfi
            | TaFunc::Wpr
            | TaFunc::Stoch
            | TaFunc::Rci => &[LEN],
            // hma needs length/2 >= 1 for its inner wma.
            TaFunc::Hma => &[TaParamRule::Length { min: 2 }],
            TaFunc::Alma => &[
                LEN,
                TaParamRule::FloatBits { positive: false },
                TaParamRule::FloatBits { positive: true },
                TaParamRule::Bool01,
            ],
            TaFunc::Tr => &[TaParamRule::Bool01],
            TaFunc::Stdev | TaFunc::Variance => &[LEN, TaParamRule::Bool01],
            TaFunc::ValueWhen
            | TaFunc::ValueWhenInt
            | TaFunc::ValueWhenBool
            | TaFunc::ValueWhenColor => &[TaParamRule::IntRange {
                min: 0,
                max: 100_000,
            }],
            TaFunc::Linreg => &[
                LEN,
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
            ],
            TaFunc::PercentileLinearInterpolation | TaFunc::PercentileNearestRank => {
                &[LEN, TaParamRule::Percent]
            }
            // Three acceleration parameters (strictly positive).
            TaFunc::Sar => &[
                TaParamRule::FloatBits { positive: true },
                TaParamRule::FloatBits { positive: true },
                TaParamRule::FloatBits { positive: true },
            ],
            // Two window lengths.
            TaFunc::Tsi => &[LEN, LEN],
            TaFunc::Bbw => &[LEN, TaParamRule::FloatBits { positive: false }],
            TaFunc::Kcw => &[
                LEN,
                TaParamRule::FloatBits { positive: false },
                TaParamRule::Bool01,
            ],
            // Two window sizes, one on each side of the pivot.
            TaFunc::PivotHigh | TaFunc::PivotLow => &[
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
            ],
            // Fast and slow moving-average lengths, then a signal length.
            TaFunc::Macd => &[LEN, LEN, LEN],
            // A window length, then a width multiplier (any finite float).
            TaFunc::Bb => &[LEN, TaParamRule::FloatBits { positive: false }],
            // A window length, a multiplier and a boolean flag.
            TaFunc::Kc => &[
                LEN,
                TaParamRule::FloatBits { positive: false },
                TaParamRule::Bool01,
            ],
            // Two window lengths: directional movement, then ADX smoothing.
            TaFunc::Dmi => &[LEN, LEN],
            // A multiplier (any finite float), then a window length.
            TaFunc::Supertrend => &[TaParamRule::FloatBits { positive: false }, LEN],
            // The baked timeframe length in seconds (>= 1); the compiler
            // computes it from the const `timeframe` string.
            TaFunc::TimeframeChange => &[TaParamRule::IntRange {
                min: 1,
                max: i64::MAX,
            }],
        }
    }

    #[test]
    fn rule_representation_preserves_every_tag_and_payload() {
        for function in TaFunc::ALL {
            let expected = previous_rules(function);
            assert_eq!(function.engine_param_rules(), expected, "{function:?}");
            for index in 0..=crate::MAX_PARAMS as u32 {
                assert_eq!(
                    engine_param_rule_at(function, index),
                    expected.get(index as usize).copied(),
                    "{function:?} rule {index}"
                );
            }
            assert_eq!(engine_param_rule_at(function, u32::MAX), None);
        }
    }
}
