//! Adversarial pins for the byte-parity claim of `staxtrace.memory/1`.
//!
//! The wave-1 envelope landing proved 31/31 shipped goldens byte-exact. This
//! file attacks the *general* claim the goldens cannot reach: that
//! `pyjson::dumps_pretty(pyjson::loads(text))` is `json.dumps(json.loads(text),
//! indent=2, default=str)` for every input, not just for the fixtures.
//!
//! Every expectation below was produced by the reference implementation, not by
//! reading the Rust source:
//!
//! ```sh
//! /media/tmos-bumblebe/dev_dev/year26/jul26/StackUnderflow/.venv/bin/python \
//!   -c 'import json,struct
//! s="-1352070300110077.2"
//! f=json.loads(s)
//! print(repr(f), "%016x" % struct.unpack("<Q", struct.pack("<d", f))[0])'
//! # -1352070300110077.2 c31336cd97cc33f5
//! ```
//!
//! Two groups:
//!
//! * [`ties`] — **failing today.** CPython's `repr` breaks an exact halfway
//!   decimal tie to *even*; Rust's shortest-float writer (`{:e}`, which
//!   `python_float_repr` delegates its digits to) breaks it *up*. The digits
//!   themselves differ, so the bytes differ. Both strings round-trip to the same
//!   `f64`, which is why no round-trip test on either side can see it — only a
//!   byte comparison can. Measured incidence: 1,176 of 269,946 random finite
//!   doubles (0.44%). Zero of the 144,158 distinct `REAL` values in the live
//!   store hit it, and zero of 2,582 aggregate cost sums, so it is not
//!   *observable* on this dataset today — but it is not a property of the
//!   dataset, and `cost_usd` in the envelope is a SUM, not a stored value.
//!
//! * [`input_classes`] — inputs CPython's `json` accepts and `serde_json`
//!   rejects (or renders differently). Each is a divergence in the parse half of
//!   the contract; several are engine-level and can only ever be recorded, not
//!   fixed.
//!
//! Run: `cargo test -p stax-memory --test adversarial_pyjson`

use stax_memory::pyjson;

/// CPython `repr` rounds a halfway decimal tie to even; Rust rounds it up.
mod ties {
    use super::pyjson;

    /// `(bit pattern, CPython repr)` — from `repr(float)` on the reference
    /// interpreter, one line per value, no interpretation.
    const CPYTHON_REPR: &[(u64, &str)] = &[
        (0xc313_36cd_97cc_33f5, "-1352070300110077.2"),
        (0x430e_1c6d_958d_7b72, "1059438285926254.2"),
        (0xc314_41f2_33f6_3165, "-1425502010969177.2"),
        (0x42b7_fa57_c450_e950, "26363981746409.312"),
        (0x4319_7469_53fb_86ad, "1791217536786859.2"),
        (0xc2d8_c0fe_9119_c088, "-108868734838530.12"),
        (0xc2bf_0dc9_05b0_7310, "-34144067629171.062"),
    ];

