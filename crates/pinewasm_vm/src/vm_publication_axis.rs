//! Borrowed chart clocks for supplied closes-only publication.
//!
//! Prices are carried only to derive the chart bar count. The nominal timeframe
//! remains lazy: supplied closes do not inspect the opening lane or timeframe.

use crate::{
    resample::{self, HtfBar, PublicationAxis, TimeAxis},
    vm_ops,
};

/// The nominal fallback used only after an absent closing lane and a successful
/// open-time read. Kept generic so the integer publication path does not depend
/// on the native string parser or price representation.
pub(crate) trait NominalSeconds {
    fn seconds(&self) -> i64;
}

impl NominalSeconds for String {
    fn seconds(&self) -> i64 {
        crate::timeframe_in_seconds(self)
    }
}

/// The exact borrowed lanes consumed by closes-only publication. No agreement
/// between lane lengths is imposed here; malformed reads still panic.
pub(crate) struct PublicationLanes<'a, P, N> {
    time: &'a [i64],
    time_close: &'a [i64],
    price_close: &'a [P],
    nominal: &'a N,
}

impl<P, N> TimeAxis for PublicationLanes<'_, P, N> {
    fn axis_len(&self) -> usize {
        self.price_close.len()
    }

    fn open_time(&self, index: usize) -> i64 {
        vm_ops::time_at(self.time, index)
    }
}

impl<P, N: NominalSeconds> PublicationAxis for PublicationLanes<'_, P, N> {
    fn close_time(&self, index: usize) -> i64 {
        match vm_ops::time_close_lane(self.time_close, index) {
            Some(close) => close,
            None => vm_ops::time_close_fallback(self.open_time(index), self.nominal.seconds()),
        }
    }
}

/// Publish using the actual borrowed-clock dictionary. This free generic root
/// keeps floating-point payloads and native timeframe parsing outside extraction
/// while keeping the native getter order and the assignment loop.
pub(crate) fn publish<P, N: NominalSeconds, F>(
    time: &[i64],
    time_close: &[i64],
    price_close: &[P],
    nominal: &N,
    bars: &mut [HtfBar<F>],
    requested_closes: &[i64],
) {
    let axis = PublicationLanes {
        time,
        time_close,
        price_close,
        nominal,
    };
    resample::publish_clock_groups(&axis, bars, requested_closes);
}

/// Publish an already borrowed axis through the existing generic publisher.
/// Forwarding borrows only; it adds no clock read, nominal parse or allocation.
pub(crate) fn publish_lanes<P, N: NominalSeconds, F>(
    lanes: &PublicationLanes<'_, P, N>,
    bars: &mut [HtfBar<F>],
    requested_closes: &[i64],
) {
    publish(
        lanes.time,
        lanes.time_close,
        lanes.price_close,
        lanes.nominal,
        bars,
        requested_closes,
    );
}

/// Borrow the four `MarketData` fields used by closes-only publication.
/// Lane lengths stay independent and no clock or nominal parser is evaluated.
pub(crate) fn market_data_lanes(data: &crate::MarketData) -> PublicationLanes<'_, f64, String> {
    PublicationLanes {
        time: &data.time,
        time_close: &data.time_close,
        price_close: &data.close,
        nominal: &data.timeframe,
    }
}

