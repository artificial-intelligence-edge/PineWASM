//! Portable round-to-nearest natural logarithm for binary64.
//!
//! [`rounded_log`] returns ln(x) rounded to nearest for every positive finite
//! binary64 x. Two kernels compute it:
//!
//! * The fast kernel ([`fast_log`]) evaluates ln(x) in binary64 arithmetic as
//!   an unevaluated sum `sum + tail` together with a proven bound on its
//!   error. It returns `sum` only when every real number within that bound of
//!   `sum + tail` rounds to `sum`, so what it returns is the correctly rounded
//!   logarithm.
//! * The exact integer interval kernel ([`exact_log`]) decides every input the
//!   fast kernel leaves open. That happens only when ln(x) lies within
//!   `bound` (at most about 2^-59 * |ln(x)|) of a rounding boundary, or when
//!   the rounded logarithm is a power of two and ln(x) lies more than a
//!   quarter of a unit in the last place from it.
//!
//! Both kernels return the correctly rounded value, so which one decides an
//! input never changes a result.
//!
//! # Fast kernel: reduction
//!
//! Write x = M * 2^(e - 52) with the integer M in [2^52, 2^53) (a subnormal
//! is normalized first) and let j be the seven bits of M after its leading
//! one, so m = M * 2^-52 lies in [1 + j/128, 1 + (j + 1)/128). Entry j of
//! [`REDUCTION`] holds an integer `C_j` in [128, 256] with `c_j` = `C_j`/256 close
//! to 1/m; `C_0` = 256 and `C_127` = 128, so an input next to one reduces without
//! a table term. Then
//!
//! ```text
//! r = m * c_j - 1 = (M * C_j - 2^60) * 2^-60.
//! ```
//!
//! The integer M * `C_j` - 2^60 is computed exactly and is below 2^53 in
//! magnitude for every entry, so r is an exact binary64 with |r| < 2^-7
//! (`reduction_is_exact_and_small`). With E = e + [j >= 64],
//!
//! ```text
//! ln(x) = E * ln(2) + L_j + ln(1 + r),   L_j = -ln(c_j) - [j >= 64] * ln(2),
//! ```
//!
//! where |`L_j`| < 0.41, and `L_j` = 0 exactly for j = 0 and j = 127.
//!
//! # Fast kernel: constants
//!
//! ln(2) = `LN2_HI` + `LN2_LO` + d with `LN2_HI` a multiple of 2^-42 and
//! |d| < 2^-101. Entry j of [`LOG_C`] splits `L_j` = `L_hi` + `L_lo` + `d_j` with `L_hi`
//! a multiple of 2^-43, |`L_lo`| <= 2^-44 and |`d_j`| < 2^-97. The test
//! `constants_match_the_exact_kernel` recomputes each constant with the
//! 192-bit interval kernel. Since |E| <= 1074 < 2^11, E * `LN2_HI` is exact, and
//! w = E * `LN2_HI` + `L_hi` is a multiple of 2^-43 below 2^10 in magnitude, so
//! it is exact too.
//!
//! # Fast kernel: evaluation
//!
//! Every operation below is a binary64 operation rounded to nearest; write
//! u = 2^-53. Rust neither contracts nor reassociates floating-point
//! operations, so every target performs exactly these roundings.
//!
//! ```text
//! hi + lo1   = w + r                  exactly (Fast2Sum)
//! k_lo       = E * LN2_LO + L_lo
//! r2         = r * r
//! p          = r2 * -0.5 + (r * r2) * q,   q = Q(r) by Horner,
//!              Q(r) = 1/3 - r/4 + r^2/5 - r^3/6 + r^4/7 - r^5/8 + r^6/9
//! lo         = (lo1 + k_lo) + p
//! sum + tail = hi + lo                exactly (Fast2Sum)
//! bound      = r2 * 2^-52 + |w| * 2^-84
//! ```
//!
//! `Fast2Sum` (a + b = s + t with s = a + b rounded and t = (a - s) + b) is
//! exact when |a| >= |b|. For w + r that holds because w = 0, or E = 0 and
//! |w| = |`L_hi`| >= |r| (`reduction_is_exact_and_small`), or E != 0 and
//! |w| > 0.28 > |r|; for hi + lo because |lo| < |hi| / 64.
//!
//! # Fast kernel: error bound
//!
//! S = sum + tail differs from ln(x) by less than `bound`:
//!
//! * Truncating the series ln(1 + r) after r^9 costs at most
//!   |r|^10 / (10 * (1 - |r|)) < 2^-59.3 * r^2.
//! * Evaluating p: r2 = r^2 (1 + e) with |e| <= u, so r2 * -0.5 is off by at
//!   most u * r^2 / 2. Rounding Q's coefficients costs at most 0.334u and the
//!   Horner steps at most 0.34u, so q is within 0.68u of Q(r). With
//!   |q| < 0.3355, the products r * r2 and (r * r2) * q add at most
//!   1.69u * |r|^3 < 0.014u * r^2, and the final addition at most
//!   0.503u * r^2. With the truncation, p is within 1.03u * r^2 of
//!   ln(1 + r) - r, and |p| < 0.503 * r^2.
//! * The two additions forming lo add at most 2.0001u * (|lo1| + |`k_lo`|) +
//!   u * |p|, with |lo1| <= u * |hi| and |`k_lo`| <= (|E| + 1) * 2^-44.
//! * `k_lo` is within (2|E| + 1) * 2^-97 of E * `LN2_LO` + `L_lo`, and the constants
//!   add |E| * |d| + |`d_j`| < |E| * 2^-101 + 2^-97.
//!
//! The terms in r^2 total less than 1.54u * r^2 < 2^-52.3 * r^2. The others
//! total at most 2^-104.9 * |hi| + |E| * 2^-94.9 + 2^-95, and all of them are
//! zero when w = 0, which happens exactly when E = 0 and j is 0 or 127: then
//! lo1 = `k_lo` = 0 and the constants carry no error. Otherwise |hi| < 3|w|, and
//! either E != 0 and |w| > 0.2895 * |E| (since |`L_hi`| < 0.4036), or E = 0 and
//! |w| = |`L_hi`| >= 2^-8 (`reduction_is_exact_and_small`). Either way the others
//! stay below 2^-86 * |w|. Since r2 >= r^2 (1 - u), the computed `bound`
//! exceeds the error even after its own rounding. The test
//! `fast_kernel_error_stays_within_its_bound` measures the error against
//! `bound` with the 192-bit kernel.
//!
//! # Fast kernel: rounding test
//!
//! `sum` is S rounded to nearest. It is also the rounding of ln(x) when every
//! value within `bound` of S lies strictly between the midpoints on either
//! side of `sum`, that is when |tail| + bound is less than half of the gap
//! from |sum| to the next binary64 magnitude on either side. The gap above
//! |sum| is one unit in the last place; the gap below is the same unless |sum|
//! is a power of two, where it is half of that. The test uses half the gap
//! above, or a quarter of it when |sum| is a power of two, which is never more
//! than half of either gap. Both limits are powers of two, and rounding to
//! nearest is monotonic, so if |tail| + bound rounds to a value below the
//! limit, the exact sum is below it as well. |sum| >= 2^-54 for every x != 1,
//! so the limit is a normal binary64.
//!
//! # Exact kernel
//!
//! Reduce x exactly to m * 2^e with m in [3/4, 3/2), then evaluate
//! 2 * atanh((m - 1)/(m + 1)) as an integer interval. This range
//! keeps the series argument <= 1/5 and avoids cancellation near 1.
//!
//! The fixed-point radius accounts for both truncated arithmetic and
//! the omitted positive tail. The 192-bit pass bounds the full
//! interval width by 2^-180. Since |ln(x)| >= 2^-53 for binary64
//! x != 1, the relative width is at most 2^-127, beyond the published
//! 118-bit worst-case accuracy requirement for binary64 logarithms.
//! See <https://www.numdam.org/item/10.1051/ita%3A2007003.pdf>.

