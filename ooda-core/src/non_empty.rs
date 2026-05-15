//! Non-empty collection newtype.
//!
//! Several [`crate::Action`] payloads carry a collection whose
//! emptiness is a logic bug — `WaitForCi { pending }` with no
//! pending checks would be a request to wait for nothing;
//! `AddressThreads { threads }` with no threads has no agent
//! prompt material. [`NonEmpty<T>`] makes the "at least one
//! element" invariant structural: a value of this type guarantees
//! `len() ≥ 1`, the unwrap on `first()` is statically sound, and
//! render code does not need a dead "empty case" branch.
//!
//! `Serialize` emits the inner `Vec<T>` shape directly (just an
//! array of elements), so JSONL records carrying these payloads
//! are bit-identical to the pre-NonEmpty form.

use serde::{Serialize, Serializer};
use std::num::NonZeroUsize;

/// `Vec<T>` with the invariant `len() ≥ 1` enforced by construction.
///
/// Storing as a tuple-struct around `Vec<T>` keeps the iterator and
/// slice APIs cheap; the invariant is preserved by the
/// constructors and the absence of any `&mut Vec<T>` accessor that
/// could drop the last element.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// Construct a single-element collection.
    pub fn singleton(head: T) -> Self {
        Self(vec![head])
    }

    /// Try to construct from a `Vec<T>`. Returns `None` if the
    /// input is empty; otherwise the input is moved in unchanged.
    pub fn try_from_vec(v: Vec<T>) -> Option<Self> {
        if v.is_empty() { None } else { Some(Self(v)) }
    }

    /// Append another element. Always preserves the invariant.
    pub fn push(&mut self, t: T) {
        self.0.push(t);
    }

    /// First element. The `len ≥ 1` invariant makes this total.
    pub fn first(&self) -> &T {
        &self.0[0]
    }

    /// `NonZeroUsize`-typed length, for call sites that want to
    /// propagate the structural non-empty guarantee through their
    /// own types. Most callers can just use `.len()` (via the
    /// `Deref<Target = [T]>` impl) for a `usize` and rely on the
    /// type-level guarantee that it's `≥ 1`.
    pub fn nonzero_len(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.len()).expect("NonEmpty invariant violated: inner Vec is empty")
    }

    /// Borrow as a `&[T]` slice.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Map each element by reference. The non-empty invariant is
    /// preserved structurally — `self.len() ≥ 1`, so the result
    /// has at least one element and is itself non-empty without
    /// any runtime `unwrap`/`expect`.
    pub fn map_ref<U>(&self, f: impl FnMut(&T) -> U) -> NonEmpty<U> {
        NonEmpty(self.0.iter().map(f).collect())
    }

    /// Like [`Self::map_ref`], but the closure also receives the
    /// 0-based index of each element. Preserves the non-empty
    /// invariant.
    pub fn enumerate_map_ref<U>(&self, mut f: impl FnMut(usize, &T) -> U) -> NonEmpty<U> {
        NonEmpty(self.0.iter().enumerate().map(|(i, t)| f(i, t)).collect())
    }

    /// Last element. Total — the `len ≥ 1` invariant makes this
    /// safe without an `Option` wrap (in contrast to slice's
    /// `last()`, which Deref still exposes for callers that prefer
    /// the slice signature).
    pub fn last(&self) -> &T {
        // SAFETY-equivalent at the type level: `len ≥ 1` ⇒
        // `len - 1 ≥ 0` is a valid index. The slice version
        // returns Option for the empty case which can't happen here.
        &self.0[self.0.len() - 1]
    }

    /// Consume into a `NonEmpty<U>` via a fallible mapping. Short-
    /// circuits on the first `Err`. The non-empty invariant is
    /// preserved structurally — on success, output cardinality
    /// equals input cardinality (≥ 1).
    ///
    /// Useful for building a `NonEmpty<U>` from an iteration that
    /// can fail (e.g. observe-layer fetchers that return `io::Result`
    /// per element).
    pub fn try_map<U, E>(self, mut f: impl FnMut(T) -> Result<U, E>) -> Result<NonEmpty<U>, E> {
        let len = self.0.len();
        let mut out = Vec::with_capacity(len);
        for t in self.0 {
            out.push(f(t)?);
        }
        // `out.len() == len ≥ 1` by the input invariant; the inner
        // Vec is non-empty by construction.
        Ok(NonEmpty(out))
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(ne: NonEmpty<T>) -> Vec<T> {
        ne.0
    }
}

/// Deref to `[T]` so `&NonEmpty<T>` flows into any function
/// expecting `&[T]` without explicit `.as_slice()` calls. All
/// slice methods (`iter`, `len`, indexing, etc.) are available
/// transparently — the only "extra" guarantee `NonEmpty<T>`
/// offers over `&[T]` is that `len() ≥ 1`, which holds whether
/// you reach it through the newtype's methods or through Deref.
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

/// Serialize as a plain array — `[t1, t2, ...]` — so JSONL
/// records that previously carried `Vec<T>` here are byte-
/// identical after the migration.
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
