//! Non-empty collection newtype.
//!
//! Carries a collection whose emptiness would be a logic bug —
//! e.g. an action payload that means "wait for these things" or
//! "act on these items" is meaningless with zero things.
//! [`NonEmpty<T>`] makes the "at least one element" invariant
//! structural: a value of the type guarantees `len() ≥ 1`,
//! `first()` is total, and downstream code carries no dead
//! "empty case" branch.
//!
//! `Serialize` is transparent — emits the inner `Vec<T>` shape
//! directly — so on-the-wire records are byte-identical to a
//! plain `Vec<T>`.

use serde::{Serialize, Serializer};
use std::num::NonZeroUsize;

/// `Vec<T>` with the invariant `len() ≥ 1` enforced by construction.
///
/// Backed by a `Vec<T>` so slice and iterator APIs come for free.
/// The invariant is preserved by the absence of any mutable
/// accessor that could empty the inner vector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// Construct a single-element collection.
    pub fn singleton(head: T) -> Self {
        Self(vec![head])
    }

    /// Try to construct from a `Vec<T>`. Returns `None` if the
    /// input is empty; otherwise the input is moved in unchanged.
    #[must_use]
    pub fn try_from_vec(v: Vec<T>) -> Option<Self> {
        if v.is_empty() { None } else { Some(Self(v)) }
    }

    /// Append another element. Always preserves the invariant.
    pub fn push(&mut self, t: T) {
        self.0.push(t);
    }

    /// First element. The `len ≥ 1` invariant makes this total.
    #[must_use]
    pub fn first(&self) -> &T {
        &self.0[0]
    }

    /// `NonZeroUsize`-typed length, for call sites that propagate
    /// the non-empty guarantee through their own types.
    ///
    /// # Panics
    ///
    /// Panics if the `NonEmpty` invariant is violated — statically
    /// impossible through the public API.
    #[must_use]
    pub fn nonzero_len(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.len()).expect("NonEmpty invariant violated: inner Vec is empty")
    }

    /// Borrow as a `&[T]` slice.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Map each element by reference. Output cardinality equals
    /// input cardinality, preserving the non-empty invariant
    /// without runtime checks.
    pub fn map_ref<U>(&self, f: impl FnMut(&T) -> U) -> NonEmpty<U> {
        NonEmpty(self.0.iter().map(f).collect())
    }

    /// [`Self::map_ref`] with the 0-based index passed alongside
    /// each element.
    pub fn enumerate_map_ref<U>(&self, mut f: impl FnMut(usize, &T) -> U) -> NonEmpty<U> {
        NonEmpty(self.0.iter().enumerate().map(|(i, t)| f(i, t)).collect())
    }

    /// Last element. Total — the `len ≥ 1` invariant makes this
    /// safe without an `Option` wrap. Slice's `Option`-returning
    /// `last()` is still reachable through `Deref` for callers
    /// that prefer the slice signature.
    #[must_use]
    pub fn last(&self) -> &T {
        &self.0[self.0.len() - 1]
    }

    /// Consume into a `NonEmpty<U>` via a fallible mapping.
    /// Short-circuits on the first `Err`; on success, output
    /// cardinality equals input cardinality, preserving the
    /// non-empty invariant.
    ///
    /// # Errors
    ///
    /// Returns the first `Err(E)` produced by `f`.
    pub fn try_map<U, E>(self, mut f: impl FnMut(T) -> Result<U, E>) -> Result<NonEmpty<U>, E> {
        let len = self.0.len();
        let mut out = Vec::with_capacity(len);
        for t in self.0 {
            out.push(f(t)?);
        }
        Ok(NonEmpty(out))
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(ne: NonEmpty<T>) -> Vec<T> {
        ne.0
    }
}