// floor(ln(2) * 2^192), least significant limb first.
const LN_2: [u64; 3] = [0x40f343267298b62d, 0xc9e3b39803f2f6af, 0xb17217f7d1cf79ab];

/// ln(2) rounded to a multiple of 2^-42, so E * `LN2_HI` is exact for
/// |E| < 2^11.
const LN2_HI: f64 = f64::from_bits(0x3fe62e42fefa3800);
/// ln(2) - `LN2_HI` rounded to nearest.
const LN2_LO: f64 = f64::from_bits(0x3d2ef35793c76730);

/// `C_j` - 128 for each seven-bit significand prefix j; see the module docs.
const REDUCTION: [u8; 128] = [
    128, 125, 123, 121, 119, 117, 116, 114, 112, 110, 109, 107, 105, 104, 102, 100, 99, 97, 96, 94,
    93, 91, 90, 88, 87, 85, 84, 83, 81, 80, 79, 77, 76, 75, 74, 72, 71, 70, 69, 68, 66, 65, 64, 63,
    62, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41, 40, 40,
    39, 38, 37, 36, 35, 35, 34, 33, 32, 31, 31, 30, 29, 28, 28, 27, 26, 25, 25, 24, 23, 23, 22, 21,
    21, 20, 19, 19, 18, 17, 17, 16, 15, 15, 14, 14, 13, 12, 12, 11, 11, 10, 9, 9, 8, 8, 7, 7, 6, 5,
    5, 4, 4, 3, 3, 2, 2, 1, 1, 0,
];

