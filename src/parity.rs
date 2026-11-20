//! Python numeric and formatting semantics.
//!
//! Every place where the reference implementation relies on CPython behavior
//! that differs from the obvious Rust expression goes through this module:
//! `round()` is half-to-even, `int(x)` truncates, `//` and `%` floor, and
//! `format(x, ".Ng")` / `format(x, ".Nf")` follow CPython's `g`/`f` rules.

/// Python `round(x)` for a float: round half to even.
pub fn py_round(x: f64) -> f64 {
    x.round_ties_even()
}

/// Python `round(x, ndigits)` for a float.
pub fn py_round_to(x: f64, ndigits: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if ndigits == 0 {
        return x.round_ties_even();
    }
    // CPython's float.__round__ with ndigits is done by _Py_dg_dtoa with the
    // same correctly-rounded decimal conversion used by formatting, so the
    // parse-back here reproduces it: 10**n is exact for the range the
    // converter uses, and the decimal conversion is correctly rounded.
    let text = format_f(x, ndigits.max(0) as usize);
    let scaled: f64 = text.parse().unwrap_or(x);
    if ndigits > 0 {
        // Formatting already rounded to ndigits decimals.
        return scaled;
    }
    let factor = 10f64.powi(-ndigits);
    (x / factor).round_ties_even() * factor
}

/// Python `int(x)` for a float: truncation toward zero.
pub fn py_int(x: f64) -> i64 {
    x.trunc() as i64
}

/// Python `int(x)` widened, for values that can exceed `i64`.
pub fn py_int_wide(x: f64) -> i128 {
    x.trunc() as i128
}

/// Python `a // b` for integers: floor division.
pub fn py_floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if r != 0 && ((r < 0) != (b < 0)) { q - 1 } else { q }
}

/// Python `a % b` for integers: result carries the divisor's sign.
pub fn py_mod(a: i64, b: i64) -> i64 {
    let r = a % b;
    if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r }
}

/// Python `a // b` for floats: floor division.
pub fn py_floor_div_f(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// Python `a % b` for floats: result carries the divisor's sign.
pub fn py_mod_f(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) { r + b } else { r }
}

/// Python `max(seq, key=...)`: the first maximum wins ties.
pub fn py_max_by<T, K: PartialOrd>(items: &[T], key: impl Fn(&T) -> K) -> Option<&T> {
    let mut best: Option<&T> = None;
    for item in items {
        match best {
            None => best = Some(item),
            Some(cur) => {
                if key(item) > key(cur) {
                    best = Some(item);
                }
            }
        }
    }
    best
}

/// Python `min(seq, key=...)`: the first minimum wins ties.
pub fn py_min_by<T, K: PartialOrd>(items: &[T], key: impl Fn(&T) -> K) -> Option<&T> {
    let mut best: Option<&T> = None;
    for item in items {
        match best {
            None => best = Some(item),
            Some(cur) => {
                if key(item) < key(cur) {
                    best = Some(item);
                }
            }
        }
    }
    best
}

/// `unittest.assertAlmostEqual(a, b, places)`: `round(a - b, places) == 0`.
pub fn py_almost_eq(a: f64, b: f64, places: u32) -> bool {
    py_round_to(a - b, places as i32) == 0.0
}

fn py_special(x: f64) -> Option<&'static str> {
    if x.is_nan() {
        Some("nan")
    } else if x.is_infinite() {
        Some(if x > 0.0 { "inf" } else { "-inf" })
    } else {
        None
    }
}

/// Python `format(x, ".Nf")` (fixed notation).
pub fn format_f(x: f64, digits: usize) -> String {
    if let Some(s) = py_special(x) {
        return s.to_string();
    }
    format!("{:.*}", digits, x)
}

/// Python `format(x, ".Ng")` (general notation, trailing zeros stripped).
pub fn fmt_g(x: f64, prec: usize) -> String {
    if let Some(s) = py_special(x) {
        return s.to_string();
    }
    let p = if prec == 0 { 1 } else { prec };
    // Exponent-form digits, correctly rounded to p significant digits. Both
    // CPython and Rust round the exact decimal value, so the digits agree.
    let sci = format!("{:.*e}", p - 1, x);
    let (mantissa, exp_text) = sci.split_once('e').expect("exponent form");
    let exp: i32 = exp_text.parse().expect("exponent");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa
        .trim_start_matches('-')
        .chars()
        .filter(|c| *c != '.')
        .collect();

    if exp >= -4 && exp < p as i32 {
        // Fixed notation with p - 1 - exp decimals, trailing zeros stripped.
        let decimals = (p as i32 - 1 - exp) as usize;
        let mut out = format!("{:.*}", decimals, x);
        if out.contains('.') {
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
                out.pop();
            }
        }
        return out;
    }

    let trimmed = digits.trim_end_matches('0');
    let body = if trimmed.is_empty() { "0" } else { trimmed };
    let mut mant = String::new();
    if negative {
        mant.push('-');
    }
    mant.push(body.as_bytes()[0] as char);
    if body.len() > 1 {
        mant.push('.');
        mant.push_str(&body[1..]);
    }
    let exp_negative = exp < 0;
    let sign = if exp_negative { '-' } else { '+' };
    let mag = exp.unsigned_abs();
    if mag < 10 {
        format!("{}e{}0{}", mant, sign, mag)
    } else {
        format!("{}e{}{}", mant, sign, mag)
    }
}

