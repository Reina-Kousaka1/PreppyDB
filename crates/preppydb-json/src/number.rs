//! Numeric JSON values with a total order suitable for index keys.

use std::cmp::Ordering;
use std::fmt;

/// A JSON number.
///
/// PreppyDB keeps integers exact (`i64`) and everything else as `f64`. Integers that
/// do not fit into `i64` are stored as `f64` (documented precision limit).
#[derive(Clone, Copy, Debug)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Number::Int(i) => Some(*i),
            Number::Float(f) => {
                if f.is_finite()
                    && f.fract() == 0.0
                    && *f >= i64::MIN as f64
                    && *f <= i64::MAX as f64
                {
                    Some(*f as i64)
                } else {
                    None
                }
            }
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            Number::Int(i) => *i as f64,
            Number::Float(f) => *f,
        }
    }

    pub fn is_integer(&self) -> bool {
        self.as_i64().is_some()
    }

    fn total_cmp(&self, other: &Number) -> Ordering {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a.cmp(b),
            _ => {
                let a = self.as_f64();
                let b = other.as_f64();
                // Total order over f64: NaN sorts after everything, -0.0 == 0.0.
                match (a.is_nan(), b.is_nan()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                }
            }
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.total_cmp(other) == Ordering::Equal
    }
}

impl Eq for Number {}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_cmp(other)
    }
}

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        Number::Int(value)
    }
}

impl From<u64> for Number {
    fn from(value: u64) -> Self {
        if value <= i64::MAX as u64 {
            Number::Int(value as i64)
        } else {
            Number::Float(value as f64)
        }
    }
}

impl From<i32> for Number {
    fn from(value: i32) -> Self {
        Number::Int(value as i64)
    }
}

impl From<usize> for Number {
    fn from(value: usize) -> Self {
        Number::from(value as u64)
    }
}

impl From<f64> for Number {
    fn from(value: f64) -> Self {
        Number::Float(value)
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Number::Int(i) => write!(f, "{i}"),
            Number::Float(v) => {
                if !v.is_finite() {
                    // JSON has no representation for NaN/Infinity; be explicit.
                    return Err(fmt::Error);
                }
                let mut s = format!("{v}");
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    s.push_str(".0");
                }
                f.write_str(&s)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_float_equality_is_numeric() {
        assert_eq!(Number::Int(3), Number::Float(3.0));
        assert!(Number::Int(3) < Number::Float(3.5));
        assert_eq!(Number::Float(0.0), Number::Float(-0.0));
    }

    #[test]
    fn nan_sorts_last_and_compares_equal() {
        assert_eq!(
            Number::Float(f64::NAN).cmp(&Number::Float(f64::NAN)),
            Ordering::Equal
        );
        assert_eq!(
            Number::Float(f64::NAN).cmp(&Number::Int(1)),
            Ordering::Greater
        );
    }

    #[test]
    fn display_keeps_float_marker() {
        assert_eq!(Number::Float(1.0).to_string(), "1.0");
        assert_eq!(Number::Int(1).to_string(), "1");
        assert_eq!(Number::Float(0.5).to_string(), "0.5");
    }

    #[test]
    fn large_u64_becomes_float() {
        assert!(matches!(Number::from(u64::MAX), Number::Float(_)));
        assert!(matches!(Number::from(7u64), Number::Int(7)));
    }
}