/// The bits of (`L_hi`, `L_lo`) for each j, with
/// `L_j` = -ln(`C_j` / 256) - [j >= 64] * ln(2); see the module docs.
const LOG_C: [(u64, u64); 128] = [
    (0x0000000000000000, 0x0000000000000000),
    (0x3f882448a3890000, 0xbd275577da74f640),
    (0x3f9432a925980000, 0x3d098139928637fe),
    (0x3f9c63d2ec148000, 0x3d2578c63f9eb2f3),
    (0x3fa252f32f8d0000, 0x3d283e9ae021b67b),
    (0x3fa67c94f2d4c000, 0xbd029efbec19afa2),
    (0x3fa894aa149fc000, 0xbd197995d05a267d),
    (0x3faccb73cdddc000, 0xbd1a68f247d82807),
    (0x3fb08598b59e4000, 0xbd17e5dd7009902c),
    (0x3fb2aa04a4472000, 0xbd20b6e8ae9c697d),
    (0x3fb3bdf5a7d1e000, 0x3d2cc85ea5db4ed7),
    (0x3fb5e95a4d97a000, 0xbd2c69063c5d1d1e),
    (0x3fb8197e2f40e000, 0x3d0f80dcf96ffdf7),
    (0x3fb9335e5d594000, 0x3d23115c3abd47da),
    (0x3fbb6ac88dad6000, 0xbd1390802bf768e5),
    (0x3fbda72763844000, 0x3d1a89401fa71733),
    (0x3fbec739830a2000, 0xbd2dc068afe645e0),
    (0x3fc08598b59e4000, 0xbd27e5dd7009902c),
    (0x3fc1178e8227e000, 0x3d21ef78ce2d07f2),
    (0x3fc23d712a49c000, 0x3d100d238fd3df5c),
    (0x3fc2d1610c868000, 0x3d039d6ccb81b4a1),
    (0x3fc3fb45a5993000, 0xbd2cd1d87e6a354d),
    (0x3fc4913d8333b000, 0x3d25837954fdb678),
    (0x3fc5bf406b544000, 0xbd127023eb68981c),
    (0x3fc6574ebe8c1000, 0x3d19cf8b2c3c2e78),
    (0x3fc7898d85445000, 0xbd1c661070914305),
    (0x3fc823c16551a000, 0x3d1e0ddb9a631e83),
    (0x3fc8beafeb390000, 0xbd073d54aae92cd1),
    (0x3fc9f6c407089000, 0x3d29904d6865817a),
    (0x3fca93ed3c8ae000, 0xbd28724350562169),
    (0x3fcb31d8575bd000, 0xbd0c358d4eace1aa),
    (0x3fcc6ffbc6f01000, 0xbcf1ec72c5962bd2),
    (0x3fcd1037f2656000, 0xbd084a7e75b6f6e4),
    (0x3fcdb13db0d49000, 0xbd2aff2af715b035),
    (0x3fce530effe71000, 0x3cc212276041f430),
    (0x3fcf991c6cb3b000, 0x3d1bcbecca0cdf30),
    (0x3fd01eae5626c800, 0xbd16f08c1485e94a),
    (0x3fd07138604d5800, 0x3cf89cdb16ed4e91),
    (0x3fd0c42d67616000, 0x3d27188b163ceae9),
    (0x3fd1178e8227e800, 0xbd2c210e63a5f01c),
    (0x3fd1bf99635a6800, 0x3d2ca6ed5147bdb7),
    (0x3fd214456d0eb800, 0x3d0a87deba46baea),
    (0x3fd269621134d800, 0x3d2c93c1df5bb3b6),
    (0x3fd2bef07cdc9000, 0x3d2a9cfa4a5004f4),
    (0x3fd314f1e1d36000, 0xbd28e27ad3213cb8),
    (0x3fd36b6776be1000, 0x3d116ecdb0f177c8),
    (0x3fd3c25277333000, 0x3d183b54b606bd5c),
    (0x3fd419b423d5e800, 0x3d08e436ec90e09d),
    (0x3fd4718dc271c800, 0xbd2f27ce0967d675),
    (0x3fd4c9e09e173000, 0xbd2e20891b0ad8a4),
    (0x3fd522ae0738a000, 0x3d2ebe708164c759),
    (0x3fd57bf753c8d000, 0x3d1fadedee5d40ef),
    (0x3fd5d5bddf596000, 0xbd0a0b2a08a465dc),
    (0x3fd630030b3ab000, 0xbd2db623e731ae00),
    (0x3fd68ac83e9c6800, 0x3d20a0d32756eba0),
    (0x3fd6e60ee6af1800, 0x3d1721657c222d87),
    (0x3fd741d876c67800, 0x3d2d8b0949dc60b3),
    (0x3fd79e26687cf800, 0x3d29ec7d2efd1778),
    (0x3fd7fafa3bd81800, 0xbd272090c812566a),
    (0x3fd85855776dc800, 0x3d2fd56f3333778a),
    (0x3fd8b639a88b3000, 0xbd205ae1e5e70470),
    (0x3fd914a8635bf800, 0xbd1766b52ee6307d),
    (0x3fd973a343135800, 0xbd152313a502d9f0),
    (0x3fd9d32bea15f000, 0xbd26279e10d0c0b0),
    (0xbfd22941fbcf7800, 0xbd165a242853da76),
    (0xbfd1c898c1699800, 0xbd1fafbc68e75404),
    (0xbfd1675cababa800, 0x3d1f1fc63382a8f0),
    (0xbfd1675cababa800, 0x3d1f1fc63382a8f0),
    (0xbfd1058bf9ae4800, 0xbd26a8c4fd055a66),
    (0xbfd0a324e2739000, 0xbd0c6bee7ef4030e),
    (0xbfd0402594b4d000, 0xbcf036b89ef42d7f),
    (0xbfcfb9186d5e4000, 0x3d0d572aab993c87),
    (0xbfcef0adcbdc6000, 0x3d2b26b79c86af24),
    (0xbfcef0adcbdc6000, 0x3d2b26b79c86af24),
    (0xbfce27076e2af000, 0xbd172f4f543fff10),
    (0xbfcd5c216b4fc000, 0x3d21ba91bbca681b),
    (0xbfcc8ff7c79aa000, 0x3d27794f689f8434),
    (0xbfcbc286742d9000, 0x3d194eb0318bb78f),
    (0xbfcbc286742d9000, 0x3d194eb0318bb78f),
    (0xbfcaf3c94e80c000, 0x3cba4e633fcd9066),
    (0xbfca23bc1fe2b000, 0xbd258c64dc46c1ea),
    (0xbfc9525a9cf45000, 0xbd2ad1d904c1d4e3),
    (0xbfc9525a9cf45000, 0xbd2ad1d904c1d4e3),
    (0xbfc87fa06520d000, 0x3d2bbdbf7fdbfa09),
    (0xbfc7ab890210e000, 0x3d2bdb9072534a58),
    (0xbfc6d60fe719d000, 0xbd10e46aa3b2e266),
    (0xbfc6d60fe719d000, 0xbd10e46aa3b2e266),
    (0xbfc5ff3070a79000, 0xbd1e9e439f105039),
    (0xbfc526e5e3a1b000, 0xbd20de8b90075b8f),
    (0xbfc526e5e3a1b000, 0xbd20de8b90075b8f),
    (0xbfc44d2b6ccb8000, 0x3d170cc16135783c),
    (0xbfc371fc201e9000, 0x3cf178864d27543a),
    (0xbfc371fc201e9000, 0x3cf178864d27543a),
    (0xbfc29552f81ff000, 0xbd248d301771c408),
    (0xbfc1b72ad52f6000, 0xbd2e80a41811a396),
    (0xbfc1b72ad52f6000, 0xbd2e80a41811a396),
    (0xbfc0d77e7cd09000, 0x3d0a699688e85bf4),
    (0xbfbfec9131dbe000, 0xbd2575545ca333f2),
    (0xbfbfec9131dbe000, 0xbd2575545ca333f2),
    (0xbfbe27076e2b0000, 0x3d2a342c2af0003c),
    (0xbfbc5e548f5bc000, 0xbd1d0c57585fbe06),
    (0xbfbc5e548f5bc000, 0xbd1d0c57585fbe06),
    (0xbfba926d3a4ae000, 0x3d253935e85baac8),
    (0xbfba926d3a4ae000, 0x3d253935e85baac8),
    (0xbfb8c345d631a000, 0x3d137c294d2f5668),
    (0xbfb6f0d28ae56000, 0xbd269737c93373da),
    (0xbfb6f0d28ae56000, 0xbd269737c93373da),
    (0xbfb51b073f062000, 0x3d1f025b61c65e57),
    (0xbfb51b073f062000, 0x3d1f025b61c65e57),
    (0xbfb341d7961be000, 0x3d2c5edaccf913df),
    (0xbfb16536eea38000, 0x3d147c5e768fa309),
    (0xbfb16536eea38000, 0x3d147c5e768fa309),
    (0xbfaf0a30c0118000, 0x3d2d599e83368e91),
    (0xbfaf0a30c0118000, 0x3d2d599e83368e91),
    (0xbfab42dd71198000, 0x3d1c827ae5d6704c),
    (0xbfab42dd71198000, 0x3d1c827ae5d6704c),
    (0xbfa77458f632c000, 0xbd2cfc4634f2a1ee),
    (0xbfa39e87b9fec000, 0x3cf502b7f526feaa),
    (0xbfa39e87b9fec000, 0x3cf502b7f526feaa),
    (0xbf9f829b0e780000, 0xbd2980267c7e09e4),
    (0xbf9f829b0e780000, 0xbd2980267c7e09e4),
    (0xbf97b91b07d58000, 0xbd288d5493faa639),
    (0xbf97b91b07d58000, 0xbd288d5493faa639),
    (0xbf8fc0a8b0fc0000, 0xbcdf1e7cf6d3a69c),
    (0xbf8fc0a8b0fc0000, 0xbcdf1e7cf6d3a69c),
    (0xbf7fe02a6b100000, 0xbd19e23f0dda40e4),
    (0xbf7fe02a6b100000, 0xbd19e23f0dda40e4),
    (0x0000000000000000, 0x0000000000000000),
];

