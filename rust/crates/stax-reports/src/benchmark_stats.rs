//! `services/benchmark_stats.py` — the comparative benchmark's honesty layer.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `SEED` … `MIN_EFFECT_GRADE` | 65-85 | [`SEED`] … [`MIN_EFFECT_GRADE`] |
//! | `z_for_confidence` | 91-98 | [`z_for_confidence`] |
//! | `wilson_interval` | 104-127 | [`wilson_interval`] |
//! | `percentile` | 133-151 | [`percentile`] |
//! | `percentile_bootstrap_ci` | 154-189 | [`percentile_bootstrap_ci`] |
//! | `benjamini_hochberg` | 195-220 | [`benjamini_hochberg`] |
//! | `pooled_rate` | 226-238 | [`pooled_rate`] |
//! | `standardized_rate` | 241-252 | [`standardized_rate`] |
//! | `standardized_difference` | 255-274 | [`standardized_difference`] |
//! | `relative_delta` | 280-287 | [`relative_delta`] |
//! | `risk_difference` | 290-292 | [`risk_difference`] |
//! | `confidence_bucket` | 298-312 | [`confidence_bucket`] |
//! | — | `statistics.NormalDist` | [`normal_cdf`], [`normal_inv_cdf`] |
//! | — | `random.Random` | [`PyRandom`] |
//! | — | `statistics.median` | [`median`] |
//!
//! # Why this module is transliterated and not re-derived
//!
//! DIV-143 named the standing risk in one line: *a confidence interval that is
//! subtly wrong is worse than one that is absent, because it reads as a verified
//! verdict.* Every number below reaches the wire through a `round(…, 4)` or a
//! `round(…, 6)`, so a one-ULP disagreement four decimals up is invisible right
//! until it is not. Three routines carry an exactness contract a paraphrase
//! loses:
//!
//! 1. **`statistics.NormalDist().inv_cdf` is Wichura's AS241, and the
//!    association order of its central branch is load-bearing.** CPython 3.12
//!    answers it from the C accelerator `_statistics._normal_dist_inv_cdf`.
//!    `inv_cdf(0.95)` is `1.6448536269514715`, one ULP *below* the textbook
//!    `1.6448536269514722`, and the difference is entirely `x = (q * num) / den`
//!    versus `x = q * (num / den)`: the second spelling disagrees with CPython on
//!    26% of the unit interval (58 819 of 200 003 uniform draws). Measured, not
//!    asserted — see [`normal_inv_cdf`].
//! 2. **`statistics.NormalDist().cdf` is `0.5 * (1 + erf(x / sqrt(2)))`, and
//!    `math.erf` is the C library's — which this crate cannot call.** That is
//!    the one place the port does NOT reach CPython's bytes, and it is a
//!    measured narrowing rather than an accepted approximation. See [`erf`] for
//!    the numbers, the blast radius and the two one-line fixes available to
//!    whoever owns `lib.rs`.
//! 3. **The bootstrap is `random.Random(1729)`, which is MT19937 plus CPython's
//!    exact `randrange`.** [`PyRandom`] carries the Mersenne Twister with
//!    CPython's `init_by_array` seeding and its rejection-sampling `_randbelow`,
//!    because [`percentile_bootstrap_ci`] feeds every `median_cost.ci` in the
//!    payload and a different draw sequence is a different interval on every
//!    row. Any other RNG — including a better one — is a guaranteed byte
//!    divergence. The proof is cheap: on the same ten values, seed 1729 gives
//!    `(0.9, 2.7)` and seed 1730 gives `(0.9, 2.9)`.
//!
//! `statistics.pstdev`'s exact-`Fraction` problem (DIV-113) does **not** arise
//! here: `benchmark_stats.py` never calls it, and `statistics.mean` is reachable
//! only through the `statistic="mean"` branch of [`percentile_bootstrap_ci`],
//! which no caller in the tree selects. See that function's docs.
//!
//! # Duplication, flagged rather than fixed
//!
//! [`median`] is a second copy of [`crate::anomaly::median`] (batch C).
//! The batch-E fence forbids editing another member's file, and reaching across
//! it would couple this module to `reports/anomaly.py`'s ownership, so it is
//! transcribed with this note instead. The two are equal by construction and
//! `the_median_matches_the_anomaly_ports_copy` pins that. One line for the
//! integrator's dedup list.

use std::collections::HashMap;
use std::hash::Hash;

// ── pinned tunables ──────────────────────────────────────────────────────────

/// `SEED = 1729`.
pub const SEED: u32 = 1729;

/// `CI_LEVEL = 0.90` — the ratified default confidence level.
pub const CI_LEVEL: f64 = 0.90;

/// `BOOTSTRAP_ITERS = 2000`.
pub const BOOTSTRAP_ITERS: usize = 2000;

/// `MIN_SESSIONS_PER_CELL = 5`.
pub const MIN_SESSIONS_PER_CELL: usize = 5;

/// `MIN_MODELS_PER_CELL = 2`.
pub const MIN_MODELS_PER_CELL: usize = 2;

/// `MIN_BALANCED_TOTAL = 20`.
pub const MIN_BALANCED_TOTAL: i64 = 20;

/// `MIN_EFFECT_COST = 0.10`.
pub const MIN_EFFECT_COST: f64 = 0.10;

/// `MIN_EFFECT_SUCCESS = 0.10`.
pub const MIN_EFFECT_SUCCESS: f64 = 0.10;

/// `MIN_EFFECT_GRADE = 0.5`.
///
/// Exported by `__all__` and read by nothing: `reports/benchmark.py` names
/// `MIN_EFFECT_COST` and `MIN_EFFECT_SUCCESS` and never the grade floor, because
/// the grade axis never became a comparison axis (§4.3's third effect size is
/// unimplemented). Ported so the module's surface matches, not because it runs.
pub const MIN_EFFECT_GRADE: f64 = 0.5;

// ── CPython's `random.Random` ────────────────────────────────────────────────

/// MT19937's state size — `N` in `_randommodule.c`.
const MT_N: usize = 624;
/// `M`.
const MT_M: usize = 397;
/// `MATRIX_A`.
const MT_MATRIX_A: u32 = 0x9908_b0df;
/// `UPPER_MASK`.
const MT_UPPER_MASK: u32 = 0x8000_0000;
/// `LOWER_MASK`.
const MT_LOWER_MASK: u32 = 0x7fff_ffff;

/// `random.Random(seed)` — CPython's Mersenne Twister, seeding included.
///
/// Transcribed from `Modules/_randommodule.c` (`init_genrand`, `init_by_array`,
/// `genrand_uint32`, `_random_Random_getrandbits_impl`) and `Lib/random.py`
/// (`randrange` → `_randbelow_with_getrandbits`).
///
/// # Why the seeding path matters
///
/// `random.Random(1729)` does **not** call `init_genrand(1729)`. `random_seed`
/// takes `abs(arg)`, splits it into little-endian 32-bit words, and calls
/// `init_by_array` — which itself starts from `init_genrand(19650218)` and then
/// stirs the key in. Seeding with `init_genrand(1729)` produces a completely
/// different stream, and every bootstrap CI in the payload would then be wrong
/// in a way that still looks like a plausible interval.
#[derive(Debug, Clone)]
pub struct PyRandom {
    state: [u32; MT_N],
    index: usize,
}

