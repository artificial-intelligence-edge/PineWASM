//! A pinned baseline for every value `pinewasm_num` produces.
//!
//! This crate is the single numeric core: the reference interpreter calls
//! it directly, and the same crate built for `wasm32-unknown-unknown` is
//! spliced into every emitted artifact. Changing it changes every result the
//! project has ever produced, which is exactly what makes the eventual move to
//! a correctly-rounded implementation risky to land blind.
//!
//! So this test is not an accuracy claim. `libm` is not correctly rounded, and
//! the point is that these bits are *known* and pinned, not that they are
//! accurate. A change to the numeric core turns into an explicit diff of which
//! values moved, rather than a silent shift in every cached indicator result.
//!
//! It is platform-independent by construction. `pinewasm_num` deliberately
//! does not call `std`'s float methods, which bind to the host libm and differ
//! between macOS, glibc, and wasm; it pins one `libm` for every target. That
//! pinning is what lets these bits be asserted exactly rather than within a
//! tolerance, and this test fails if the pin is ever quietly loosened.
//!
//! Regenerate deliberately, never to make a red test green: a diff here is the
//! blast radius of a numerics change and belongs in that change's review.

/// `(name, id, [(a, b, expected_bits)])`, compared with `to_bits()` so that a
/// sign-of-zero or na-payload change cannot hide behind `==`.
type Case = (&'static str, u32, &'static [(f64, f64, u64)]);

#[rustfmt::skip]
const GOLDEN: &[Case] = &[
    ("exp", 0, &[(0e0f64, 0e0f64, 0x3ff0000000000000u64), (1e0f64, 0e0f64, 0x4005bf0a8b14576au64), (-1e0f64, 0e0f64, 0x3fd78b56362cef38u64), (5e-1f64, 0e0f64, 0x3ffa61298e1e069cu64), (1e1f64, 0e0f64, 0x40d5829dcf950560u64), (-1e1f64, 0e0f64, 0x3f07cd79b5647c9au64), (7e2f64, 0e0f64, 0x7f0d945df4f8ec8eu64), (1e-1f64, 0e0f64, 0x3ff1aec7b35a00d4u64), ]),
    ("log", 1, &[(1e0f64, 0e0f64, 0x0000000000000000u64), (2e0f64, 0e0f64, 0x3fe62e42fefa39efu64), (1e1f64, 0e0f64, 0x40026bb1bbb55516u64), (5e-1f64, 0e0f64, 0xbfe62e42fefa39efu64), (1e-8f64, 0e0f64, 0xc0326bb1bbb55516u64), (1e8f64, 0e0f64, 0x40326bb1bbb55516u64), (std::f64::consts::SQRT_2, 0e0f64, 0x3fd62e42fefa39f0u64), (std::f64::consts::E, 0e0f64, 0x3ff0000000000000u64), ]),
    ("log10", 2, &[(1e0f64, 0e0f64, 0x0000000000000000u64), (2e0f64, 0e0f64, 0x3fd34413509f79ffu64), (1e1f64, 0e0f64, 0x3ff0000000000000u64), (5e-1f64, 0e0f64, 0xbfd34413509f79ffu64), (1e-8f64, 0e0f64, 0xc020000000000000u64), (1e8f64, 0e0f64, 0x4020000000000000u64), (std::f64::consts::SQRT_2, 0e0f64, 0x3fc34413509f7a00u64), (std::f64::consts::E, 0e0f64, 0x3fdbcb7b1526e50eu64), ]),
    ("sin", 3, &[(0e0f64, 0e0f64, 0x0000000000000000u64), (5e-1f64, 0e0f64, 0x3fdeaee8744b05f0u64), (1e0f64, 0e0f64, 0x3feaed548f090ceeu64), (-1e0f64, 0e0f64, 0xbfeaed548f090ceeu64), (std::f64::consts::PI, 0e0f64, 0x3ca1a62633145c07u64), (std::f64::consts::FRAC_PI_2, 0e0f64, 0x3ff0000000000000u64), (1e2f64, 0e0f64, 0xbfe03425b78c4db8u64), (2.5e-1f64, 0e0f64, 0x3fcfaaeed4f31577u64), ]),
    ("cos", 4, &[(0e0f64, 0e0f64, 0x3ff0000000000000u64), (5e-1f64, 0e0f64, 0x3fec1528065b7d50u64), (1e0f64, 0e0f64, 0x3fe14a280fb5068cu64), (-1e0f64, 0e0f64, 0x3fe14a280fb5068cu64), (std::f64::consts::PI, 0e0f64, 0xbff0000000000000u64), (std::f64::consts::FRAC_PI_2, 0e0f64, 0x3c91a62633145c07u64), (1e2f64, 0e0f64, 0x3feb981dbf665fdfu64), (2.5e-1f64, 0e0f64, 0x3fef01549f7deea1u64), ]),
    ("tan", 5, &[(0e0f64, 0e0f64, 0x0000000000000000u64), (5e-1f64, 0e0f64, 0x3fe17b4f5bf3474au64), (1e0f64, 0e0f64, 0x3ff8eb245cbee3a6u64), (-1e0f64, 0e0f64, 0xbff8eb245cbee3a6u64), (std::f64::consts::PI, 0e0f64, 0xbca1a62633145c07u64), (std::f64::consts::FRAC_PI_2, 0e0f64, 0x434d02967c31cdb5u64), (1e2f64, 0e0f64, 0xbfe2ca74d62b5d38u64), (2.5e-1f64, 0e0f64, 0x3fd05785a43c4c56u64), ]),
    ("asin", 6, &[(0e0f64, 0e0f64, 0x0000000000000000u64), (5e-1f64, 0e0f64, 0x3fe0c152382d7366u64), (-5e-1f64, 0e0f64, 0xbfe0c152382d7366u64), (1e0f64, 0e0f64, 0x3ff921fb54442d18u64), (-1e0f64, 0e0f64, 0xbff921fb54442d18u64), (std::f64::consts::FRAC_1_SQRT_2, 0e0f64, 0x3fe921fb54442d19u64), (2.5e-1f64, 0e0f64, 0x3fd02be9ce0b87cdu64), (-9.9e-1f64, 0e0f64, 0xbff6de3c6f33d51du64), ]),
    ("acos", 7, &[(0e0f64, 0e0f64, 0x3ff921fb54442d18u64), (5e-1f64, 0e0f64, 0x3ff0c152382d7366u64), (-5e-1f64, 0e0f64, 0x4000c152382d7366u64), (1e0f64, 0e0f64, 0x0000000000000000u64), (-1e0f64, 0e0f64, 0x400921fb54442d18u64), (std::f64::consts::FRAC_1_SQRT_2, 0e0f64, 0x3fe921fb54442d18u64), (2.5e-1f64, 0e0f64, 0x3ff51700e0c14b25u64), (-9.9e-1f64, 0e0f64, 0x4008001be1bc011bu64), ]),
    ("atan", 8, &[(0e0f64, 0e0f64, 0x0000000000000000u64), (5e-1f64, 0e0f64, 0x3fddac670561bb4fu64), (1e0f64, 0e0f64, 0x3fe921fb54442d18u64), (-1e0f64, 0e0f64, 0xbfe921fb54442d18u64), (std::f64::consts::PI, 0e0f64, 0x3ff433b8a322ddd2u64), (std::f64::consts::FRAC_PI_2, 0e0f64, 0x3ff00fe987ed02ffu64), (1e2f64, 0e0f64, 0x3ff8f905eb2def22u64), (2.5e-1f64, 0e0f64, 0x3fcf5b75f92c80ddu64), ]),
    ("pow", 9, &[(2e0f64, 1e1f64, 0x4090000000000000u64), (1e1f64, 5e-1f64, 0x40094c583ada5b53u64), (1.5e0f64, 2.5e0f64, 0x40060b9fd68a4554u64), (5e-1f64, -3e0f64, 0x4020000000000000u64), (3e0f64, (1.0f64/3.0f64), 0x3ff7137449123ef6u64), (2e0f64, 5e-1f64, 0x3ff6a09e667f3bcdu64), (7e0f64, -1.5e0f64, 0x3faba539079b6475u64), (1.0001e0f64, 1e3f64, 0x3ff1aec1e81e6de8u64), ]),
    ("round2", 10, &[(1.005e0f64, 2e0f64, 0x3ff0000000000000u64), (2.675e0f64, 2e0f64, 0x400570a3d70a3d71u64), (1.23456789e0f64, 4e0f64, 0x3ff3c0ebedfa43feu64), (-1.005e0f64, 2e0f64, 0xbff0000000000000u64), (5e-1f64, 0e0f64, 0x3ff0000000000000u64), (1.2345678e3f64, -2e0f64, 0x40934c0000000000u64), (9.999e0f64, 2e0f64, 0x4024000000000000u64), (0e0f64, 3e0f64, 0x0000000000000000u64), ]),
    ("rem", 11, &[(1e1f64, 3e0f64, 0x3ff0000000000000u64), (-1e1f64, 3e0f64, 0xbff0000000000000u64), (1.05e1f64, 2.5e-1f64, 0x0000000000000000u64), (1e10f64, 7e0f64, 0x4010000000000000u64), (5e0f64, 2.5e0f64, 0x0000000000000000u64), (-7.5e0f64, 2e0f64, 0xbff8000000000000u64), (3e-1f64, 1e-1f64, 0x3fb9999999999998u64), (1e2f64, 7e-1f64, 0x3fe333333333336cu64), ]),
];