// ln(1 + r) - r + r^2/2 = r^3 * Q(r): the series coefficients rounded to
// nearest. 1/4 and 1/8 are exact.
const Q3: f64 = 1.0 / 3.0;
const Q4: f64 = -1.0 / 4.0;
const Q5: f64 = 1.0 / 5.0;
const Q6: f64 = -1.0 / 6.0;
const Q7: f64 = 1.0 / 7.0;
const Q8: f64 = -1.0 / 8.0;
const Q9: f64 = 1.0 / 9.0;

const SIGN: u64 = 1 << 63;
const EXPONENT: u64 = 0x7ff << 52;
const FRACTION: u64 = (1 << 52) - 1;
const TWO_POW_M52: f64 = f64::from_bits((1023 - 52) << 52);
const TWO_POW_M60: f64 = f64::from_bits((1023 - 60) << 52);
const TWO_POW_M84: f64 = f64::from_bits((1023 - 84) << 52);

#[derive(Clone, Copy)]
struct Fixed<const N: usize>([u64; N]);

impl<const N: usize> Fixed<N> {
    const ZERO: Self = Self([0; N]);

    fn small(n: u64) -> Self {
        let mut out = Self::ZERO;
        out.0[0] = n;
        out
    }

    fn ratio(numerator: u64, denominator: u64) -> Self {
        let mut out = Self::ZERO;
        let mut remainder = numerator;
        for i in (0..N - 1).rev() {
            let digit = u128::from(remainder) << 64;
            out.0[i] = (digit / u128::from(denominator)) as u64;
            remainder = (digit % u128::from(denominator)) as u64;
        }
        out
    }

    fn add(self, other: Self) -> Self {
        let mut out = Self::ZERO;
        let mut carry = false;
        for i in 0..N {
            let (sum, a) = self.0[i].overflowing_add(other.0[i]);
            let (sum, b) = sum.overflowing_add(u64::from(carry));
            out.0[i] = sum;
            carry = a || b;
        }
        debug_assert!(!carry);
        out
    }

    fn sub(self, other: Self) -> Self {
        let mut out = Self::ZERO;
        let mut borrow = false;
        for i in 0..N {
            let (diff, a) = self.0[i].overflowing_sub(other.0[i]);
            let (diff, b) = diff.overflowing_sub(u64::from(borrow));
            out.0[i] = diff;
            borrow = a || b;
        }
        debug_assert!(!borrow);
        out
    }

    fn multiply_small(self, n: u64) -> Self {
        let mut out = Self::ZERO;
        let mut carry = 0;
        for i in 0..N {
            let p = u128::from(self.0[i]) * u128::from(n) + carry;
            out.0[i] = p as u64;
            carry = p >> 64;
        }
        debug_assert_eq!(carry, 0);
        out
    }