impl PyRandom {
    /// `random.Random(seed)` for a seed that fits one 32-bit word.
    ///
    /// The campaign's only seed is [`SEED`]; a wider one would need more words,
    /// which `random_seed` builds from `abs(n)` little-endian. Restricting the
    /// parameter to `u32` makes the unrepresentable case unrepresentable.
    ///
    /// `keyused` is `1` even for a zero seed (`bits == 0 ? 1 : …`), so the key
    /// is always at least one word and never empty.
    #[must_use]
    pub fn seeded(seed: u32) -> Self {
        let mut rng = Self {
            state: [0; MT_N],
            index: MT_N,
        };
        rng.init_by_array(&[seed]);
        rng
    }

    /// `init_genrand(s)`.
    fn init_genrand(&mut self, s: u32) {
        self.state[0] = s;
        for i in 1..MT_N {
            let prev = self.state[i - 1];
            #[allow(
                clippy::cast_possible_truncation,
                reason = "i < 624, and the C source adds it as a uint32_t"
            )]
            let step = 1_812_433_253_u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
            self.state[i] = step;
        }
        self.index = MT_N;
    }

    /// `init_by_array(init_key, key_length)`.
    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19_650_218);
        let mut i: usize = 1;
        let mut j: usize = 0;
        let mut k = MT_N.max(key.len());
        while k > 0 {
            let prev = self.state[i - 1];
            #[allow(
                clippy::cast_possible_truncation,
                reason = "`(uint32_t)j` in the C source, and j < key.len()"
            )]
            let mixed = (self.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            self.state[i] = mixed;
            i += 1;
            j += 1;
            if i >= MT_N {
                self.state[0] = self.state[MT_N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        let mut k = MT_N - 1;
        while k > 0 {
            let prev = self.state[i - 1];
            #[allow(
                clippy::cast_possible_truncation,
                reason = "`(uint32_t)i` in the C source, and i < 624"
            )]
            let mixed = (self.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            self.state[i] = mixed;
            i += 1;
            if i >= MT_N {
                self.state[0] = self.state[MT_N - 1];
                i = 1;
            }
            k -= 1;
        }
        // "MSB is 1; assuring non-zero initial array."
        self.state[0] = 0x8000_0000;
    }

    /// `genrand_uint32` — one tempered 32-bit draw.
    fn genrand_uint32(&mut self) -> u32 {
        if self.index >= MT_N {
            for kk in 0..(MT_N - MT_M) {
                let y = (self.state[kk] & MT_UPPER_MASK) | (self.state[kk + 1] & MT_LOWER_MASK);
                self.state[kk] = self.state[kk + MT_M] ^ (y >> 1) ^ twist(y);
            }
            for kk in (MT_N - MT_M)..(MT_N - 1) {
                let y = (self.state[kk] & MT_UPPER_MASK) | (self.state[kk + 1] & MT_LOWER_MASK);
                self.state[kk] = self.state[kk + MT_M - MT_N] ^ (y >> 1) ^ twist(y);
            }
            let y = (self.state[MT_N - 1] & MT_UPPER_MASK) | (self.state[0] & MT_LOWER_MASK);
            self.state[MT_N - 1] = self.state[MT_M - 1] ^ (y >> 1) ^ twist(y);
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `getrandbits(k)` for `k <= 64`.
    ///
    /// `k == 0` returns `0` **without a draw** — the C source returns before
    /// touching the generator, so a degenerate call does not advance the stream.
    /// The `k <= 32` fast path and the little-endian word loop agree at
    /// `k == 32`; both are written out because a `k` of 33 would take the loop
    /// and nothing in the caller *guarantees* 32.
    ///
    /// # Panics
    /// `k > 64`, which no call site can produce (`k` is a `bit_length` of a row
    /// count).
    #[must_use]
    pub fn getrandbits(&mut self, k: u32) -> u64 {
        assert!(k <= 64, "getrandbits beyond 64 bits is not reachable here");
        if k == 0 {
            return 0;
        }
        if k <= 32 {
            return u64::from(self.genrand_uint32() >> (32 - k));
        }
        let words = (k - 1) / 32 + 1;
        let mut out: u64 = 0;
        let mut remaining = k;
        for i in 0..words {
            let mut r = self.genrand_uint32();
            if remaining < 32 {
                r >>= 32 - remaining;
            }
            out |= u64::from(r) << (32 * i);
            remaining = remaining.saturating_sub(32);
        }
        out
    }

    /// `random.randrange(n)` for `n > 0` — i.e. `_randbelow_with_getrandbits`.
    ///
    /// Rejection sampling on `n.bit_length()` bits, **not** a modulo: `k` is
    /// `n.bit_length()` and not `(n - 1).bit_length()` (the CPython comment reads
    /// "don't use (n-1) here because n can be 1"), so `randrange(1)` draws one
    /// bit, discards it when it is 1, and returns 0. Those consumed draws are
    /// part of the stream, and a modulo implementation would not consume them.
    ///
    /// `n == 0` returns 0 without a draw, matching 3.12's `if not n` guard —
    /// `randrange(0)` itself raises, but `_randbelow(0)` does not, and this
    /// function is the latter.
    #[must_use]
    pub fn randrange(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let k = usize::BITS - n.leading_zeros();
        loop {
            let r = self.getrandbits(k);
            if let Ok(r) = usize::try_from(r)
                && r < n
            {
                return r;
            }
        }
    }
}

/// `mag01[y & 0x1]` — `0` for an even `y`, `MATRIX_A` for an odd one.
const fn twist(y: u32) -> u32 {
    if y & 1 == 0 { 0 } else { MT_MATRIX_A }
}

// ── the normal distribution ──────────────────────────────────────────────────

// ── `math.erf`, and the one measured gap in this module ─────────────────────
//
// See [`erf`]'s doc comment for the disposition. The code below is the
// FreeBSD/SunPro `fdlibm` routine, transcribed; its notice is preserved as that
// licence requires.
//
// ====================================================
// Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//
// Developed at SunPro, a Sun Microsystems, Inc. business.
// Permission to use, copy, modify, and distribute this
// software is freely granted, provided that this notice
// is preserved.
// ====================================================

/// `erx` — `erf(1)` rounded to 24 bits.
#[allow(
    clippy::excessive_precision,
    reason = "the coefficients are copied digit for digit from `s_erf.c`; trimming them to what f64 can hold would be an edit to a transcription"
)]
const ERX: f64 = 8.450_629_115_104_675_29e-1;
/// `efx8`, for the `|x| < 2^-28` underflow guard.
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const EFX8: f64 = 1.027_033_336_764_100_69e0;
/// `pp0`…`pp4` / `qq1`…`qq5` — the `[0, 0.84375]` rational.
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const PP: [f64; 5] = [
    1.283_791_670_955_125_585_61e-1,
    -3.250_421_072_470_014_993_70e-1,
    -2.848_174_957_559_851_047_66e-2,
    -5.770_270_296_489_441_591_57e-3,
    -2.376_301_665_665_016_260_84e-5,
];
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const QQ: [f64; 5] = [
    3.979_172_239_591_553_528_19e-1,
    6.502_224_998_876_729_444_85e-2,
    5.081_306_281_875_765_627_76e-3,
    1.324_947_380_043_216_445_26e-4,
    -3.960_228_278_775_368_123_20e-6,
];
/// `pa0`…`pa6` / `qa1`…`qa6` — the `[0.84375, 1.25]` rational.
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const PA: [f64; 7] = [
    -2.362_118_560_752_659_440_77e-3,
    4.148_561_186_837_483_316_66e-1,
    -3.722_078_760_357_013_238_47e-1,
    3.183_466_199_011_617_536_74e-1,
    -1.108_946_942_823_966_774_76e-1,
    3.547_830_432_561_823_593_71e-2,
    -2.166_375_594_868_790_843_00e-3,
];
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const QA: [f64; 6] = [
    1.064_208_804_008_442_282_86e-1,
    5.403_979_177_021_710_489_37e-1,
    7.182_865_441_419_626_628_68e-2,
    1.261_712_198_087_616_421_12e-1,
    1.363_708_391_202_905_073_62e-2,
    1.198_449_984_679_910_741_70e-2,
];
/// `ra0`…`ra7` / `sa1`…`sa8` — `erfc` on `[1.25, 1/0.35]`.
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const RA: [f64; 8] = [
    -9.864_944_034_847_148_227_05e-3,
    -6.938_585_727_071_817_643_72e-1,
    -1.055_862_622_532_329_098_14e1,
    -6.237_533_245_032_600_603_96e1,
    -1.623_966_694_625_734_703_55e2,
    -1.846_050_929_067_110_359_94e2,
    -8.128_743_550_630_659_342_46e1,
    -9.814_329_344_169_145_485_92e0,
];
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const SA: [f64; 8] = [
    1.965_127_166_743_925_712_92e1,
    1.376_577_541_435_190_426_00e2,
    4.345_658_774_752_292_288_21e2,
    6.453_872_717_332_678_803_36e2,
    4.290_081_400_275_678_333_86e2,
    1.086_350_055_417_794_351_34e2,
    6.570_249_770_319_281_701_35e0,
    -6.042_441_521_485_809_874_38e-2,
];
/// `rb0`…`rb6` / `sb1`…`sb7` — `erfc` on `[1/0.35, 28]`.
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const RB: [f64; 7] = [
    -9.864_942_924_700_099_285_97e-3,
    -7.992_832_376_805_230_065_74e-1,
    -1.775_795_491_775_475_198_89e1,
    -1.606_363_848_558_219_160_62e2,
    -6.375_664_433_683_896_277_22e2,
    -1.025_095_131_611_077_249_54e3,
    -4.835_191_916_086_513_970_19e2,
];
#[allow(clippy::excessive_precision, reason = "see `ERX`")]
const SB: [f64; 7] = [
    3.033_806_074_348_245_829_24e1,
    3.257_925_129_965_739_188_26e2,
    1.536_729_586_084_436_959_94e3,
    3.199_858_219_508_595_539_08e3,
    2.553_050_406_433_164_425_83e3,
    4.745_285_412_069_553_672_15e2,
    -2.244_095_244_658_581_833_62e1,
];