#[test]
fn every_pure_id_matches_its_pinned_bits() {
    let mut checked = 0usize;
    for (name, id, cases) in GOLDEN {
        for (a, b, want) in *cases {
            let got = pinewasm_num::pure(*id, *a, *b).to_bits();
            assert_eq!(
                got, *want,
                "{name}({a:e}, {b:e}) drifted: {got:#018x} != {want:#018x}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 96, "the baseline lost coverage");
}

#[test]
fn the_baseline_covers_every_id() {
    // A new id with no baseline would otherwise be silently unpinned.
    let covered: Vec<u32> = GOLDEN.iter().map(|(_, id, _)| *id).collect();
    for id in 0..pinewasm_num::id::COUNT {
        assert!(covered.contains(&id), "pure id {id} has no pinned baseline");
    }
}

#[test]
fn na_propagates_through_every_id() {
    // The na policy lives in this crate rather than the emitter, so it is part
    // of the numeric core being pinned: na in, na out, for every id.
    for id in 0..pinewasm_num::id::COUNT {
        assert!(
            pinewasm_num::pure(id, f64::NAN, 1.0).is_nan(),
            "id {id} did not propagate na in its first argument"
        );
    }
    // The binary ids take na in *either* argument. Checking only the first
    // would leave `pow(1.0, na)` and a missing round precision unguarded,
    // and those are exactly the shapes a reimplementation gets wrong.
    for id in [
        pinewasm_num::id::POW,
        pinewasm_num::id::ROUND2,
        pinewasm_num::id::REM,
    ] {
        assert!(
            pinewasm_num::pure(id, 1.0, f64::NAN).is_nan(),
            "id {id} did not propagate na in its second argument"
        );
    }
}