    fn divide_small(self, divisor: u64) -> Self {
        let mut out = Self::ZERO;
        let mut remainder = 0;
        for i in (0..N).rev() {
            let digit = (remainder << 64) | u128::from(self.0[i]);
            out.0[i] = (digit / u128::from(divisor)) as u64;
            remainder = digit % u128::from(divisor);
        }
        out
    }

    fn multiply_fractions(self, other: Self) -> Self {
        let fraction = N - 1;
        debug_assert!(fraction <= 3 && self.0[fraction] == 0 && other.0[fraction] == 0);
        let mut product = [0_u64; 6];
        for i in 0..fraction {
            let mut carry = 0;
            for j in 0..fraction {
                let p = u128::from(self.0[i]) * u128::from(other.0[j])
                    + u128::from(product[i + j])
                    + carry;
                product[i + j] = p as u64;
                carry = p >> 64;
            }
            product[i + fraction] = carry as u64;
        }
        let mut out = Self::ZERO;
        out.0[..fraction].copy_from_slice(&product[fraction..2 * fraction]);
        out
    }

    fn to_nearest(self) -> f64 {
        let i = self.0.iter().rposition(|&limb| limb != 0).unwrap();
        let high = i * 64 + 63 - self.0[i].leading_zeros() as usize;
        // A nonzero log(binary64) has magnitude at least 2^-53, and
        // no finite binary64 log approaches the normal/subnormal boundary.
        let shift = high - 52;
        let limb = shift / 64;
        let offset = shift % 64;
        let mut significand = self.0[limb] >> offset;
        if offset != 0 && limb + 1 < N {
            significand |= self.0[limb + 1] << (64 - offset);
        }
        let guard = shift - 1;
        let below = self.0[guard / 64] & ((1_u64 << (guard % 64)) - 1);
        if (self.0[guard / 64] >> (guard % 64)) & 1 != 0
            && (significand & 1 != 0
                || below != 0
                || self.0[..guard / 64].iter().any(|&part| part != 0))
        {
            significand += 1;
        }
        let bias = 1023_i32 + high as i32 - ((N - 1) * 64) as i32;
        f64::from_bits(((bias as u64 - 1) << 52) + significand)
    }
}

/// |ln(significand * 2^(exponent - 52))| as an N-limb fixed-point value with
/// N - 1 fractional limbs, and the radius of the interval around it that
/// contains the exact magnitude.
fn log_magnitude<const N: usize>(significand: u64, mut exponent: i32) -> (Fixed<N>, Fixed<N>) {
    let mut denominator = 1_u64 << 52;
    if significand >= denominator * 3 / 2 {
        denominator <<= 1;
        exponent += 1;
    }
    let numerator = significand.abs_diff(denominator);
    let mut term = Fixed::<N>::ratio(numerator, significand + denominator);
    let z_squared = term.multiply_fractions(term);
    let mut sum = term;
    let precision = (N - 1) * 64;
    for k in (3..=precision).step_by(2) {
        term = term.multiply_fractions(z_squared);
        if term.0.iter().all(|&limb| limb == 0) {
            break;
        }
        sum = sum.add(term.divide_small(k as u64));
    }
    sum = sum.multiply_small(2);
    let mut ln_two = Fixed::<N>::ZERO;
    ln_two.0[..N - 1].copy_from_slice(&LN_2[4 - N..]);
    let mut magnitude = ln_two.multiply_small(u64::from(exponent.unsigned_abs()));
    if exponent == 0 {
        magnitude = sum;
    } else if (exponent < 0) == (significand < denominator) {
        magnitude = magnitude.add(sum);
    } else {
        magnitude = magnitude.sub(sum);
    }
    // In units of 2^-precision: z is truncated by <1 and z² by <1.4.
    // Each truncated power stays within 4 units, so dividing by k>=3
    // costs <3 units per addend. The first omitted zero term and tail
    // together cost <5 units before doubling. The ln(2) term costs
    // <1074 units, since its exponent is at most 1074 in magnitude.
    let radius = Fixed::small((3 * precision + 1100) as u64);
    (magnitude, radius)
}

fn log_interval<const N: usize>(significand: u64, exponent: i32) -> (f64, f64) {
    let (magnitude, radius) = log_magnitude::<N>(significand, exponent);
    (
        magnitude.sub(radius).to_nearest(),
        magnitude.add(radius).to_nearest(),
    )
}

/// The fast kernel's approximation of ln(x): `sum + tail` is within `bound`
/// of ln(x), and `sum` is `sum + tail` rounded to nearest.
#[derive(Clone, Copy)]
struct Approximation {
    sum: f64,
    tail: f64,
    bound: f64,
}

/// Evaluates ln(significand * 2^(exponent - 52)) for x != 1, with the
/// significand in [2^52, 2^53). See the module docs for the error bound.
#[inline]
fn approximate_log(significand: u64, exponent: i32) -> Approximation {
    let j = ((significand >> 45) & 127) as usize;
    let c = u64::from(REDUCTION[j]) + 128;
    // Exact: |significand * c - 2^60| < 2^53, and scaling by 2^-60 cannot
    // underflow.
    let r = ((significand * c) as i64 - (1 << 60)) as f64 * TWO_POW_M60;
    let (l_hi, l_lo) = LOG_C[j];
    let k = f64::from(exponent + i32::from(j >= 64));
    // Exact: see the module docs.
    let w = k * LN2_HI + f64::from_bits(l_hi);
    let hi = w + r;
    let lo1 = (w - hi) + r;
    let k_lo = k * LN2_LO + f64::from_bits(l_lo);
    let r2 = r * r;
    let q = Q3 + r * (Q4 + r * (Q5 + r * (Q6 + r * (Q7 + r * (Q8 + r * Q9)))));
    let p = r2 * -0.5 + (r * r2) * q;
    let lo = (lo1 + k_lo) + p;
    let sum = hi + lo;
    let tail = (hi - sum) + lo;
    Approximation {
        sum,
        tail,
        bound: r2 * TWO_POW_M52 + w.abs() * TWO_POW_M84,
    }
}