/// `erfc1` — the `[0.84375, 1.25]` branch.
fn erfc1(x: f64) -> f64 {
    let s = x.abs() - 1.0;
    let p = PA[0] + s * (PA[1] + s * (PA[2] + s * (PA[3] + s * (PA[4] + s * (PA[5] + s * PA[6])))));
    let q = 1.0 + s * (QA[0] + s * (QA[1] + s * (QA[2] + s * (QA[3] + s * (QA[4] + s * QA[5])))));
    1.0 - ERX - p / q
}

/// `erfc2` — the two asymptotic branches above 1.25.
fn erfc2(ix: u32, x: f64) -> f64 {
    if ix < 0x3ff4_0000 {
        return erfc1(x);
    }
    let x = x.abs();
    let s = 1.0 / (x * x);
    let (r, big_s) = if ix < 0x4006_db6d {
        (
            RA[0]
                + s * (RA[1]
                    + s * (RA[2]
                        + s * (RA[3] + s * (RA[4] + s * (RA[5] + s * (RA[6] + s * RA[7])))))),
            1.0 + s
                * (SA[0]
                    + s * (SA[1]
                        + s * (SA[2]
                            + s * (SA[3] + s * (SA[4] + s * (SA[5] + s * (SA[6] + s * SA[7]))))))),
        )
    } else {
        (
            RB[0] + s * (RB[1] + s * (RB[2] + s * (RB[3] + s * (RB[4] + s * (RB[5] + s * RB[6]))))),
            1.0 + s
                * (SB[0]
                    + s * (SB[1]
                        + s * (SB[2] + s * (SB[3] + s * (SB[4] + s * (SB[5] + s * SB[6])))))),
        )
    };
    // `with_set_low_word(x, 0)` — the top 32 bits of `x`, low half zeroed, so
    // `z * z` is exact and the cancellation in `(z - x) * (z + x)` is not.
    let z = f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000);
    (-z * z - 0.5625).exp() * ((z - x) * (z + x) + r / big_s).exp() / x
}

/// `math.erf(x)` — fdlibm's `s_erf.c`, transcribed. **A measured narrowing.**
///
/// # Why this is not CPython's bytes, and what that costs
///
/// `math.erf` is `return erf(x)` on every build defining `HAVE_ERF` — which is
/// every glibc build — so CPython answers from the platform. Calling that same
/// symbol needs an `extern "C"` block, and `lib.rs` carries
/// `#![forbid(unsafe_code)]`, which a submodule cannot relax and this batch's
/// fence forbids editing. Adding a math dependency needs a `Cargo.toml` the
/// fence also forbids. So the routine is written out.
///
/// The natural candidate is the source glibc's `s_erf.c` derives from — SunPro
/// fdlibm — and it is **not** bit-identical to glibc 2.31 on this machine.
/// Measured over 220 042 points spanning every branch:
///
/// | | |
/// |---|---|
/// | inputs where the two differ | 5 546 (2.520%) |
/// | largest disagreement | exactly **1 ULP**, never more |
/// | worst input seen | `x = 1.1931399268791605` |
///
/// The difference is in the polynomial branches, not in the `exp` call —
/// `erf(0.25)` already disagrees, and that branch never touches `exp`. Two
/// other candidates were tried and are further away, not closer: CPython's own
/// `m_erf_series` (which `mathmodule.c` compiles out when `HAVE_ERF`) misses on
/// 11.8% of inputs, and an FMA-contracted Horner on 8%.
///
/// # Blast radius
///
/// [`normal_cdf`] is called from exactly one place —
/// `services::benchmark::two_proportion_pvalue` — whose result is consumed by
/// exactly one more: [`benjamini_hochberg`]. No p-value reaches the payload.
/// So a 1-ULP move can only change a response by flipping a
/// `statistically_separated` boolean, and only when a p-value sits within ~2
/// ULP of its exact `(k/m)·α` threshold. On the harness store it cannot happen
/// at all: every measured success rate is `0.0`, so `var <= 0` short-circuits
/// the p-value to `1.0` before `cdf` is reached, and no case row exercises this
/// function.
///
/// # The fixes, for whoever owns `lib.rs`
///
/// Either downgrade `forbid(unsafe_code)` to `deny` so a single
/// `unsafe extern "C" { fn erf(x: f64) -> f64; }` can be `#[allow]`ed here, or
/// add a math crate to the workspace. Both are one line and neither is this
/// batch's to make.
#[must_use]
pub fn erf(x: f64) -> f64 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "`get_high_word` — the top 32 bits, by construction"
    )]
    let mut ix = (x.to_bits() >> 32) as u32;
    let sign = ix >> 31;
    ix &= 0x7fff_ffff;
    if ix >= 0x7ff0_0000 {
        // erf(nan) = nan, erf(±inf) = ±1.
        return 1.0 - 2.0 * f64::from(sign) + 1.0 / x;
    }
    if ix < 0x3feb_0000 {
        // |x| < 0.84375.
        if ix < 0x3e30_0000 {
            // |x| < 2^-28 — the underflow guard.
            return 0.125 * (8.0 * x + EFX8 * x);
        }
        let z = x * x;
        let r = PP[0] + z * (PP[1] + z * (PP[2] + z * (PP[3] + z * PP[4])));
        let s = 1.0 + z * (QQ[0] + z * (QQ[1] + z * (QQ[2] + z * (QQ[3] + z * QQ[4]))));
        let y = r / s;
        return x + x * y;
    }
    let y = if ix < 0x4018_0000 {
        // 0.84375 <= |x| < 6.
        1.0 - erfc2(ix, x)
    } else {
        // Beyond 6, erf is 1 to the last bit — `1 - 2^-1022` raises inexact
        // exactly as fdlibm intends and rounds to 1.0.
        1.0 - f64::from_bits(0x0010_0000_0000_0000)
    };
    if sign != 0 { -y } else { y }
}

