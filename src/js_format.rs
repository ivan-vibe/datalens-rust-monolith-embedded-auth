//! JS-compatible decimal rounding.
//!
//! `Number.prototype.toFixed` and `Intl.NumberFormat` round the exact decimal representation and
//! break an exact tie by the larger magnitude (`(0.625).toFixed(2) === "0.63"`). Rust's
//! `format!("{:.2}", x)` uses ties-to-even and produces `"0.62"` for the same value, which showed
//! up in gravity scatter point labels and can affect any formatted number.

/// Format `value` with `digits` fractional digits using JS half-away-from-zero rounding.
pub fn fixed_half_away(value: f64, digits: usize) -> String {
    if !value.is_finite() {
        return format!("{value}");
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let factor = 10f64.powi(digits as i32);
    let scaled = value.abs() * factor;
    let floor = scaled.floor();
    let rounded = if scaled - floor >= 0.5 { floor + 1.0 } else { floor };
    let rounded = rounded / factor;
    if digits == 0 {
        format!("{sign}{rounded:.0}")
    } else {
        format!("{sign}{rounded:.digits$}")
    }
}