/// The correctly rounded ln(x), or `None` when the approximation's error
/// bound reaches a rounding boundary.
#[inline]
fn fast_log(significand: u64, exponent: i32) -> Option<f64> {
    let Approximation { sum, tail, bound } = approximate_log(significand, exponent);
    let magnitude = sum.to_bits() & !SIGN;
    // Half a unit in the last place of |sum|: 2^(exponent of sum - 53).
    let mut limit = f64::from_bits((magnitude & EXPONENT) - (53 << 52));
    if magnitude & FRACTION == 0 {
        limit *= 0.5;
    }
    if tail.abs() + bound < limit {
        Some(sum)
    } else {
        None
    }
}

/// The exact interval kernel, for the inputs the fast kernel leaves open.
#[cold]
#[inline(never)]
fn exact_log(x: f64, significand: u64, exponent: i32) -> f64 {
    // Away from one, |ln(x)| >= ln(2), so a 64-bit fractional pass
    // can decide most results without losing its rounding guard bit.
    if !(0.5..2.0).contains(&x) {
        let (lo, hi) = log_interval::<2>(significand, exponent);
        if lo == hi {
            return if x < 1.0 { -lo } else { lo };
        }
    }
    let (lo, hi) = log_interval::<3>(significand, exponent);
    let rounded = if lo == hi {
        lo
    } else {
        let (lo, hi) = log_interval::<4>(significand, exponent);
        debug_assert_eq!(lo, hi, "192-bit logarithm interval must round uniquely");
        lo
    };
    if x < 1.0 {
        -rounded
    } else {
        rounded
    }
}

/// x = significand * 2^(exponent - 52) with the significand in [2^52, 2^53),
/// for positive finite x.
fn decompose(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let mut significand = bits & FRACTION;
    let mut exponent = ((bits >> 52) as i32) - 1023;
    if exponent == -1023 {
        let shift = significand.leading_zeros() - 11;
        significand <<= shift;
        exponent = -1022 - shift as i32;
    } else {
        significand |= 1 << 52;
    }
    (significand, exponent)
}