/// `_SQRT2` — `sqrt(2.0)`, the constant `NormalDist.cdf` divides by.
const SQRT2: f64 = std::f64::consts::SQRT_2;

/// `statistics.NormalDist().cdf(x)` — the standard normal, `mu=0`, `sigma=1`.
///
/// `0.5 * (1.0 + erf((x - mu) / (sigma * _SQRT2)))`, with `mu` and `sigma`
/// folded in: `(x - 0.0) / (1.0 * SQRT2)` is `x / SQRT2` bit for bit, because
/// both `- 0.0` and `* 1.0` are exact on every finite double.
#[must_use]
pub fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / SQRT2))
}

/// `statistics.NormalDist().inv_cdf(p)` — Wichura's AS241, CPython's spelling.
///
/// Transcribed from `Modules/_statisticsmodule.c::normal_dist_inv_cdf` with
/// `mu = 0.0` and `sigma = 1.0`. Three details are not cosmetic:
///
/// * the central branch is `x = (q * num) / den`, left-associated — see the
///   module docs for the measurement;
/// * `mu + (x * sigma)` with `mu = 0.0`, `sigma = 1.0` is exactly `x`, so the
///   result is returned unwrapped;
/// * the `p <= 0 || p >= 1` guard raises `StatisticsError` in Python. The only
///   caller here is [`z_for_confidence`], which clamps `ci` into
///   `[0.5, 0.999999]` before the arithmetic can reach an edge, so the branch is
///   unreachable and answers `f64::NAN` rather than growing an error type no
///   caller would read.
#[must_use]
#[allow(
    clippy::excessive_precision,
    reason = "the constants are copied digit for digit from the C source"
)]
pub fn normal_inv_cdf(p: f64) -> f64 {
    if p <= 0.0 || p >= 1.0 {
        return f64::NAN;
    }
    let q = p - 0.5;
    if q.abs() <= 0.425 {
        let r = 0.180_625 - q * q;
        let num = ((((((2.5090809287301226727e+3 * r + 3.3430575583588128105e+4) * r
            + 6.7265770927008700853e+4)
            * r
            + 4.5921953931549871457e+4)
            * r
            + 1.3731693765509461125e+4)
            * r
            + 1.9715909503065514427e+3)
            * r
            + 1.3314166789178437745e+2)
            * r
            + 3.3871328727963666080e0;
        let den = ((((((5.2264952788528545610e+3 * r + 2.8729085735721942674e+4) * r
            + 3.9307895800092710610e+4)
            * r
            + 2.1213794301586595867e+4)
            * r
            + 5.3941960214247511077e+3)
            * r
            + 6.8718700749205790830e+2)
            * r
            + 4.2313330701600911252e+1)
            * r
            + 1.0;
        return (q * num) / den;
    }
    let mut r = if q > 0.0 { 1.0 - p } else { p };
    if !(r > 0.0 && r < 1.0) {
        return f64::NAN;
    }
    r = (-r.ln()).sqrt();
    let x = if r <= 5.0 {
        r -= 1.6;
        let num = ((((((7.74545014278341407640e-4 * r + 2.27238449892691845833e-2) * r
            + 2.41780725177450611770e-1)
            * r
            + 1.27045825245236838258e0)
            * r
            + 3.64784832476320460504e0)
            * r
            + 5.76949722146069140550e0)
            * r
            + 4.63033784615654529590e0)
            * r
            + 1.42343711074968357734e0;
        let den = ((((((1.05075007164441684324e-9 * r + 5.47593808499534494600e-4) * r
            + 1.51986665636164571966e-2)
            * r
            + 1.48103976427480074590e-1)
            * r
            + 6.89767334985100004550e-1)
            * r
            + 1.67638483018380384940e0)
            * r
            + 2.05319162663775882187e0)
            * r
            + 1.0;
        num / den
    } else {
        r -= 5.0;
        let num = ((((((2.01033439929228813265e-7 * r + 2.71155556874348757815e-5) * r
            + 1.24266094738807843860e-3)
            * r
            + 2.65321895265761230930e-2)
            * r
            + 2.96560571828504891230e-1)
            * r
            + 1.78482653991729133580e0)
            * r
            + 5.46378491116411436990e0)
            * r
            + 6.65790464350110377720e0;
        let den = ((((((2.04426310338993978564e-15 * r + 1.42151175831644588870e-7) * r
            + 1.84631831751005468180e-5)
            * r
            + 7.86869131145613259100e-4)
            * r
            + 1.48753612908506148525e-2)
            * r
            + 1.36929880922735805310e-1)
            * r
            + 5.99832206555887937690e-1)
            * r
            + 1.0;
        num / den
    };
    if q < 0.0 { -x } else { x }
}

/// `z_for_confidence(ci_level)`.
///
/// `min(max(float(ci_level), 0.5), 0.999999)` then `inv_cdf(1 - (1 - ci) / 2)`.
/// The clamp is why [`normal_inv_cdf`]'s error branches are dead: the argument
/// lands in `[0.75, 0.9999995]`, where `sqrt(-log(1 - p))` is at most 3.8, so
/// even AS241's `r > 5.0` tail is unreachable from this entry point.
#[must_use]
pub fn z_for_confidence(ci_level: f64) -> f64 {
    // `min(max(x, 0.5), 0.999999)` in Python's order. NOT `clamp`: `clamp`
    // panics when the bounds are misordered and propagates NaN, while Python's
    // `max`-then-`min` returns the second operand on a NaN comparison. No
    // caller can supply either, but the shape stays the transcription's.
    #[allow(clippy::manual_clamp, reason = "see above")]
    let ci = ci_level.max(0.5).min(0.999_999);
    normal_inv_cdf(1.0 - (1.0 - ci) / 2.0)
}

