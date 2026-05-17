//! Count-noun pluralization for user-facing strings.
//!
//! Domain: English-regular pluralization only. Singular iff `n == 1`;
//! every other count (including zero) takes the plural form.
//! Irregular plurals are out of scope — the caller is responsible
//! for choosing a regular noun phrase.

/// Format `<n> <noun>` with English-regular pluralization.
///
/// Invariant: the returned string is singular iff `n == 1`.
pub(crate) fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singular() {
        assert_eq!(count(1, "thread"), "1 thread");
    }

    #[test]
    fn zero_is_plural() {
        assert_eq!(count(0, "thread"), "0 threads");
    }

    #[test]
    fn plural() {
        assert_eq!(count(5, "thread"), "5 threads");
    }

    #[test]
    fn noun_phrase() {
        assert_eq!(
            count(2, "low-confidence finding"),
            "2 low-confidence findings"
        );
    }
}