/// Python `str(float)` / `repr(float)`: shortest round-tripping form, with
/// Python's fixed/exponential switchover and `e+NN` exponent spelling.
pub fn fmt_float_repr(x: f64) -> String {
    if let Some(s) = py_special(x) {
        return s.to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // CPython's repr: the shortest decimal that round-trips (David Gay), then
    // fixed notation while -4 <= exp < 16, exponential otherwise.
    let mut digits = String::new();
    let mut exp: i32 = 0;
    let mut negative = false;
    for p in 1..=17usize {
        let sci = format!("{:.*e}", p - 1, x);
        let (mantissa, exp_text) = sci.split_once('e').expect("exponent form");
        let parsed: f64 = sci.parse().expect("parse-back");
        if parsed.to_bits() == x.to_bits() {
            negative = mantissa.starts_with('-');
            exp = exp_text.parse().expect("exponent");
            digits = mantissa
                .trim_start_matches('-')
                .chars()
                .filter(|c| *c != '.')
                .collect();
            break;
        }
    }
    let sign = if negative { "-" } else { "" };
    if exp >= -4 && exp < 16 {
        let mut out = String::new();
        out.push_str(sign);
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                for _ in digits.len()..int_len {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
        return out;
    }
    let mut out = String::new();
    out.push_str(sign);
    out.push(digits.as_bytes()[0] as char);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    out.push('e');
    out.push(if exp < 0 { '-' } else { '+' });
    let mag = exp.unsigned_abs();
    if mag < 10 {
        out.push('0');
    }
    out.push_str(&mag.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_is_half_even() {
        assert_eq!(py_round(0.5), 0.0);
        assert_eq!(py_round(1.5), 2.0);
        assert_eq!(py_round(2.5), 2.0);
        assert_eq!(py_round(-0.5), -0.0);
        assert_eq!(py_round(-2.5), -2.0);
        assert_eq!(py_round_to(2.675, 2), 2.67);
        assert_eq!(py_round_to(1.005, 2), 1.0);
    }

    #[test]
    fn int_truncates_toward_zero() {
        assert_eq!(py_int(1.9), 1);
        assert_eq!(py_int(-1.9), -1);
        assert_eq!(py_int(0.999), 0);
    }

    #[test]
    fn floor_div_and_mod_follow_python() {
        assert_eq!(py_floor_div(7, 2), 3);
        assert_eq!(py_floor_div(-7, 2), -4);
        assert_eq!(py_floor_div(7, -2), -4);
        assert_eq!(py_mod(7, 2), 1);
        assert_eq!(py_mod(-7, 2), 1);
        assert_eq!(py_mod(7, -2), -1);
        assert_eq!(py_floor_div_f(-7.0, 2.0), -4.0);
        assert_eq!(py_mod_f(-7.5, 2.0), 0.5);
    }

    #[test]
    fn max_min_keep_first_on_ties() {
        let v = [(1, 'a'), (3, 'b'), (3, 'c')];
        assert_eq!(py_max_by(&v, |t| t.0).unwrap().1, 'b');
        assert_eq!(py_min_by(&v, |t| t.0).unwrap().1, 'a');
    }

    #[test]
    fn almost_equal_uses_places() {
        assert!(py_almost_eq(1.0001, 1.0, 3));
        assert!(!py_almost_eq(1.001, 1.0, 3));
        assert!(py_almost_eq(0.1 + 0.2, 0.3, 7));
    }

    #[test]
    fn general_format_matches_python() {
        assert_eq!(fmt_g(1.0, 6), "1");
        assert_eq!(fmt_g(0.5, 6), "0.5");
        assert_eq!(fmt_g(1.23456789, 4), "1.235");
        assert_eq!(fmt_g(100000.0, 6), "100000");
        assert_eq!(fmt_g(1000000.0, 6), "1e+06");
        assert_eq!(fmt_g(0.0001, 6), "0.0001");
        assert_eq!(fmt_g(0.00001, 6), "1e-05");
        assert_eq!(fmt_g(1234567890.0, 3), "1.23e+09");
        assert_eq!(fmt_g(-2.5, 2), "-2.5");
        assert_eq!(fmt_g(0.0, 3), "0");
        assert_eq!(fmt_g(9.999e-5, 3), "0.0001");
        assert_eq!(fmt_g(1e16, 1), "1e+16");
    }

    #[test]
    fn fixed_format_matches_python() {
        assert_eq!(format_f(1.0, 2), "1.00");
        assert_eq!(format_f(-0.0, 2), "-0.00");
        assert_eq!(format_f(2.675, 2), "2.67");
        assert_eq!(format_f(0.125, 2), "0.12");
        assert_eq!(format_f(f64::NAN, 2), "nan");
        assert_eq!(format_f(f64::INFINITY, 2), "inf");
    }
}