// ── Wilson score interval ────────────────────────────────────────────────────

/// `wilson_interval(successes, n, ci_level=…)`.
///
/// `n <= 0` is the widest possible interval, not an error. Transcribed term for
/// term, including `(…) ** 0.5` as `.sqrt()` — CPython's `float.__pow__(0.5)` is
/// `pow(x, 0.5)`, which IEEE-754 requires to equal `sqrt(x)` for a finite
/// non-negative base.
#[must_use]
pub fn wilson_interval(successes: i64, n: i64, ci_level: f64) -> (f64, f64) {
    if n <= 0 {
        return (0.0, 1.0);
    }
    let z = z_for_confidence(ci_level);
    #[allow(
        clippy::cast_precision_loss,
        reason = "both are session counts, far below 2^53"
    )]
    let (successes, n) = (successes as f64, n as f64);
    let p = successes / n;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = (z / denom) * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    let lo = 0.0_f64.max(center - margin);
    let hi = 1.0_f64.min(center + margin);
    (lo, hi)
}

// ── percentile + bootstrap ───────────────────────────────────────────────────

/// `percentile(sorted_values, q)` — linear interpolation, numpy's `'linear'`.
///
/// The caller sorts; this does not. `int(rank)` truncates toward zero, which for
/// a non-negative `rank` is `floor`.
#[must_use]
pub fn percentile(sorted_values: &[f64], q: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    if sorted_values.len() == 1 {
        return sorted_values[0];
    }
    // `min(max(float(q), 0.0), 1.0)` — same reasoning as `z_for_confidence`.
    #[allow(clippy::manual_clamp, reason = "Python's max-then-min, not clamp")]
    let q = q.max(0.0).min(1.0);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a resample count of 2000, or a row count"
    )]
    let rank = q * (sorted_values.len() - 1) as f64;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`int(rank)` on a value clamped to [0, len-1]"
    )]
    let lo_idx = rank as usize;
    #[allow(
        clippy::cast_precision_loss,
        reason = "lo_idx <= rank, and rank < 2^53"
    )]
    let frac = rank - lo_idx as f64;
    if lo_idx + 1 >= sorted_values.len() {
        return sorted_values[sorted_values.len() - 1];
    }
    let lo = sorted_values[lo_idx];
    let hi = sorted_values[lo_idx + 1];
    lo + (hi - lo) * frac
}

/// `statistics.median` — sort, then the middle or the mean of the two middles.
///
/// A transcription of [`crate::anomaly::median`]; see the module docs
/// for why it is copied rather than imported. Python raises `StatisticsError` on
/// an empty sequence and both ports answer `0.0`; every call site here guards
/// first, so that branch is defence in depth.
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// `percentile_bootstrap_ci(values, statistic="median", iters, ci_level, seed)`.
///
/// # The `statistic` parameter is not ported, deliberately
///
/// Python selects `statistics.mean` when `statistic == "mean"` and
/// `statistics.median` otherwise. `statistics.mean` is **not** `fsum/n`: it
/// accumulates through `Fraction` and converts once, so it is the correctly
/// rounded exact mean and a two-pass `f64` would be a narrowing, not a port (the
/// same shape as DIV-113). No caller in the tree passes `"mean"` —
/// `reports/benchmark.py:765` is the only call site and it passes the literal
/// `"median"` — so porting the branch would mean shipping an unmeasured
/// exact-rational accumulator to satisfy dead code. Recorded as a finding
/// instead; this function is median-only.
///
/// Empty → `(0.0, 0.0)`; one value → `(v, v)`, neither touching the RNG.
#[must_use]
pub fn percentile_bootstrap_ci(
    values: &[f64],
    iters: usize,
    ci_level: f64,
    seed: u32,
) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    if values.len() == 1 {
        return (values[0], values[0]);
    }
    let mut rng = PyRandom::seeded(seed);
    let n = values.len();
    let iters = iters.max(1);
    let mut draws: Vec<f64> = Vec::with_capacity(iters);
    let mut sample: Vec<f64> = vec![0.0; n];
    for _ in 0..iters {
        // `[clean[rng.randrange(n)] for _ in range(n)]` — the draws are taken
        // left to right, so the generator advances exactly n times per
        // iteration (plus whatever `randrange` rejects).
        for slot in &mut sample {
            *slot = values[rng.randrange(n)];
        }
        draws.push(median(&sample));
    }
    draws.sort_by(f64::total_cmp);
    let alpha = (1.0 - ci_level) / 2.0;
    (percentile(&draws, alpha), percentile(&draws, 1.0 - alpha))
}

// ── Benjamini–Hochberg ───────────────────────────────────────────────────────

/// `benjamini_hochberg(pvalues, alpha=…)` — the step-up, aligned to input order.
///
/// `sorted(range(m), key=lambda i: pvalues[i])` is a **stable** sort on the
/// p-value alone, so equal p-values keep ascending index order. That ordering
/// decides which rank each hypothesis is tested at; `sort_by` here is stable for
/// the same reason.
///
/// The loop keeps the **largest** rank that clears its threshold and then
/// rejects everything up to it — a hypothesis can be rejected while failing its
/// own test. That is the step-up, and a per-rank filter is a different, strictly
/// more conservative procedure.
#[must_use]
pub fn benjamini_hochberg(pvalues: &[f64], alpha: f64) -> Vec<bool> {
    let m = pvalues.len();
    if m == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|a, b| pvalues[*a].total_cmp(&pvalues[*b]));
    #[allow(
        clippy::cast_precision_loss,
        reason = "the hypothesis family is a handful of model pairs"
    )]
    let m_f = m as f64;
    let mut max_k = 0_usize;
    for (rank, idx) in order.iter().enumerate() {
        #[allow(clippy::cast_precision_loss, reason = "see above")]
        let rank_f = (rank + 1) as f64;
        if pvalues[*idx] <= (rank_f / m_f) * alpha {
            max_k = rank + 1;
        }
    }
    let mut reject = vec![false; m];
    for (rank, idx) in order.iter().enumerate() {
        // `if rank <= max_k` with Python's 1-based `enumerate(order, start=1)`.
        if rank < max_k {
            reject[*idx] = true;
        }
    }
    reject
}

// ── standardization vs pooling ───────────────────────────────────────────────

/// `pooled_rate(cells)` — `Σ(n·rate) / Σn`, the *confounded* number.
///
/// Python iterates `cells.values()`, so the keys never enter the arithmetic and
/// the parameter here is the value list. `num` is a `+=` chain over `n * rate`
/// and `den` an integer `+=` chain: neither is `sum()`, so neither is
/// Neumaier-compensated.
///
/// **Unreached.** `reports/benchmark.py` names ten members of this module and
/// this is not one of them. Ported because the module is the port's unit, and
/// pinned by test so a future caller inherits measured behaviour rather than a
/// fresh guess.
#[must_use]
pub fn pooled_rate(cells: &[(i64, f64)]) -> f64 {
    let mut num = 0.0_f64;
    let mut den = 0_i64;
    for (n, rate) in cells {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a session count multiplied by a rate, as Python does"
        )]
        let term = *n as f64 * rate;
        num += term;
        den += n;
    }
    #[allow(clippy::cast_precision_loss, reason = "see above")]
    if den == 0 { 0.0 } else { num / den as f64 }
}