/// Four-field wrapper used by tests.
/// Production requested-close resampling calls `publish_lanes`.
#[cfg(test)]
pub(crate) fn publish_market_data<F>(
    data: &crate::MarketData,
    bars: &mut [HtfBar<F>],
    requested_closes: &[i64],
) {
    let lanes = market_data_lanes(data);
    publish(
        lanes.time,
        lanes.time_close,
        lanes.price_close,
        lanes.nominal,
        bars,
        requested_closes,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MarketData;
    use std::{any::Any, cell::Cell, panic::AssertUnwindSafe};

    struct NominalCheck {
        value: i64,
        panic: bool,
        calls: Cell<usize>,
    }

    impl NominalCheck {
        fn new(value: i64, panic: bool) -> Self {
            Self {
                value,
                panic,
                calls: Cell::new(0),
            }
        }
    }

    impl NominalSeconds for NominalCheck {
        fn seconds(&self) -> i64 {
            self.calls.set(self.calls.get() + 1);
            assert!(!self.panic, "nominal timeframe reached");
            self.value
        }
    }

    fn panic_text(panic: &(dyn Any + Send)) -> String {
        if let Some(value) = panic.downcast_ref::<String>() {
            value.clone()
        } else if let Some(value) = panic.downcast_ref::<&str>() {
            (*value).to_owned()
        } else {
            "non-string panic".to_owned()
        }
    }

    fn outcome<T>(body: impl FnOnce() -> T) -> Result<T, String> {
        std::panic::catch_unwind(AssertUnwindSafe(body)).map_err(|panic| panic_text(&*panic))
    }

    // Baseline copy of vm_ops::time_at
    // and MarketData::time_close_at, expanded separately from PublicationLanes.
    #[allow(clippy::cast_possible_wrap)]
    fn old_open(time: &[i64], index: usize) -> i64 {
        if time.is_empty() {
            crate::SYNTHETIC_EPOCH_MS + (index as i64) * crate::MS_PER_DAY
        } else {
            time[index]
        }
    }

    fn old_close<N: NominalSeconds>(
        time: &[i64],
        closes: &[i64],
        nominal: &N,
        index: usize,
    ) -> i64 {
        if closes.is_empty() {
            old_open(time, index).saturating_add(nominal.seconds().saturating_mul(1000))
        } else {
            closes[index]
        }
    }

    // Baseline copy of the publication expression and assignment order.
    fn old_publish<P, N: NominalSeconds, F>(
        time: &[i64],
        closes: &[i64],
        prices: &[P],
        nominal: &N,
        bars: &mut [HtfBar<F>],
        requested: &[i64],
    ) {
        let mut i = 0;
        while i < bars.len() {
            let bar = &mut bars[i];
            let end = bar.last_chart_bar;
            bar.publication_chart_bar = if old_close(time, closes, nominal, end) >= requested[end] {
                end
            } else if end + 1 < prices.len() && old_open(time, end + 1) >= requested[end] {
                end + 1
            } else {
                usize::MAX
            };
            i += 1;
        }
    }

    fn bar(last: usize, publication: usize) -> HtfBar {
        HtfBar {
            open: -0.0,
            high: f64::INFINITY,
            low: f64::NEG_INFINITY,
            close: f64::from_bits(0x7ff8_0000_0000_1234),
            volume: f64::from_bits(0x7ff0_0000_0000_4321),
            time: -7,
            last_chart_bar: last,
            publication_chart_bar: publication,
        }
    }

    fn bits(bars: &[HtfBar]) -> Vec<([u64; 5], i64, usize, usize)> {
        bars.iter()
            .map(|b| {
                (
                    [
                        b.open.to_bits(),
                        b.high.to_bits(),
                        b.low.to_bits(),
                        b.close.to_bits(),
                        b.volume.to_bits(),
                    ],
                    b.time,
                    b.last_chart_bar,
                    b.publication_chart_bar,
                )
            })
            .collect()
    }

    #[test]
    fn native_getters_match_all_lane_shapes() {
        for case in 0usize..4096 {
            let prices = vec![1.0; case % 7];
            let time: Vec<i64> = (0..(case / 7) % 7)
                .map(|i| i64::try_from(i).unwrap() * 100)
                .collect();
            let closes: Vec<i64> = (0..(case / 49) % 7)
                .map(|i| i64::try_from(i).unwrap() * 100 + 50)
                .collect();
            let index = (case / 343) % 8;
            let timeframe = ["1D", "1S", "0", "bad", "9223372036854775807M"][case % 5].to_owned();
            let data = MarketData {
                close: prices,
                time,
                time_close: closes,
                timeframe,
                ..MarketData::default()
            };
            let axis = PublicationLanes {
                time: &data.time,
                time_close: &data.time_close,
                price_close: &data.close,
                nominal: &data.timeframe,
            };
            assert_eq!(axis.axis_len(), data.bars(), "count case {case}");
            assert_eq!(
                outcome(|| axis.open_time(index)),
                outcome(|| data.time_at(index)),
                "open case {case}"
            );
            assert_eq!(
                outcome(|| axis.close_time(index)),
                outcome(|| data.time_close_at(index)),
                "close case {case}"
            );
        }
    }

    #[test]
    fn publication_matches_frozen_path_including_partial_failures() {
        for case in 0usize..2000 {
            let prices = vec![(); case % 5];
            let time: Vec<i64> = (0..(case / 5) % 5)
                .map(|i| i64::try_from(i).unwrap() * 10)
                .collect();
            let closes: Vec<i64> = (0..(case / 25) % 5)
                .map(|i| i64::try_from(i).unwrap() * 10 + 5)
                .collect();
            let requested: Vec<i64> = (0..(case / 125) % 5)
                .map(|i| i64::try_from(i).unwrap() * 20)
                .collect();
            let mut actual = vec![bar(case % 4, 71), bar((case / 4) % 4, 72), bar(0, 73)];
            let mut expected = actual.clone();
            let actual_nominal =
                NominalCheck::new(i64::try_from(case % 3).unwrap(), case % 11 == 0);
            let old_nominal = NominalCheck::new(actual_nominal.value, actual_nominal.panic);
            let actual_result = outcome(|| {
                publish(
                    &time,
                    &closes,
                    &prices,
                    &actual_nominal,
                    &mut actual,
                    &requested,
                );
            });
            let expected_result = outcome(|| {
                old_publish(
                    &time,
                    &closes,
                    &prices,
                    &old_nominal,
                    &mut expected,
                    &requested,
                );
            });
            assert_eq!(actual_result, expected_result, "result case {case}");
            assert_eq!(bits(&actual), bits(&expected), "state case {case}");
            assert_eq!(
                actual_nominal.calls.get(),
                old_nominal.calls.get(),
                "lazy calls case {case}"
            );
        }
    }

    #[test]
    fn lazy_nominal_order_and_invalid_reads() {
        let nominal = NominalCheck::new(1, true);
        let axis = PublicationLanes {
            time: &[][..],
            time_close: &[10, 20],
            price_close: &[(); 2],
            nominal: &nominal,
        };
        assert_eq!(axis.close_time(0), 10);
        assert!(outcome(|| axis.close_time(2))
            .unwrap_err()
            .contains("index out of bounds"));
        assert_eq!(nominal.calls.get(), 0);
        let missing = PublicationLanes {
            time: &[0],
            time_close: &[],
            price_close: &[(); 3],
            nominal: &nominal,
        };
        assert!(outcome(|| missing.close_time(1))
            .unwrap_err()
            .contains("index out of bounds"));
        assert_eq!(
            nominal.calls.get(),
            0,
            "open read must fail before nominal evaluation"
        );
        assert_eq!(
            outcome(|| missing.close_time(0)),
            Err("nominal timeframe reached".to_owned())
        );
        assert_eq!(nominal.calls.get(), 1);
    }

    #[test]
    fn delayed_sentinel_empty_and_price_length() {
        let nominal = NominalCheck::new(1, true);
        let mut bars = vec![bar(0, 71)];
        publish(&[0, 20], &[5], &[(); 2], &nominal, &mut bars, &[10]);
        assert_eq!(bars[0].publication_chart_bar, 1);
        publish(&[0, 20], &[5], &[(); 1], &nominal, &mut bars, &[10]);
        assert_eq!(bars[0].publication_chart_bar, usize::MAX);
        publish(
            &[],
            &[],
            &[] as &[()],
            &nominal,
            &mut [] as &mut [HtfBar],
            &[],
        );
        assert_eq!(nominal.calls.get(), 0);
        let saturated = NominalCheck::new(i64::MAX, false);
        let axis = PublicationLanes {
            time: &[i64::MAX - 1],
            time_close: &[],
            price_close: &[()],
            nominal: &saturated,
        };
        assert_eq!(axis.close_time(0), i64::MAX);
    }

    #[test]
    fn malformed_middle_preserves_prefix_current_suffix_and_payload_bits() {
        let nominal = NominalCheck::new(0, false);
        for (time, closes, requested, ends) in [
            (vec![0, 20, 30], vec![10], vec![5, 15], [0, 1, 0]),
            (vec![0, 20, 30], vec![10, 30], vec![5], [0, 1, 0]),
            (vec![0], vec![10, 0], vec![5, 15], [0, 1, 0]),
        ] {
            let mut actual: Vec<_> = ends
                .into_iter()
                .enumerate()
                .map(|(i, end)| bar(end, 71 + i))
                .collect();
            let before = bits(&actual);
            let mut expected = actual.clone();
            let result =
                outcome(|| publish(&time, &closes, &[(); 3], &nominal, &mut actual, &requested));
            assert!(result.is_err());
            assert_eq!(
                result,
                outcome(|| old_publish(
                    &time,
                    &closes,
                    &[(); 3],
                    &nominal,
                    &mut expected,
                    &requested
                ))
            );
            assert_eq!(bits(&actual), bits(&expected));
            assert_eq!(actual[0].publication_chart_bar, 0);
            assert_eq!(&bits(&actual)[1..], &before[1..]);
            for (after, before) in bits(&actual).iter().zip(before.iter()) {
                assert_eq!(after.0, before.0);
                assert_eq!((after.1, after.2), (before.1, before.2));
            }
        }
    }

    #[test]
    fn noncopy_payloads_are_not_cloned_or_modified() {
        struct Payload(Vec<u8>);
        let price = [Payload(vec![1, 2, 3])];
        let nominal = NominalCheck::new(0, true);
        let mut bars = [HtfBar {
            open: Payload(vec![4]),
            high: Payload(vec![5]),
            low: Payload(vec![6]),
            close: Payload(vec![7]),
            volume: Payload(vec![8]),
            time: 9,
            last_chart_bar: 0,
            publication_chart_bar: 71,
        }];
        let pointers = [
            bars[0].open.0.as_ptr(),
            bars[0].high.0.as_ptr(),
            bars[0].low.0.as_ptr(),
            bars[0].close.0.as_ptr(),
            bars[0].volume.0.as_ptr(),
        ];
        publish(&[0], &[10], &price, &nominal, &mut bars, &[5]);
        assert_eq!(bars[0].publication_chart_bar, 0);
        assert_eq!(
            pointers,
            [
                bars[0].open.0.as_ptr(),
                bars[0].high.0.as_ptr(),
                bars[0].low.0.as_ptr(),
                bars[0].close.0.as_ptr(),
                bars[0].volume.0.as_ptr()
            ]
        );
        assert_eq!(
            (
                bars[0].open.0.as_slice(),
                bars[0].high.0.as_slice(),
                bars[0].low.0.as_slice(),
                bars[0].close.0.as_slice(),
                bars[0].volume.0.as_slice()
            ),
            (&[4][..], &[5][..], &[6][..], &[7][..], &[8][..])
        );
        assert_eq!(price[0].0, vec![1, 2, 3]);
    }

    #[test]
    fn authoritative_native_branch_uses_delayed_publication() {
        let mut data = MarketData {
            open: vec![1.0; 4],
            high: vec![2.0; 4],
            low: vec![0.0; 4],
            close: vec![1.5; 4],
            volume: vec![10.0; 4],
            time: vec![0, 10, 20, 30],
            time_close: vec![5, 15, 25, 35],
            ..MarketData::default()
        };
        data.request_time_close
            .insert("2D".to_owned(), vec![20, 20, 40, 40]);
        let result =
            resample::resample_aligned_security(&data, 172_800, resample::CalendarAlignment::Fixed);
        assert_eq!(result.bucket_of, vec![0, 0, 1, 1]);
        assert_eq!(
            result
                .bars
                .iter()
                .map(|bar| (bar.last_chart_bar, bar.publication_chart_bar))
                .collect::<Vec<_>>(),
            vec![(1, 2), (3, usize::MAX)]
        );
    }

    // Baseline copy of the four-field argument projection used by
    // resample_aligned_security. The group slice and requested closes are test parameters.
    fn reference_market_publish<F>(data: &MarketData, bars: &mut [HtfBar<F>], requested: &[i64]) {
        publish(
            &data.time,
            &data.time_close,
            &data.close,
            &data.timeframe,
            bars,
            requested,
        );
    }

    #[test]
    fn market_data_fields_are_the_original_borrowed_buffers() {
        let mut data = MarketData {
            open: vec![1.0; 2],
            close: vec![-0.0, f64::from_bits(0x7ff8_0000_0000_4321), 0.0],
            time: vec![17, 31, 49, 71],
            time_close: vec![23, 41],
            timeframe: " 8S ".to_owned(),
            ..MarketData::default()
        };
        data.syminfo.timezone = "2S".to_owned();
        let lanes = market_data_lanes(&data);
        assert_eq!(lanes.time.as_ptr(), data.time.as_ptr());
        assert_eq!(lanes.time_close.as_ptr(), data.time_close.as_ptr());
        assert_eq!(lanes.price_close.as_ptr(), data.close.as_ptr());
        assert!(std::ptr::eq(lanes.nominal, &raw const data.timeframe));
        assert_eq!(lanes.time, data.time);
        assert_eq!(lanes.time_close, data.time_close);
        assert_eq!(lanes.price_close.len(), 3);
        assert_eq!(lanes.axis_len(), data.close.len());
        assert_eq!(lanes.nominal, " 8S ");
        assert_eq!(
            lanes
                .price_close
                .iter()
                .map(|x| x.to_bits())
                .collect::<Vec<_>>(),
            data.close.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn market_data_all_empty_fields_are_preserved_without_reads() {
        let data = MarketData {
            close: Vec::new(),
            time: Vec::new(),
            time_close: Vec::new(),
            timeframe: String::new(),
            ..MarketData::default()
        };
        let lanes = market_data_lanes(&data);
        assert_eq!(lanes.axis_len(), 0);
        assert!(lanes.time.is_empty() && lanes.time_close.is_empty());
        assert!(std::ptr::eq(lanes.nominal, &raw const data.timeframe));
        publish_market_data::<f64>(&data, &mut [], &[]);
    }

    #[test]
    fn market_data_wrapper_matches_frozen_call_for_4096_shapes() {
        let frames = [
            "1D",
            "?",
            "-3S",
            "9223372036854775807M",
            " \u{2003}8S\u{3000} ",
        ];
        for case in 0usize..4096 {
            let mut data = MarketData {
                open: vec![9.0; (case / 2401) % 7],
                close: vec![-0.0; case % 7],
                time: (0..(case / 7) % 7)
                    .map(|i| i64::try_from(i).unwrap() * 100)
                    .collect(),
                time_close: (0..(case / 49) % 7)
                    .map(|i| i64::try_from(i).unwrap() * 100 + 50)
                    .collect(),
                timeframe: frames[case % frames.len()].to_owned(),
                ..MarketData::default()
            };
            data.syminfo.timezone = "2S".to_owned();
            let requested: Vec<i64> = (0..(case / 343) % 7)
                .map(|i| i64::try_from(i).unwrap() * 100 + 75)
                .collect();
            let initial = vec![
                bar(case % 7, 101),
                bar((case / 7) % 7, 103),
                bar((case / 49) % 7, 107),
            ];
            let mut actual = initial.clone();
            let mut expected = initial;
            let result = outcome(|| publish_market_data(&data, &mut actual, &requested));
            let reference = outcome(|| reference_market_publish(&data, &mut expected, &requested));
            assert_eq!(result, reference, "case {case}: panic/result");
            assert_eq!(
                bits(&actual),
                bits(&expected),
                "case {case}: full prefix/frame"
            );
        }
    }

    #[test]
    fn market_data_payload_bits_delay_sentinel_and_nominal_fallback() {
        let mut data = MarketData {
            open: vec![99.0],
            close: vec![-0.0, 0.0, f64::from_bits(0x7ff8_0000_0000_1234)],
            time: vec![0, 20, 40],
            time_close: vec![5, 25, 45],
            timeframe: "?".to_owned(),
            ..MarketData::default()
        };
        let mut bars = vec![bar(0, 71), bar(1, 73), bar(2, 79)];
        bars[1].open = 0.0;
        bars[1].high = -0.0;
        bars[2].close = f64::from_bits(1);
        let before = bits(&bars);
        publish_market_data(&data, &mut bars, &[10, 30, 50]);
        assert_eq!(
            bars.iter()
                .map(|b| b.publication_chart_bar)
                .collect::<Vec<_>>(),
            vec![1, 2, usize::MAX]
        );
        for (after, prior) in bits(&bars).iter().zip(before.iter()) {
            assert_eq!((after.0, after.1, after.2), (prior.0, prior.1, prior.2));
        }
        data.time_close.clear();
        data.timeframe = " 2S ".to_owned();
        data.syminfo.timezone = "1S".to_owned();
        let mut fallback = [bar(0, 83)];
        publish_market_data(&data, &mut fallback, &[1500]);
        assert_eq!(fallback[0].publication_chart_bar, 0);
        assert_eq!(data.close[0].to_bits(), (-0.0f64).to_bits());
        assert_eq!(data.close[1].to_bits(), 0.0f64.to_bits());
        assert_eq!(data.close[2].to_bits(), 0x7ff8_0000_0000_1234);
    }

    #[test]
    fn market_data_middle_failures_keep_the_same_partial_prefix() {
        for which in 0..3 {
            let data = MarketData {
                close: vec![1.0; 3],
                time: if which == 1 { vec![0] } else { vec![0, 20, 40] },
                time_close: if which == 0 {
                    vec![10]
                } else if which == 1 {
                    Vec::new()
                } else {
                    vec![10, 30, 50]
                },
                timeframe: "1S".to_owned(),
                ..MarketData::default()
            };
            let requested = if which == 2 { vec![5] } else { vec![5, 25, 45] };
            let mut actual = vec![bar(0, 101), bar(1, 103), bar(2, 107)];
            let initial = bits(&actual);
            let mut expected = actual.clone();
            let result = outcome(|| publish_market_data(&data, &mut actual, &requested));
            let reference = outcome(|| reference_market_publish(&data, &mut expected, &requested));
            assert!(result.is_err(), "which {which}");
            assert_eq!(result, reference, "which {which}");
            assert_eq!(bits(&actual), bits(&expected), "which {which}");
            assert_eq!(actual[0].publication_chart_bar, 0);
            assert_eq!(&bits(&actual)[1..], &initial[1..]);
        }
    }

    #[test]
    fn market_data_authoritative_resample_call_keeps_delayed_publication() {
        let mut data = MarketData {
            open: vec![1.0; 4],
            high: vec![2.0; 4],
            low: vec![0.0; 4],
            close: vec![1.5; 4],
            volume: vec![10.0; 4],
            time: vec![0, 10, 20, 30],
            time_close: vec![5, 15, 25, 35],
            timeframe: "1S".to_owned(),
            ..MarketData::default()
        };
        data.request_time_close
            .insert("2D".to_owned(), vec![20, 20, 40, 40]);
        let result =
            resample::resample_aligned_security(&data, 172_800, resample::CalendarAlignment::Fixed);
        assert_eq!(result.bucket_of, vec![0, 0, 1, 1]);
        assert_eq!(
            result
                .bars
                .iter()
                .map(|b| (b.last_chart_bar, b.publication_chart_bar))
                .collect::<Vec<_>>(),
            vec![(1, 2), (3, usize::MAX)]
        );
    }
}

#[cfg(test)]
mod requested_spine_tests {
    use super::*;
    use crate::resample::{requested_close_buckets, requested_close_resample, BarSource};
    use std::{
        cell::RefCell,
        panic::{catch_unwind, AssertUnwindSafe},
    };

    type Event = (&'static str, usize);
    struct Source<'a> {
        events: &'a RefCell<Vec<Event>>,
        stop: Option<Event>,
    }
    impl Source<'_> {
        fn read(&self, kind: &'static str, index: usize) {
            self.events.borrow_mut().push((kind, index));
            assert_ne!(self.stop, Some((kind, index)), "source callback stopped");
        }
    }
    impl BarSource<HtfBar<u64>> for Source<'_> {
        fn open_bucket(&self, i: usize, last: usize) -> HtfBar<u64> {
            self.read("open", i);
            let value = u64::try_from(i).unwrap() + 1;
            HtfBar {
                open: value,
                high: value,
                low: value,
                close: value,
                volume: value,
                time: i64::try_from(i).unwrap(),
                last_chart_bar: last,
                publication_chart_bar: last,
            }
        }
        fn extend_bucket(&self, bar: &mut HtfBar<u64>, i: usize) {
            self.read("extend", i);
            let value = u64::try_from(i).unwrap() + 1;
            bar.high = bar.high * 10 + value;
            bar.low = bar.low * 10 + value;
            bar.close = value;
            bar.volume = bar.volume * 10 + value;
        }
    }
    struct Nominal<'a> {
        events: &'a RefCell<Vec<Event>>,
        stop: bool,
    }
    impl NominalSeconds for Nominal<'_> {
        fn seconds(&self) -> i64 {
            self.events.borrow_mut().push(("nominal", 0));
            assert!(!self.stop, "nominal callback stopped");
            0
        }
    }
    fn caught<T>(run: impl FnOnce() -> T) -> Result<T, String> {
        catch_unwind(AssertUnwindSafe(run)).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic".to_owned())
        })
    }
    fn previous<P, N: NominalSeconds, F, S: BarSource<HtfBar<F>>>(
        data: &crate::MarketData,
        source: &S,
        lanes: &PublicationLanes<'_, P, N>,
        requested: &[i64],
    ) -> (Vec<HtfBar<F>>, Vec<usize>) {
        let groups = requested_close_buckets(data, requested);
        let mut bars = resample::fold_buckets(source, &groups.bucket_of, &groups.last_chart_bar);
        publish(
            lanes.time,
            lanes.time_close,
            lanes.price_close,
            lanes.nominal,
            &mut bars,
            requested,
        );
        (bars, groups.bucket_of)
    }

    #[test]
    fn requested_spine_all_fold_callbacks_precede_nominal_and_keep_left_order() {
        let data = crate::MarketData {
            close: vec![0.0; 3],
            ..crate::MarketData::default()
        };
        let actual_events = RefCell::new(Vec::new());
        let previous_events = RefCell::new(Vec::new());
        let nominal = Nominal {
            events: &actual_events,
            stop: false,
        };
        let old_nominal = Nominal {
            events: &previous_events,
            stop: false,
        };
        let lanes = PublicationLanes {
            time: &[0, 10, 20],
            time_close: &[],
            price_close: &[(); 3],
            nominal: &nominal,
        };
        let old_lanes = PublicationLanes {
            time: lanes.time,
            time_close: lanes.time_close,
            price_close: lanes.price_close,
            nominal: &old_nominal,
        };
        let actual = requested_close_resample(
            &data,
            &Source {
                events: &actual_events,
                stop: None,
            },
            &lanes,
            &[15, 15, 25],
        );
        let expected = previous(
            &data,
            &Source {
                events: &previous_events,
                stop: None,
            },
            &old_lanes,
            &[15, 15, 25],
        );
        assert_eq!(actual, expected);
        assert_eq!(actual.1, [0, 0, 1]);
        assert_eq!(actual.0[0].volume, 12);
        assert_eq!(actual.0[0].publication_chart_bar, 2);
        assert_eq!(actual.0[1].publication_chart_bar, usize::MAX);
        assert_eq!(
            *actual_events.borrow(),
            [
                ("open", 0),
                ("extend", 1),
                ("open", 2),
                ("nominal", 0),
                ("nominal", 0)
            ]
        );
        assert_eq!(actual_events, previous_events);
    }

    #[test]
    fn requested_spine_short_keys_and_each_source_panic_skip_later_phases() {
        let data = crate::MarketData {
            close: vec![0.0; 3],
            ..crate::MarketData::default()
        };
        for (requested, stop, expected_events) in [
            (&[15][..], None, vec![]),
            (&[15, 15, 25][..], Some(("open", 0)), vec![("open", 0)]),
            (
                &[15, 15, 25][..],
                Some(("extend", 1)),
                vec![("open", 0), ("extend", 1)],
            ),
            (
                &[15, 15, 25][..],
                Some(("open", 2)),
                vec![("open", 0), ("extend", 1), ("open", 2)],
            ),
        ] {
            let events = RefCell::new(Vec::new());
            let old_events = RefCell::new(Vec::new());
            let nominal = Nominal {
                events: &events,
                stop: true,
            };
            let old_nominal = Nominal {
                events: &old_events,
                stop: true,
            };
            let lanes = PublicationLanes {
                time: &[],
                time_close: &[],
                price_close: &[(); 3],
                nominal: &nominal,
            };
            let old_lanes = PublicationLanes {
                time: &[],
                time_close: &[],
                price_close: &[(); 3],
                nominal: &old_nominal,
            };
            let actual = caught(|| {
                requested_close_resample(
                    &data,
                    &Source {
                        events: &events,
                        stop,
                    },
                    &lanes,
                    requested,
                )
            });
            let expected = caught(|| {
                previous(
                    &data,
                    &Source {
                        events: &old_events,
                        stop,
                    },
                    &old_lanes,
                    requested,
                )
            });
            assert!(actual.is_err());
            assert_eq!(actual, expected);
            assert_eq!(*events.borrow(), expected_events);
            assert_eq!(events, old_events);
        }
    }

    #[test]
    fn requested_spine_explicit_closes_are_lazy_and_empty_reads_nothing() {
        let data = crate::MarketData {
            close: vec![0.0; 3],
            ..crate::MarketData::default()
        };
        let events = RefCell::new(Vec::new());
        let nominal = Nominal {
            events: &events,
            stop: true,
        };
        // The price-clock count is deliberately different: grouping still uses data.close.
        let lanes = PublicationLanes {
            time: &[],
            time_close: &[15, 15, 25],
            price_close: &[()],
            nominal: &nominal,
        };
        let (bars, map) = requested_close_resample(
            &data,
            &Source {
                events: &events,
                stop: None,
            },
            &lanes,
            &[15, 15, 25],
        );
        assert_eq!(map, [0, 0, 1]);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].last_chart_bar, 1);
        assert_eq!(bars[0].publication_chart_bar, 1);
        assert_eq!(bars[1].last_chart_bar, 2);
        assert_eq!(bars[1].publication_chart_bar, 2);
        assert_eq!(*events.borrow(), [("open", 0), ("extend", 1), ("open", 2)]);
        events.borrow_mut().clear();
        let empty = crate::MarketData::default();
        let result = requested_close_resample(
            &empty,
            &Source {
                events: &events,
                stop: Some(("open", 0)),
            },
            &lanes,
            &[],
        );
        assert_eq!(result, (vec![], vec![]));
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn requested_spine_clock_failure_precedes_nominal_and_stops_next_group() {
        let data = crate::MarketData {
            close: vec![0.0; 3],
            ..crate::MarketData::default()
        };
        for (time, closes, expected_nominal) in [
            (&[0, 10, 20][..], &[15][..], false),
            (&[0][..], &[][..], false),
            (&[0, 10, 20][..], &[][..], true),
        ] {
            let events = RefCell::new(Vec::new());
            let old_events = RefCell::new(Vec::new());
            let nominal = Nominal {
                events: &events,
                stop: true,
            };
            let old_nominal = Nominal {
                events: &old_events,
                stop: true,
            };
            let lanes = PublicationLanes {
                time,
                time_close: closes,
                price_close: &[(); 3],
                nominal: &nominal,
            };
            let old_lanes = PublicationLanes {
                time,
                time_close: closes,
                price_close: &[(); 3],
                nominal: &old_nominal,
            };
            let actual = caught(|| {
                requested_close_resample(
                    &data,
                    &Source {
                        events: &events,
                        stop: None,
                    },
                    &lanes,
                    &[15, 15, 25],
                )
            });
            let expected = caught(|| {
                previous(
                    &data,
                    &Source {
                        events: &old_events,
                        stop: None,
                    },
                    &old_lanes,
                    &[15, 15, 25],
                )
            });
            assert!(actual.is_err());
            assert_eq!(actual, expected);
            assert_eq!(events, old_events);
            assert_eq!(events.borrow().contains(&("nominal", 0)), expected_nominal);
            assert_eq!(
                &events.borrow()[..3],
                [("open", 0), ("extend", 1), ("open", 2)]
            );
        }
    }
}