/// Transparent deref to `[T]`: every slice method is available
/// without `.as_slice()` boilerplate. The `len() ≥ 1` guarantee
/// holds whether reached through the newtype's methods or through
/// the slice surface.
impl<T> std::ops::Deref for NonEmpty<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Serialize transparently as the inner `Vec<T>` so on-the-wire
/// records are byte-identical to a plain `Vec<T>`.
impl<T: Serialize> Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(ser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_constructs_length_one() {
        let ne = NonEmpty::singleton(42);
        assert_eq!(ne.len(), 1);
        assert_eq!(ne.nonzero_len().get(), 1);
        assert_eq!(*ne.first(), 42);
    }

    #[test]
    fn try_from_vec_rejects_empty() {
        let v: Vec<i32> = vec![];
        assert!(NonEmpty::try_from_vec(v).is_none());
    }

    #[test]
    fn try_from_vec_preserves_order() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        assert_eq!(ne.len(), 3);
        assert_eq!(ne.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn push_extends_collection() {
        let mut ne = NonEmpty::singleton("a");
        ne.push("b");
        ne.push("c");
        assert_eq!(ne.as_slice(), &["a", "b", "c"]);
    }

    #[test]
    fn iter_visits_all_elements_in_order() {
        let ne = NonEmpty::try_from_vec(vec![10, 20, 30]).unwrap();
        let collected: Vec<i32> = ne.iter().copied().collect();
        assert_eq!(collected, vec![10, 20, 30]);
    }

    #[test]
    fn into_iter_yields_owned_elements() {
        let ne = NonEmpty::try_from_vec(vec![String::from("x"), String::from("y")]).unwrap();
        let collected: Vec<String> = ne.into_iter().collect();
        assert_eq!(collected, vec!["x", "y"]);
    }

    #[test]
    fn convertible_back_to_vec() {
        let ne = NonEmpty::try_from_vec(vec![1, 2]).unwrap();
        let v: Vec<i32> = ne.into();
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn map_ref_preserves_length_and_order() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let doubled: NonEmpty<i32> = ne.map_ref(|x| x * 2);
        assert_eq!(doubled.as_slice(), &[2, 4, 6]);
    }

    #[test]
    fn map_ref_can_change_type() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let strings: NonEmpty<String> = ne.map_ref(|x| format!("n={x}"));
        assert_eq!(strings.as_slice(), &["n=1", "n=2", "n=3"]);
    }

    #[test]
    fn enumerate_map_ref_provides_zero_indexed_position() {
        let ne = NonEmpty::try_from_vec(vec!["a", "b", "c"]).unwrap();
        let with_idx: NonEmpty<String> = ne.enumerate_map_ref(|i, s| format!("{i}:{s}"));
        assert_eq!(with_idx.as_slice(), &["0:a", "1:b", "2:c"]);
    }

    #[test]
    fn last_returns_owned_ref_total_no_option() {
        let ne = NonEmpty::try_from_vec(vec![10, 20, 30]).unwrap();
        assert_eq!(*ne.last(), 30);
        let singleton = NonEmpty::singleton(42);
        assert_eq!(*singleton.last(), 42);
    }

    #[test]
    fn try_map_propagates_first_error_and_short_circuits() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let result: Result<NonEmpty<i32>, &'static str> =
            ne.try_map(|x| if x == 2 { Err("hit two") } else { Ok(x * 10) });
        assert_eq!(result, Err("hit two"));
    }

    #[test]
    fn try_map_succeeds_when_all_ok_and_preserves_length() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let result: Result<NonEmpty<i32>, ()> = ne.try_map(|x| Ok(x * 10));
        let out = result.unwrap();
        assert_eq!(out.as_slice(), &[10, 20, 30]);
    }

    #[test]
    fn try_map_on_singleton_yields_singleton() {
        let ne = NonEmpty::singleton("hello");
        let result: Result<NonEmpty<usize>, ()> = ne.try_map(|s| Ok(s.len()));
        let out = result.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(*out.first(), 5);
    }

    #[test]
    fn map_ref_on_singleton_preserves_singleton_shape() {
        let ne = NonEmpty::singleton(42);
        let mapped = ne.map_ref(|x| x + 1);
        assert_eq!(mapped.len(), 1);
        assert_eq!(*mapped.first(), 43);
    }

    #[test]
    fn serializes_as_array_not_object() {
        let ne = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let json = serde_json::to_string(&ne).unwrap();
        // Bit-identical to a plain Vec serialization — no struct
        // wrapper.
        assert_eq!(json, "[1,2,3]");
    }
}