/// `standardized_rate(cells, weights)` — per-stratum rates under common weights.
///
/// Iterates `weights`, not `cells`, and skips a stratum absent from `cells` or
/// carrying a non-positive weight. **Unreached**, like [`pooled_rate`].
#[must_use]
pub fn standardized_rate<K: Eq + Hash>(
    cells: &HashMap<K, (i64, f64)>,
    weights: &[(K, f64)],
) -> f64 {
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (stratum, w) in weights {
        if let Some((_, rate)) = cells.get(stratum)
            && *w > 0.0
        {
            num += w * rate;
            den += w;
        }
    }
    if den == 0.0 { 0.0 } else { num / den }
}

/// `standardized_difference(a_cells, b_cells)` — the Simpson's-paradox defence.
///
/// The shared strata are `set(a_cells) & set(b_cells)`, whose *iteration order*
/// Python does not fix — and the resulting weight dict is consumed by
/// [`standardized_rate`], whose accumulation is a `+=` chain, so the order is
/// observable in the last ULP. Reproducing it would mean reproducing CPython's
/// set layout, which for `str`-bearing tuple keys is randomised per process
/// (`PYTHONHASHSEED`): the Python side is not self-consistent across runs
/// either.
///
/// **Unreached**, so this is a hazard recorded rather than a divergence
/// incurred. The port sorts the shared strata for determinism, and if a caller
/// ever appears the ledger already names the difference.
#[must_use]
pub fn standardized_difference<K: Eq + Hash + Ord + Clone>(
    a_cells: &HashMap<K, (i64, f64)>,
    b_cells: &HashMap<K, (i64, f64)>,
) -> f64 {
    let mut shared: Vec<K> = a_cells
        .keys()
        .filter(|k| b_cells.contains_key(*k))
        .cloned()
        .collect();
    shared.sort();
    let weights: Vec<(K, f64)> = shared
        .into_iter()
        .map(|k| {
            #[allow(
                clippy::cast_precision_loss,
                reason = "`float(a + b)` over two session counts"
            )]
            let w = (a_cells[&k].0 + b_cells[&k].0) as f64;
            (k, w)
        })
        .collect();
    if weights.is_empty() {
        return 0.0;
    }
    standardized_rate(a_cells, &weights) - standardized_rate(b_cells, &weights)
}

// ── effect sizes ─────────────────────────────────────────────────────────────

/// `relative_delta(new, base)` — `(base - new) / base`, positive ⇒ `new` lower.
///
/// `if base == 0` is an equality on a float, so `-0.0` takes the guard too
/// (`-0.0 == 0` is true in Python and in Rust).
#[must_use]
pub fn relative_delta(new: f64, base: f64) -> f64 {
    if base == 0.0 {
        return 0.0;
    }
    (base - new) / base
}

/// `risk_difference(p_a, p_b)`.
#[must_use]
pub fn risk_difference(p_a: f64, p_b: f64) -> f64 {
    p_a - p_b
}

// ── confidence label ─────────────────────────────────────────────────────────

