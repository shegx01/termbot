use std::fmt;

/// A newtype wrapper that prevents accidental logging of sensitive values.
///
/// `Debug` and `Display` both print `[REDACTED]` — derive-based `#[instrument]`
/// spans and any `{:?}` format expressions cannot leak the inner value.
/// Access the raw value only via `Secret::expose()`, which is grep-auditable.
#[derive(Clone)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap a value in `Secret`.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Return a reference to the inner value.
    ///
    /// This is the only way to access the raw bytes; it is deliberately
    /// verbose so accidental uses are easy to spot in a code review.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_formatting_is_redacted() {
        let s = Secret::new(vec![1u8, 2, 3]);
        assert_eq!(format!("{:?}", s), "[REDACTED]");
    }

    #[test]
    fn secret_display_formatting_is_redacted() {
        let s = Secret::new("my-secret-value");
        assert_eq!(format!("{}", s), "[REDACTED]");
    }

    #[test]
    fn secret_expose_returns_inner_value() {
        let s = Secret::new(vec![10u8, 20, 30]);
        assert_eq!(s.expose(), &vec![10u8, 20, 30]);
    }

    #[test]
    fn secret_clone_preserves_inner_value() {
        let s = Secret::new(vec![1u8, 2]);
        let s2 = s.clone();
        assert_eq!(s.expose(), s2.expose());
    }
}