    /// FAILS TODAY. Every value below renders one ULP-of-the-last-digit high.
    #[test]
    fn halfway_decimal_ties_render_like_cpython() {
        let mut wrong = Vec::new();
        for (bits, want) in CPYTHON_REPR {
            let got = pyjson::python_float_repr(f64::from_bits(*bits));
            if got != *want {
                wrong.push(format!("{bits:#018x}: want {want}, got {got}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "python_float_repr diverges from CPython repr on halfway ties:\n  {}",
            wrong.join("\n  ")
        );
    }

    /// The divergence is in the *writer*, not the parser: both runtimes land on
    /// the identical `f64`. Guards against a future "fix" aimed at the parser.
    #[test]
    fn the_tie_values_parse_identically_so_the_writer_owns_the_bug() {
        let parsed = pyjson::loads(r#"{"v": -1352070300110077.2}"#).expect("valid JSON");
        let bits = parsed["v"].as_f64().expect("a float").to_bits();
        assert_eq!(
            bits, 0xc313_36cd_97cc_33f5,
            "serde_json + float_roundtrip must agree with CPython's parse"
        );
    }

    /// Both spellings round-trip to the same double — the reason every
    /// round-trip-shaped test on both sides is blind to this.
    #[test]
    fn both_spellings_round_trip_which_is_why_only_bytes_catch_it() {
        let cpython: f64 = "-1352070300110077.2".parse().unwrap();
        let ours: f64 = "-1352070300110077.3".parse().unwrap();
        assert_eq!(cpython.to_bits(), ours.to_bits());
    }
}

/// Inputs CPython's `json` accepts that `serde_json` does not, and vice versa.
mod input_classes {
    use super::pyjson;

    /// Everything CPython parses and we reject. `expected` is what
    /// `json.dumps(json.loads(text), indent=2)` prints on the reference.
    const CPYTHON_ACCEPTS: &[(&str, &str, &str)] = &[
        (
            "lone-high-surrogate",
            r#"{"v": "\ud800"}"#,
            "{\n  \"v\": \"\\ud800\"\n}",
        ),
        (
            "lone-low-surrogate",
            r#"{"v": "\udc00"}"#,
            "{\n  \"v\": \"\\udc00\"\n}",
        ),
        ("nan-literal", r#"{"v": NaN}"#, "{\n  \"v\": NaN\n}"),
        (
            "infinity-literal",
            r#"{"v": Infinity}"#,
            "{\n  \"v\": Infinity\n}",
        ),
        (
            "negative-infinity-literal",
            r#"{"v": -Infinity}"#,
            "{\n  \"v\": -Infinity\n}",
        ),
    ];

    /// DOCUMENTS TODAY'S BEHAVIOR: every one of these is a hard parse error
    /// here. Flip the assertion if the ledger's disposition ever changes.
    #[test]
    fn cpython_accepts_inputs_this_parser_rejects() {
        for (name, text, cpython) in CPYTHON_ACCEPTS {
            assert!(
                pyjson::loads(text).is_err(),
                "{name}: expected a parse error; CPython prints {cpython}"
            );
        }
    }

    /// Integers CPython keeps exact and we widen to `f64` — silent, not an
    /// error, so a diff is the only way to see it.
    #[test]
    fn integers_beyond_u64_silently_become_floats() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "18446744073709551616",
                "18446744073709551616",
                "1.8446744073709552e+19",
            ),
            (
                "-9223372036854775809",
                "-9223372036854775809",
                "-9.223372036854776e+18",
            ),
            (
                "1000000000000000000000000000000",
                "1000000000000000000000000000000",
                "1e+30",
            ),
        ];
        for (literal, cpython, ours) in cases {
            let text = format!("{{\"v\": {literal}}}");
            let value = pyjson::loads(&text).expect("parses via the f64 fallback");
            let rendered = pyjson::dumps_pretty(&value);
            assert_eq!(rendered, format!("{{\n  \"v\": {ours}\n}}"));
            assert_ne!(
                rendered,
                format!("{{\n  \"v\": {cpython}\n}}"),
                "if this now matches CPython the ledger row is stale"
            );
        }
    }

    /// A big enough integer is not a widening but a refusal — CPython prints it.
    #[test]
    fn integers_beyond_f64_are_a_parse_error_not_a_widening() {
        let text = format!("{{\"v\": 1{}}}", "0".repeat(400));
        assert!(
            pyjson::loads(&text).is_err(),
            "CPython prints all 401 digits"
        );
    }

    /// `serde_json`'s nesting limit is 128; CPython's `json` reaches ~1000.
    /// Anything between is a document CPython reads and this parser refuses.
    #[test]
    fn nesting_depth_ceiling_is_far_below_cpythons() {
        assert!(
            pyjson::loads(&format!("{}1{}", "[".repeat(127), "]".repeat(127))).is_ok(),
            "127 deep must still parse"
        );
        for depth in [128_usize, 129, 200, 500, 900] {
            let text = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
            assert!(
                pyjson::loads(&text).is_err(),
                "depth {depth}: CPython parses this and we do not"
            );
        }
    }

    /// `-0` is an *integer* literal to CPython (which prints `0`) and a negative
    /// zero *float* here.
    #[test]
    fn bare_negative_zero_changes_type() {
        let value = pyjson::loads("-0").expect("parses");
        assert_eq!(pyjson::dumps_pretty(&value), "-0.0", "CPython prints 0");
    }

    /// Not everything adversarial diverges — these are the ones that hold, and
    /// they are worth pinning so a "fix" for the above cannot regress them.
    #[test]
    fn the_cases_that_do_agree_stay_agreeing() {
        // (input, CPython's json.dumps(..., indent=2) output)
        let agree: &[(&str, &str)] = &[
            ("{}", "{}"),
            ("[]", "[]"),
            ("[[]]", "[\n  []\n]"),
            ("[1, []]", "[\n  1,\n  []\n]"),
            (r#"{"a": {}, "b": []}"#, "{\n  \"a\": {},\n  \"b\": []\n}"),
            (
                r#"{"a": 1, "b": 2, "a": 3}"#,
                "{\n  \"a\": 3,\n  \"b\": 2\n}",
            ),
            (
                r#"{"v": "\ud83d\ude80"}"#,
                "{\n  \"v\": \"\\ud83d\\ude80\"\n}",
            ),
            (r#"{"v": "\u007f"}"#, "{\n  \"v\": \"\\u007f\"\n}"),
            (r#"{"v": 1e16}"#, "{\n  \"v\": 1e+16\n}"),
            (r#"{"v": 1e15}"#, "{\n  \"v\": 1000000000000000.0\n}"),
            (r#"{"v": 5e-324}"#, "{\n  \"v\": 5e-324\n}"),
            (r#"{"v": -0.0}"#, "{\n  \"v\": -0.0\n}"),
            (
                r#"{"v": 9223372036854775808}"#,
                "{\n  \"v\": 9223372036854775808\n}",
            ),
        ];
        for (text, want) in agree {
            let value = pyjson::loads(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(&pyjson::dumps_pretty(&value), want, "input {text}");
        }
    }
}