pub(crate) fn rounded_log(x: f64) -> f64 {
    if !x.is_finite() || x <= 0.0 {
        return libm::log(x);
    }
    if x == 1.0 {
        return 0.0;
    }
    let (significand, exponent) = decompose(x);
    match fast_log(significand, exponent) {
        Some(rounded) => rounded,
        None => exact_log(x, significand, exponent),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    #[test]
    fn binary64_boundaries_round_to_nearest() {
        for (input, expected) in [
            (0x3ff0000000000000, 0x0000000000000000),
            (0x4008000000000000, 0x3ff193ea7aad030b),
            (0x3fefffffffffffff, 0xbca0000000000000),
            (0x3ff0000000000001, 0x3cafffffffffffff),
            (0x0000000000000001, 0xc0874385446d71c3),
            (0x0010000000000000, 0xc086232bdd7abcd2),
            (0x000fffffffffffff, 0xc086232bdd7abcd2),
            (0x7fefffffffffffff, 0x40862e42fefa39ef),
            (0x3ffa6ae5142326b5, 0x3fe00bcc31ebded7),
        ] {
            assert_eq!(
                rounded_log(f64::from_bits(input)).to_bits(),
                expected,
                "input {input:016x}"
            );
        }
    }

    #[test]
    fn special_inputs_keep_existing_log_semantics() {
        assert_eq!(rounded_log(0.0), f64::NEG_INFINITY);
        assert_eq!(rounded_log(-0.0), f64::NEG_INFINITY);
        assert_eq!(rounded_log(f64::INFINITY), f64::INFINITY);
        assert!(rounded_log(-1.0).is_nan());
        assert!(rounded_log(f64::NAN).is_nan());
    }

    /// A signed fixed-point value with 192 fractional bits.
    #[derive(Clone, Copy)]
    struct Signed {
        negative: bool,
        magnitude: Fixed<4>,
    }

    fn compare(a: Fixed<4>, b: Fixed<4>) -> core::cmp::Ordering {
        a.0.iter().rev().cmp(b.0.iter().rev())
    }

    impl Signed {
        /// `value` exactly, except that bits below 2^-192 are truncated.
        fn from_f64(value: f64) -> Self {
            let bits = value.to_bits();
            let mut magnitude = Fixed::<4>::ZERO;
            if bits & !SIGN != 0 {
                let biased = ((bits >> 52) & 0x7ff) as i32;
                assert!(biased != 0 && biased != 0x7ff, "{value:e} must be normal");
                let mantissa = (bits & FRACTION) | (1 << 52);
                // value = mantissa * 2^(biased - 1075); bit 0 of the
                // mantissa lands at bit `shift` of the fixed-point integer.
                let shift = biased - 1075 + 192;
                if shift < 0 {
                    magnitude.0[0] = mantissa.checked_shr(shift.unsigned_abs()).unwrap_or(0);
                } else {
                    let (limb, offset) = ((shift / 64) as usize, shift % 64);
                    magnitude.0[limb] |= mantissa << offset;
                    if offset > 11 {
                        magnitude.0[limb + 1] |= mantissa >> (64 - offset);
                    }
                }
            }
            Self {
                negative: bits & SIGN != 0,
                magnitude,
            }
        }

        fn add(self, other: Self) -> Self {
            if self.negative == other.negative {
                return Self {
                    negative: self.negative,
                    magnitude: self.magnitude.add(other.magnitude),
                };
            }
            if compare(self.magnitude, other.magnitude).is_ge() {
                Self {
                    negative: self.negative,
                    magnitude: self.magnitude.sub(other.magnitude),
                }
            } else {
                Self {
                    negative: other.negative,
                    magnitude: other.magnitude.sub(self.magnitude),
                }
            }
        }

        fn negate(self) -> Self {
            Self {
                negative: !self.negative,
                magnitude: self.magnitude,
            }
        }
    }

    fn approximately(value: Fixed<4>) -> f64 {
        value
            .0
            .iter()
            .enumerate()
            .map(|(i, &limb)| limb as f64 * 2.0_f64.powi(64 * i as i32 - 192))
            .sum()
    }

    /// ln(x) for positive finite x, as a 192-bit value and the radius of the
    /// interval around it that contains the exact logarithm.
    fn exact(x: f64) -> (Signed, Fixed<4>) {
        let (significand, exponent) = decompose(x);
        let (magnitude, radius) = log_magnitude::<4>(significand, exponent);
        (
            Signed {
                negative: x < 1.0,
                magnitude,
            },
            radius,
        )
    }

    /// SplitMix64, so every run tests the same inputs.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 * TWO_POW_M53
        }
    }

    const TWO_POW_M53: f64 = f64::from_bits((1023 - 53) << 52);

    /// The input families the fast kernel has to cover, `count` of each.
    fn families(count: usize, seed: u64) -> [(&'static str, Vec<f64>); 6] {
        let mut rng = Rng(seed);
        let mut any = Vec::with_capacity(count);
        while any.len() < count {
            let x = f64::from_bits(rng.next() >> 1);
            if x.is_finite() && x > 0.0 && x != 1.0 {
                any.push(x);
            }
        }
        let subnormal = (0..count)
            .map(|_| f64::from_bits(1 + rng.next() % ((1 << 52) - 1)))
            .collect();
        let around_one = (0..count)
            .map(|_| 0.5 + 1.5 * rng.unit())
            .filter(|&x| x != 1.0)
            .collect();
        let distance = (0..count)
            .map(|_| 1.0 + (rng.unit() - rng.unit()).abs())
            .filter(|&x| x != 1.0)
            .collect();
        let near_one = (0..count)
            .map(|_| {
                let scale = 2.0_f64.powi(-((rng.next() % 38) as i32) - 7);
                1.0 + (rng.unit() * 2.0 - 1.0) * scale
            })
            .filter(|&x| x != 1.0)
            .collect();
        let mut edges = Vec::new();
        for j in 0..128_u64 {
            let first = (128 + j) << 45;
            let last = ((129 + j) << 45) - 1;
            for significand in [first, first + 1, last - 1, last] {
                for biased in [1_u64, 2, 1021, 1022, 1023, 1024, 2045, 2046] {
                    let x = f64::from_bits((biased << 52) | (significand & FRACTION));
                    if x != 1.0 {
                        edges.push(x);
                    }
                }
            }
        }
        for k in 1..=1024 {
            edges.push(1.0 + f64::from(k) * f64::EPSILON);
            edges.push(1.0 - f64::from(k) * f64::EPSILON / 2.0);
        }
        for exponent in -1074_i32..=1023 {
            if exponent != 0 {
                edges.push(f64::from_bits(if exponent >= -1022 {
                    ((exponent + 1023) as u64) << 52
                } else {
                    1 << (exponent + 1074)
                }));
            }
        }
        [
            ("any positive", any),
            ("subnormal", subnormal),
            ("[0.5, 2)", around_one),
            ("1 + |a - b|", distance),
            ("near one", near_one),
            ("edges", edges),
        ]
    }

    #[test]
    fn reduction_is_exact_and_small() {
        for j in 0..128_u64 {
            let c = u64::from(REDUCTION[j as usize]) + 128;
            assert_eq!(c == 256, j == 0, "entry {j}");
            assert_eq!(c == 128, j == 127, "entry {j}");
            // m * c - 1 grows with m, so the ends of the subinterval bound it.
            let mut widest = 0.0_f64;
            for significand in [(128 + j) << 45, ((129 + j) << 45) - 1] {
                let reduced = (significand * c) as i64 - (1 << 60);
                assert!(reduced.unsigned_abs() < 1 << 53, "entry {j}");
                widest = widest.max((reduced as f64 * TWO_POW_M60).abs());
            }
            assert!(widest < 1.0 / 128.0, "entry {j}");
            let (l_hi, l_lo) = LOG_C[j as usize];
            if j == 0 || j == 127 {
                assert_eq!((l_hi, l_lo), (0, 0), "entry {j}");
            } else {
                // Fast2Sum(w, r) needs |w| >= |r| when E = 0, and the error
                // bound needs 2^-8 <= |L_hi| < 0.4036.
                let l_hi = f64::from_bits(l_hi).abs();
                assert!(l_hi >= widest, "entry {j}");
                assert!((1.0 / 256.0..0.4036).contains(&l_hi), "entry {j}");
            }
        }
    }

    #[test]
    fn constants_match_the_exact_kernel() {
        let two_pow_m42 = f64::from_bits((1023 - 42) << 52);
        assert_eq!(LN2_HI % two_pow_m42, 0.0);
        let (ln2, radius) = exact(2.0);
        let split = Signed::from_f64(LN2_HI).add(Signed::from_f64(LN2_LO));
        let error = split.add(ln2.negate()).magnitude.add(radius);
        assert!(approximately(error) < 2.0_f64.powi(-101), "ln(2) split");

        let two_pow_m43 = f64::from_bits((1023 - 43) << 52);
        let two_pow_m44 = f64::from_bits((1023 - 44) << 52);
        for j in 1..127 {
            let c = f64::from(REDUCTION[j]) + 128.0;
            // L_j = -ln(c/256) for j < 64 and -ln(c/128) for j >= 64.
            let reduced = if j < 64 { c / 256.0 } else { c / 128.0 };
            let (log, radius) = exact(reduced);
            let (l_hi, l_lo) = (f64::from_bits(LOG_C[j].0), f64::from_bits(LOG_C[j].1));
            assert_eq!(l_hi % two_pow_m43, 0.0, "entry {j}");
            assert!(l_lo.abs() <= two_pow_m44, "entry {j}");
            assert_eq!(l_hi < 0.0, j >= 64, "entry {j}");
            let split = Signed::from_f64(l_hi).add(Signed::from_f64(l_lo));
            let error = split.add(log).magnitude.add(radius);
            assert!(approximately(error) < 2.0_f64.powi(-97), "entry {j}");
        }
    }

    /// Checks the documented error bound against the 192-bit kernel, and that
    /// every result the fast kernel decides is the one the exact kernel
    /// gives. Returns the largest error relative to its bound and the
    /// fraction of inputs decided.
    fn check_fast_kernel(inputs: &[f64]) -> (f64, f64) {
        let mut worst = 0.0_f64;
        let mut decided = 0_u32;
        for &x in inputs {
            let (significand, exponent) = decompose(x);
            let Approximation { sum, tail, bound } = approximate_log(significand, exponent);
            let (log, radius) = exact(x);
            // Truncating `tail` to 192 bits loses less than 2^-192.
            let approximation = Signed::from_f64(sum).add(Signed::from_f64(tail));
            let error = approximation
                .add(log.negate())
                .magnitude
                .add(radius)
                .add(Fixed::small(1));
            let limit = Signed::from_f64(bound).magnitude;
            assert!(
                compare(error, limit).is_lt(),
                "x = {:#018x}: error {:e} exceeds bound {bound:e}",
                x.to_bits(),
                approximately(error)
            );
            worst = worst.max(approximately(error) / bound);
            if let Some(rounded) = fast_log(significand, exponent) {
                decided += 1;
                assert_eq!(
                    rounded.to_bits(),
                    exact_log(x, significand, exponent).to_bits(),
                    "x = {:#018x}",
                    x.to_bits()
                );
            }
        }
        (worst, f64::from(decided) / inputs.len() as f64)
    }

    #[test]
    fn fast_kernel_error_stays_within_its_bound() {
        for (family, inputs) in families(4096, 0x6c6e_5f62_6f75_6e64) {
            let (worst, decided) = check_fast_kernel(&inputs);
            assert!(worst < 1.0, "{family}");
            std::eprintln!(
                "{family}: {} inputs, error at most {worst:.3} of the bound, {:.3}% decided",
                inputs.len(),
                decided * 100.0
            );
        }
    }

    /// The fast kernel must stay fast: if it starts leaving more inputs to the
    /// exact kernel, `math.log` slows down again.
    #[test]
    fn fast_kernel_decides_almost_every_input() {
        for (family, inputs) in families(20_000, 0x6465_6369_6465_6421) {
            let decided = inputs
                .iter()
                .filter(|&&x| {
                    let (significand, exponent) = decompose(x);
                    fast_log(significand, exponent).is_some()
                })
                .count();
            let rate = decided as f64 / inputs.len() as f64;
            let required = if family == "edges" { 0.99 } else { 0.995 };
            assert!(rate >= required, "{family}: {rate}");
        }
    }

    #[test]
    fn exact_kernel_settles_what_the_fast_kernel_leaves_open() {
        // ln(1 - 2^-53) = -2^-53 - 2^-107 - ... rounds to a power of two;
        // ln(1 - 2^-52) lies within 2^-157 of a rounding midpoint, and the
        // logarithm of the last input within 2^-71 of one.
        for x in [
            1.0 - f64::EPSILON / 2.0,
            1.0 - f64::EPSILON,
            f64::from_bits(0x3fef_a2f0_83a8_ebba),
        ] {
            let (significand, exponent) = decompose(x);
            assert_eq!(fast_log(significand, exponent), None, "{x:e}");
            assert_eq!(
                rounded_log(x).to_bits(),
                exact_log(x, significand, exponent).to_bits()
            );
        }
    }

    /// A longer run of `fast_kernel_error_stays_within_its_bound`. Run it in
    /// release mode with `--ignored --nocapture`; `PINEWASM_LN_CHECK_COUNT`
    /// sets the number of inputs per family and `PINEWASM_LN_CHECK_SEED` the
    /// seed.
    #[test]
    #[ignore = "long run; set PINEWASM_LN_CHECK_COUNT and run in release mode"]
    fn fast_kernel_long_check() {
        let count = std::env::var("PINEWASM_LN_CHECK_COUNT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1_000_000);
        let seed = std::env::var("PINEWASM_LN_CHECK_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0x6c6f_6e67);
        for (family, inputs) in families(count, seed) {
            let (worst, decided) = check_fast_kernel(&inputs);
            std::eprintln!(
                "{family}: {} inputs, error at most {worst:.4} of the bound, {:.4}% decided",
                inputs.len(),
                decided * 100.0
            );
        }
    }
}
