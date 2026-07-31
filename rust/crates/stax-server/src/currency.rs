//! `infra/currency.py::active_currency_payload` — the `currency` block stamped
//! onto every cost-bearing response.
//!
//! ```python
//! payload = format_in_currency(0.0)   # {code, symbol, rate_from_usd, amount, warning}
//! payload.pop("amount", None)         # -> {code, symbol, rate_from_usd, warning}
//! ```
//!
//! Key order is that of the dict literal in `format_in_currency`, minus the
//! popped `amount` — `code`, `symbol`, `rate_from_usd`, `warning` — and the
//! order is part of the byte contract, so it is written out literally here
//! rather than derived from a struct whose field order someone could sort.
//!
//! # Scope, stated rather than implied (DIV-052)
//!
//! `resolve_rate` short-circuits USD to `(1.0, None)` before touching anything.
//! Every other code walks a chain that ends at a Frankfurter HTTP fetch, a
//! disk cache under the app dir, and a hardcoded snapshot. Only the USD leg is
//! ported: it is the default, it is what the harness states are configured
//! with, and it is the only leg with no network in it.
//!
//! A non-USD configured currency therefore returns [`CurrencyUnsupported`]
//! instead of a payload. That is deliberate and it is the safe direction — the
//! rule `format_in_currency` states in prose ("never silently emit
//! `rate_from_usd=1.0` for a non-USD code") is enforced here by refusing to
//! answer at all, rather than by inventing a rate.

use serde_json::{Map, Value};

/// The configured currency is not USD and the rate chain is not ported.
#[derive(Debug, Clone)]
pub struct CurrencyUnsupported {
    /// The configured code, uppercased the way `format_in_currency` does.
    pub code: String,
}

impl std::fmt::Display for CurrencyUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "currency {} needs the Frankfurter rate chain, which wave 5 does not port (DIV-052)",
            self.code
        )
    }
}

impl std::error::Error for CurrencyUnsupported {}

/// `active_currency_payload()`.
///
/// # Errors
/// When `configured` resolves to anything but USD — see the module docs.
pub fn active_currency_payload(configured: &str) -> Result<Value, CurrencyUnsupported> {
    // `format_in_currency`: `requested = (target or "USD").upper()`, then a
    // non-`^[A-Z]{3}$` code silently becomes USD *before* resolution.
    let mut requested = configured.to_ascii_uppercase();
    if !is_iso_code(&requested) {
        requested = "USD".to_owned();
    }
    if requested != "USD" {
        return Err(CurrencyUnsupported { code: requested });
    }

    let mut payload = Map::new();
    payload.insert("code".to_owned(), Value::String("USD".to_owned()));
    payload.insert("symbol".to_owned(), Value::String(symbol("USD")));
    // `resolve_rate` returns the *float* 1.0 for USD, so this renders `1.0`,
    // not `1`. `_SYMBOLS`-adjacent detail, same class of trap.
    payload.insert("rate_from_usd".to_owned(), Value::from(1.0_f64));
    payload.insert("warning".to_owned(), Value::Null);
    Ok(Value::Object(payload))
}

/// `_ISO_CODE_RE = ^[A-Z]{3}$`, applied after the uppercase.
fn is_iso_code(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())
}

/// `get_symbol` — `_SYMBOLS.get(code, code)`.
///
/// Only the USD entry is reachable while [`active_currency_payload`] refuses
/// everything else, so only it is transcribed; the fallback is the code itself,
/// which is the Python default and keeps this honest for the day the chain
/// lands.
fn symbol(code: &str) -> String {
    match code {
        "USD" => "$".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_payload_is_byte_exact() {
        let payload = active_currency_payload("USD").expect("USD");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            r#"{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}"#
        );
    }

    #[test]
    fn a_junk_code_degrades_to_usd_before_resolution() {
        // `format_in_currency` rewrites a non-ISO code to USD *and then*
        // resolves, so `currency = "us"` yields the plain USD payload with no
        // warning at all — not a warning about the junk.
        assert!(active_currency_payload("us").is_ok());
        assert!(active_currency_payload("").is_ok());
    }

    #[test]
    fn a_real_foreign_code_refuses_rather_than_guessing() {
        let err = active_currency_payload("eur").expect_err("not ported");
        assert_eq!(err.code, "EUR");
    }

    #[test]
    fn rate_renders_as_a_float_not_an_int() {
        // `1` instead of `1.0` is a one-byte body divergence on every
        // cost-bearing response in the app.
        let payload = active_currency_payload("USD").expect("USD");
        assert!(stax_memory::pyjson::dumps_http(&payload).contains("\"rate_from_usd\":1.0"));
    }
}
