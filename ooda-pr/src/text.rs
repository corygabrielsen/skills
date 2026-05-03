//! Prose helpers — count-noun pluralization for user-facing strings.
//!
//! Regular plural only (append `s`). Irregular plurals are out of
//! scope; add them when a real consumer needs one.

/// Format a count + noun phrase with English-regular pluralization.
///
/// `count(1, "thread")` → `"1 thread"`
/// `count(0, "thread")` → `"0 threads"`
/// `count(5, "low-confidence finding")` → `"5 low-confidence findings"`
pub fn count(n: usize, noun: &str) -> String {
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
        assert_eq!(count(2, "low-confidence finding"), "2 low-confidence findings");
    }
}