/// `confidence_bucket(score)` — `{none, low, medium, high}`.
#[must_use]
pub fn confidence_bucket(score: f64) -> &'static str {
    if score >= 0.66 {
        "high"
    } else if score >= 0.40 {
        "medium"
    } else if score >= 0.15 {
        "low"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expected value below was produced by running the *Python* routine
    /// in CPython 3.12.13 against `python-legacy: services/benchmark_stats.py`,
    /// and is pinned as the exact `f64` bit pattern — a decimal literal can hide
    /// an ULP and this whole module exists because ULPs matter here.
    fn bits(x: f64) -> String {
        format!("{:016x}", x.to_bits())
    }

    // ── the RNG ──────────────────────────────────────────────────────────────

    #[test]
    fn the_seeded_stream_is_cpythons_not_a_fresh_mersenne_twister() {
        // `random.Random(1729); [r.getrandbits(32) for _ in range(8)]`
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<u64> = (0..8).map(|_| rng.getrandbits(32)).collect();
        assert_eq!(
            got,
            vec![
                4_279_386_776,
                2_807_804_504,
                3_800_273_382,
                193_155_062,
                1_764_365_577,
                2_153_083_049,
                1_965_890_240,
                728_673_765,
            ]
        );
    }

    #[test]
    fn randrange_rejects_rather_than_taking_a_modulo() {
        // `random.Random(1729); [r.randrange(5) for _ in range(20)]`
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<usize> = (0..20).map(|_| rng.randrange(5)).collect();
        assert_eq!(
            got,
            vec![0, 3, 4, 3, 1, 0, 1, 2, 4, 2, 2, 1, 1, 4, 3, 0, 4, 3, 2, 3]
        );

        // 33 is the biggest single cell on the harness store.
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<usize> = (0..12).map(|_| rng.randrange(33)).collect();
        assert_eq!(got, vec![2, 26, 32, 29, 10, 3, 11, 18, 23, 18, 13, 9]);

        // A 10-bit k, so the rejection loop runs more than once.
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<usize> = (0..6).map(|_| rng.randrange(1000)).collect();
        assert_eq!(got, vec![669, 906, 46, 420, 513, 468]);
    }

    #[test]
    fn randrange_one_still_consumes_a_draw() {
        // `k = n.bit_length()` is 1 when n == 1, so the generator IS advanced.
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<usize> = (0..4).map(|_| rng.randrange(1)).collect();
        assert_eq!(got, vec![0, 0, 0, 0]);

        let mut untouched = PyRandom::seeded(SEED);
        let mut consumed = PyRandom::seeded(SEED);
        for _ in 0..4 {
            let _ = consumed.randrange(1);
        }
        assert_ne!(untouched.getrandbits(32), consumed.getrandbits(32));

        // `random.Random(1729); [r.randrange(2) for _ in range(16)]`
        let mut rng = PyRandom::seeded(SEED);
        let got: Vec<usize> = (0..16).map(|_| rng.randrange(2)).collect();
        assert_eq!(got, vec![0, 1, 1, 0, 0, 0, 1, 1, 1, 0, 0, 1, 0, 1, 1, 1]);
    }

    // ── the normal distribution ──────────────────────────────────────────────

    #[test]
    fn inv_cdf_is_as241_to_the_last_bit() {
        // `statistics.NormalDist().inv_cdf(p)`, as hex bits.
        for (p, expected) in [
            (0.95, "3ffa515209676ab8"),
            (0.975, "3fff5c0331eeff82"),
            (0.5, "0000000000000000"),
            (0.9995, "400a52ffadd2f906"),
            (0.75, "3fe5956b87528a49"),
            (0.5000005, "3eb506f17795452f"),
        ] {
            assert_eq!(bits(normal_inv_cdf(p)), expected, "inv_cdf({p})");
        }
        // The textbook value is 1.6448536269514722; CPython answers one ULP
        // below it, and so must this.
        assert!((normal_inv_cdf(0.95) - 1.644_853_626_951_472_2).abs() > 0.0);
        // The unreachable guard, ported as a total function.
        assert!(normal_inv_cdf(0.0).is_nan());
        assert!(normal_inv_cdf(1.0).is_nan());
    }

    #[test]
    fn z_for_confidence_clamps_at_both_ends() {
        // `bs.z_for_confidence(x)` for 0.90 / 0.95 / 0.0 / 1.0.
        assert_eq!(bits(z_for_confidence(0.90)), "3ffa515209676ab8");
        assert_eq!(bits(z_for_confidence(CI_LEVEL)), "3ffa515209676ab8");
        assert_eq!(bits(z_for_confidence(0.95)), "3fff5c0331eeff82");
        // 0.0 clamps UP to 0.5 → inv_cdf(0.75) = 0.6744897501960817.
        assert_eq!(bits(z_for_confidence(0.0)), "3fe5956b87528a49");
        // 1.0 clamps DOWN to 0.999999 → inv_cdf(0.9999995) = 4.891638475671084.
        assert_eq!(bits(z_for_confidence(1.0)), "40139109ad33734d");
    }

    #[test]
    fn cdf_and_erf_are_fdlibms_across_every_branch() {
        // `statistics.NormalDist().cdf(x)`.
        for (x, expected) in [
            (0.0, "3fe0000000000000"),
            (1.0, "3feaec4bd120d37d"),
            (1.644_853_626_951_472_2, "3fee666666666666"),
            (2.5, "3fefcd21635036c6"),
            (0.333_333_333_333_333_3, "3fe42d895ac42011"),
            (7.5, "3feffffffffffee0"),
        ] {
            assert_eq!(bits(normal_cdf(x)), expected, "cdf({x})");
        }

        // `math.erf` itself, across every branch of fdlibm's ladder. These
        // agree with CPython bit for bit.
        for (x, expected) in [
            (0.0, "0000000000000000"),
            (1e-20, "3bcaa4a230244ae0"),
            (0.5, "3fe0a7ef5c18edd2"),
            (0.9, "3fe98045a6c8a2e6"),
            (1.0, "3feaf767a741088b"),
            (2.0, "3fefd9ae142795e3"),
            (2.5, "3feffcaa8f4c9bea"),
            (5.0, "3fefffffffffc9e8"),
            (7.0, "3ff0000000000000"),
            (-1.0, "bfeaf767a741088b"),
        ] {
            assert_eq!(bits(erf(x)), expected, "erf({x})");
        }
    }

    /// The gap [`erf`] documents, pinned so it cannot widen silently and so that
    /// closing it is a visible one-line change to this test.
    #[test]
    fn erf_diverges_from_cpython_by_exactly_one_ulp_and_no_more() {
        // `math.erf(0.25)` is 0x3fd1af54e232d608 in CPython 3.12.13 on glibc
        // 2.31; fdlibm answers the neighbouring double, and never more than
        // that (220 042 points measured, max |ULP| = 1).
        assert_eq!(erf(0.25).to_bits(), 0x3fd1_af54_e232_d608_u64 + 1);
        // …and `cdf`, which is what the engine actually calls, still lands on
        // CPython's byte here: the `1.0 +` lifts the exponent by two, so a
        // last-place move in `erf` falls below the sum's resolution.
        // `statistics.NormalDist().cdf(0.25 * sqrt(2))` = 0.6381631950841185.
        assert_eq!(bits(normal_cdf(0.25 * SQRT2)), "3fe46bd5388cb582");
    }

    // ── Wilson ───────────────────────────────────────────────────────────────

    #[test]
    fn wilson_matches_the_intervals_the_live_store_publishes() {
        // The unrounded `ci_wilson` values `/api/benchmark` answers today.
        for (s, n, lo_bits, hi_bits) in [
            (0_i64, 11_i64, "0000000000000000", "3fc944918f26bd98"),
            (0, 23, "0000000000000000", "3fbaf1c0d4bf5b6b"),
            (0, 32, "0000000000000000", "3fb3f4ff12a2e811"),
            (0, 27, "0000000000000000", "3fb750efaf886369"),
            (0, 5, "0000000000000000", "3fd678b157f7cff6"),
            (0, 1, "0000000000000000", "3fe75d4215f56427"),
        ] {
            let (lo, hi) = wilson_interval(s, n, CI_LEVEL);
            assert_eq!(bits(lo), lo_bits, "wilson({s}, {n}).lo");
            assert_eq!(bits(hi), hi_bits, "wilson({s}, {n}).hi");
        }
    }

    #[test]
    fn wilson_is_total_at_the_edges_and_is_not_symmetric() {
        assert_eq!(wilson_interval(0, 0, CI_LEVEL), (0.0, 1.0));
        assert_eq!(wilson_interval(3, -1, CI_LEVEL), (0.0, 1.0));
        // 3 of 4 — the docstring's own example: honest uncertainty, not 0.75±.
        let (lo, hi) = wilson_interval(3, 4, CI_LEVEL);
        assert_eq!(bits(lo), "3fd6cb74e733f856"); // 0.3561680085985065
        assert_eq!(bits(hi), "3fee259f8ba7eca9"); // 0.9420926788001412
        // n == successes: the upper edge is 1.0 only after the clamp.
        let (lo, hi) = wilson_interval(5, 5, CI_LEVEL);
        assert_eq!(bits(lo), "3fe4c3a754041805"); // 0.648883499234899
        assert_eq!(hi, 1.0);
    }

    // ── percentile + bootstrap ───────────────────────────────────────────────

    #[test]
    fn percentile_interpolates_linearly_and_clamps_q() {
        let data = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&data, 0.0), 1.0);
        assert_eq!(percentile(&data, 1.0), 4.0);
        assert_eq!(percentile(&data, 0.5), 2.5);
        // q outside [0, 1] is clamped, not an error.
        assert_eq!(percentile(&data, -3.0), 1.0);
        assert_eq!(percentile(&data, 7.0), 4.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[9.5], 0.5), 9.5);
        // `bs.percentile([1.0, 2.0, 3.0, 4.0], (1.0 - 0.90) / 2.0)` = 1.15 —
        // note that alpha is 0.049999999999999996, not 0.05.
        assert_eq!(
            bits(percentile(&data, (1.0 - CI_LEVEL) / 2.0)),
            "3ff2666666666666"
        );
    }

    #[test]
    fn the_bootstrap_ci_is_reproducible_and_pinned_to_cpythons_draws() {
        // `bs.percentile_bootstrap_ci([0.1, 0.5, …, 3.7])` = (0.9, 2.7).
        let spread = [0.1, 0.5, 0.9, 1.3, 1.7, 2.1, 2.5, 2.9, 3.3, 3.7];
        let (lo, hi) = percentile_bootstrap_ci(&spread, BOOTSTRAP_ITERS, CI_LEVEL, SEED);
        assert_eq!(bits(lo), "3feccccccccccccd"); // 0.9
        assert_eq!(bits(hi), "400599999999999a"); // 2.7

        // The seed is not decoration: 1730 answers (0.9, 2.9) in CPython, and
        // any other generator would land somewhere else again.
        let (_, hi_other) = percentile_bootstrap_ci(&spread, BOOTSTRAP_ITERS, CI_LEVEL, 1730);
        assert_eq!(bits(hi_other), "4007333333333333"); // 2.9

        // Two runs of the same seed agree — what the read-through cache and the
        // differ both rely on.
        assert_eq!(
            percentile_bootstrap_ci(&spread, BOOTSTRAP_ITERS, CI_LEVEL, SEED),
            (lo, hi)
        );

        // `bs.percentile_bootstrap_ci([1.0 … 7.0])` = (2.0, 6.0).
        let (lo, hi) = percentile_bootstrap_ci(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
            BOOTSTRAP_ITERS,
            CI_LEVEL,
            SEED,
        );
        assert_eq!((lo, hi), (2.0, 6.0));

        // Degenerate inputs never touch the generator.
        assert_eq!(
            percentile_bootstrap_ci(&[], BOOTSTRAP_ITERS, CI_LEVEL, SEED),
            (0.0, 0.0)
        );
        assert_eq!(
            percentile_bootstrap_ci(&[4.25], BOOTSTRAP_ITERS, CI_LEVEL, SEED),
            (4.25, 4.25)
        );
        // `iters=0` still runs once — `range(max(1, iters))`.
        let single = percentile_bootstrap_ci(&[1.0, 2.0], 0, CI_LEVEL, SEED);
        assert_eq!(single.0, single.1);
    }

    #[test]
    fn the_median_matches_the_anomaly_ports_copy() {
        // The duplication this module flags, pinned so collapsing it later is a
        // provable refactor.
        for values in [
            vec![1.0],
            vec![1.0, 2.0],
            vec![3.0, 1.0, 2.0],
            vec![4.0, 1.0, 3.0, 2.0],
            vec![0.0, 0.0, 0.0],
            vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
        ] {
            assert_eq!(median(&values), crate::anomaly::median(&values));
        }
        assert_eq!(median(&[]), 0.0);
    }

    // ── Benjamini–Hochberg ───────────────────────────────────────────────────

    #[test]
    fn bh_matches_cpython_on_the_families_the_engine_builds() {
        // `bs.benjamini_hochberg(p, alpha=0.10)` for each.
        assert_eq!(
            benjamini_hochberg(&[0.01, 0.04, 0.03, 0.9], 0.10),
            vec![true, true, true, false]
        );
        assert_eq!(
            benjamini_hochberg(&[0.001, 0.09, 0.03], 0.10),
            vec![true, true, true]
        );
        // The family the engine actually builds on this store: every p is 1.0
        // (the two-proportion test short-circuits at zero variance) and alpha is
        // `1 - ci_level` = 0.09999999999999998. Nothing is rejected, which is
        // why every `statistically_separated` in the payload is false.
        assert_eq!(
            benjamini_hochberg(&[1.0, 1.0, 1.0], 1.0 - CI_LEVEL),
            vec![false, false, false]
        );
        assert_eq!(benjamini_hochberg(&[], 0.10), Vec::<bool>::new());
    }

    #[test]
    fn bh_rescues_a_hypothesis_that_fails_its_own_threshold() {
        // m = 4, alpha = 0.10, thresholds 0.025 / 0.05 / 0.075 / 0.10.
        //   sorted p = [0.001, 0.06, 0.07, 0.10]
        //   rank 2: 0.06 > 0.05 ✗ … rank 4: 0.10 <= 0.10 ✓ → max_k = 4
        // so the rank-2 hypothesis IS rejected despite failing its own line.
        // A per-rank filter would answer [false, true, false, false].
        assert_eq!(
            benjamini_hochberg(&[0.06, 0.001, 0.10, 0.07], 0.10),
            vec![true, true, true, true]
        );
    }

    // ── standardization + effects ────────────────────────────────────────────

    #[test]
    fn pooled_and_standardized_rates_are_the_confounded_and_honest_numbers() {
        // A drew the easy stratum, B the hard one — the Simpson's setup.
        //   A: easy (n=90, rate=0.90), hard (n=10, rate=0.30)
        //   B: easy (n=10, rate=0.95), hard (n=90, rate=0.40)
        // Pooling says A wins 0.84 to 0.455 …
        assert_eq!(
            bits(pooled_rate(&[(90, 0.90), (10, 0.30)])),
            "3feae147ae147ae1"
        );
        assert_eq!(
            bits(pooled_rate(&[(10, 0.95), (90, 0.40)])),
            "3fdd1eb851eb851f"
        );
        // … standardizing reverses it, which is the whole point of §4.7.
        let a_cells: HashMap<&str, (i64, f64)> =
            [("easy", (90_i64, 0.90_f64)), ("hard", (10, 0.30))].into();
        let b_cells: HashMap<&str, (i64, f64)> =
            [("easy", (10_i64, 0.95_f64)), ("hard", (90, 0.40))].into();
        let diff = standardized_difference(&a_cells, &b_cells);
        assert_eq!(bits(diff), "bfb3333333333338"); // -0.07500000000000007
        assert!(diff < 0.0, "standardization must favour B: {diff}");
        // No shared stratum → 0.0, never imputed.
        let lonely: HashMap<&str, (i64, f64)> = [("other", (5_i64, 1.0_f64))].into();
        assert_eq!(standardized_difference(&a_cells, &lonely), 0.0);
        // An empty pool is 0.0, not a division by zero.
        assert_eq!(pooled_rate(&[]), 0.0);
        assert_eq!(standardized_rate::<&str>(&HashMap::new(), &[]), 0.0);
    }

    #[test]
    fn relative_delta_and_risk_difference_carry_the_engines_sign_convention() {
        // The live `cost_relative_delta` on `build × large`, before round(…, 4):
        // relative_delta(0.603816, 10.361756) = 0.9417264795658188.
        assert_eq!(
            bits(relative_delta(0.603_816, 10.361_756)),
            "3fee229f91f0659d"
        );
        assert!(relative_delta(1.0, 2.0) > 0.0); // `new` is cheaper
        assert!(relative_delta(2.0, 1.0) < 0.0);
        // A zero base has no ratio; `-0.0` takes the same guard.
        assert_eq!(relative_delta(5.0, 0.0), 0.0);
        assert_eq!(relative_delta(5.0, -0.0), 0.0);
        assert_eq!(risk_difference(0.75, 0.25), 0.5);
    }

    #[test]
    fn the_confidence_buckets_are_closed_below_and_open_above() {
        assert_eq!(confidence_bucket(1.0), "high");
        assert_eq!(confidence_bucket(0.66), "high");
        assert_eq!(confidence_bucket(0.659_999), "medium");
        assert_eq!(confidence_bucket(0.40), "medium");
        assert_eq!(confidence_bucket(0.399_999), "low");
        assert_eq!(confidence_bucket(0.15), "low");
        assert_eq!(confidence_bucket(0.149_999), "none");
        assert_eq!(confidence_bucket(0.0), "none");
        assert_eq!(confidence_bucket(-1.0), "none");
    }

    #[test]
    fn the_tunables_are_the_ratified_ones() {
        assert_eq!(SEED, 1729);
        assert_eq!(BOOTSTRAP_ITERS, 2000);
        assert_eq!(MIN_SESSIONS_PER_CELL, 5);
        assert_eq!(MIN_MODELS_PER_CELL, 2);
        assert_eq!(MIN_BALANCED_TOTAL, 20);
        assert_eq!(bits(CI_LEVEL), "3feccccccccccccd");
        assert_eq!(bits(MIN_EFFECT_COST), "3fb999999999999a");
        assert_eq!(bits(MIN_EFFECT_SUCCESS), "3fb999999999999a");
        assert_eq!(MIN_EFFECT_GRADE, 0.5);
    }
}
