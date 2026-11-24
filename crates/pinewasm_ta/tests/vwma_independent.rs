use pinewasm_ta::{Decl, Engine, Lanes, Layout, State, TaFunc};

struct Feed {
    source: [f64; 5],
    volume: [f64; 5],
}

impl Feed {
    fn discriminator() -> Self {
        Self {
            source: [2.0, f64::NAN, f64::NAN, 8.0, 10.0],
            volume: [1.0, 3.0, 5.0, 2.0, 2.0],
        }
    }
}

impl Lanes for Feed {
    fn open(&self, bar: u32) -> f64 {
        self.close(bar)
    }
    fn high(&self, bar: u32) -> f64 {
        self.close(bar)
    }
    fn low(&self, bar: u32) -> f64 {
        self.close(bar)
    }
    fn close(&self, bar: u32) -> f64 {
        self.source[bar as usize]
    }
    fn volume(&self, bar: u32) -> f64 {
        self.volume[bar as usize]
    }
    fn time_ms(&self, bar: u32) -> i64 {
        i64::from(bar) * 60_000
    }
}

fn optional(value: f64) -> Option<f64> {
    if value.is_nan() {
        None
    } else {
        Some(value)
    }
}

#[test]
fn vwma_keeps_weighted_and_unmasked_volume_inputs_separate() {
    let decls = [Decl::new(TaFunc::Vwma, &[2]).unwrap()];
    let layout = Layout::plan(&decls);
    assert_eq!(layout.cells, 4, "the 2N footprint is sufficient");
    let mut states = [State::Invalid];
    let mut cells = [f64::NAN; 4];
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut []);
    let feed = Feed::discriminator();
    let actual: Vec<_> = (0..5)
        .map(|bar| {
            optional(
                engine
                    .advance(
                        0,
                        [feed.source[bar as usize], f64::NAN, f64::NAN],
                        &feed,
                        bar,
                    )
                    .unwrap()
                    .value,
            )
        })
        .collect();
    // At bar3 numerator SMA=(2*1 + 8*2)/2, denominator SMA=(5+2)/2.
    // Masking the source's NA bars in the denominator incorrectly yields6.
    assert_eq!(actual, [None, None, None, Some(18.0 / 7.0), Some(9.0)]);
}

#[test]
fn vwma_independent_windows_restore_reset_and_fail_closed_on_short_storage() {
    let decls = [
        Decl::new(TaFunc::Vwma, &[2]).unwrap(),
        Decl::new(TaFunc::Vwma, &[3]).unwrap(),
    ];
    let mut states = [State::Invalid; 2];
    let mut cells = [f64::NAN; 10];
    let feed = Feed::discriminator();
    Engine::init(&decls, &mut states, &mut cells, &mut []);
    for bar in 0..3 {
        let mut engine = Engine::bind(&decls, &mut states, &mut cells, &mut []);
        for site in 0..2 {
            engine
                .advance(
                    site,
                    [feed.source[bar as usize], f64::NAN, f64::NAN],
                    &feed,
                    bar,
                )
                .unwrap();
        }
    }
    let before_states = states;
    let before_cells = cells;
    let first = Engine::bind(&decls, &mut states, &mut cells, &mut [])
        .advance(0, [8.0, f64::NAN, f64::NAN], &feed, 3)
        .unwrap()
        .value;
    assert_eq!(first, 18.0 / 7.0);
    assert!(
        cells[4..]
            .iter()
            .zip(&before_cells[4..])
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "the next site's cells are untouched"
    );
    states = before_states;
    cells = before_cells;
    let replay = Engine::bind(&decls, &mut states, &mut cells, &mut [])
        .advance(0, [8.0, f64::NAN, f64::NAN], &feed, 3)
        .unwrap()
        .value;
    assert_eq!(first.to_bits(), replay.to_bits());
    Engine::bind(&decls, &mut states, &mut cells, &mut []).reset();
    assert!(cells.iter().all(|v| v.is_nan()));
    assert!(states
        .iter()
        .all(|s| matches!(s, State::VwmaIndependent { .. })));
    assert!(Engine::bind(&decls, &mut states, &mut cells, &mut [])
        .advance(0, [2.0, f64::NAN, f64::NAN], &feed, 0)
        .unwrap()
        .value
        .is_nan());

    let decls = [Decl::new(TaFunc::Vwma, &[2]).unwrap()];
    let mut states = [State::Invalid];
    let mut cells = [f64::NAN; 3];
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut []);
    for bar in 0..5 {
        assert!(engine
            .advance(
                0,
                [feed.source[bar as usize], f64::NAN, f64::NAN],
                &feed,
                bar
            )
            .unwrap()
            .value
            .is_nan());
    }
}

#[test]
fn vwma_sma_ratio_handles_missing_and_zero_volume_without_pair_poisoning() {
    let decls = [Decl::new(TaFunc::Vwma, &[2]).unwrap()];
    let mut states = [State::Invalid];
    let mut cells = [f64::NAN; 4];
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut []);
    let feed = Feed {
        source: [2.0, 4.0, 6.0, 8.0, 10.0],
        volume: [1.0, 3.0, f64::NAN, 0.0, 0.0],
    };
    let actual: Vec<_> = (0..5)
        .map(|bar| {
            optional(
                engine
                    .advance(
                        0,
                        [feed.source[bar as usize], f64::NAN, f64::NAN],
                        &feed,
                        bar,
                    )
                    .unwrap()
                    .value,
            )
        })
        .collect();
    assert_eq!(actual, [None, Some(3.5), Some(3.5), Some(4.0), None]);
}
