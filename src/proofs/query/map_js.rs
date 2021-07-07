use super::super::Node;
use crate::Result;
use failure::{bail, ensure, format_err};
use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};
use wasm_bindgen::prelude::*;

/// `MapBuilder` allows a consumer to construct a `Map` by inserting the nodes
/// contained in a proof, in key-order.
pub(crate) struct JsMapBuilder(JsMap);

impl JsMapBuilder {
    /// Creates a new `MapBuilder` with an empty internal `Map`.
    pub fn new() -> Self {
        JsMapBuilder(JsMap {
            entries: Default::default(),
            right_edge: true,
        })
    }

    /// Adds the node's data to the uncerlying `Map` (if node is type `KV`), or
    /// makes a note of non-contiguous data (if node is type `KVHash` or
    /// `Hash`).
    pub fn insert(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::KV(key, value) => {
                if let Some((prev_key, _)) = self.0.entries.last_key_value() {
                    ensure!(
                        key > prev_key,
                        "Expected nodes to be in increasing key order"
                    );
                }

                let value = (self.0.right_edge, value.clone());
                self.0.entries.insert(key.clone(), value);
                self.0.right_edge = true;
            }
            _ => self.0.right_edge = false,
        }

        Ok(())
    }

    /// Consumes the `MapBuilder` and returns its internal `Map`.
    pub fn build(self) -> JsMap {
        self.0
    }
}

/// `Map` stores data extracted from a proof (which has already been verified
/// against a known root hash), and allows a consumer to access the data by
/// looking up individual keys using the `get` method, or iterating over ranges
/// using the `range` method.
#[wasm_bindgen]
pub struct JsMap {
    entries: BTreeMap<Vec<u8>, (bool, Vec<u8>)>,
    right_edge: bool,
}

#[wasm_bindgen]
pub struct JsFlatMap {
    inner: Vec<(Vec<u8>, (bool, Vec<u8>))>,
    prev_key: Option<Vec<u8>>,
    start_key: Option<Vec<u8>> 
}

impl Iterator for JsFlatMap {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let (key, (contiguous, value)) = match self.inner.iter.next() {
            // no more items, ensure no data was excluded at end of range
            None => {
                return match check_end_bound(self.prev_key, self) {
                    Err(err) => Some(Err(err)),
                    Ok(_) => None,
                }
            }

            // got next item, destructure
            Some((key, (contiguous, value))) => (key, (contiguous, value)),
        };

        self.prev_key = Some(key.clone());

        // don't check for contiguous nodes if we have an exact match for lower
        // bound
        let skip_exclusion_check = if let Some(ref start_key) = self.start_key {
            start_key == key
        } else {
            false
        };

        // if nodes weren't contiguous, we cannot verify that we have all values
        // in the desired range
        if !skip_exclusion_check && !contiguous {
            return Some(Err(format_err!("Proof is missing data for query")));
        }

        // passed checks, return entry
        Some(Ok((key.as_slice(), value.as_slice())))
    }
}

#[wasm_bindgen]
struct OptionVec {
    inner: Result<Option<Vec<u8>>>
}

impl OptionVec {
    fn new(inner: Result<Option<Vec<u8>>>) -> OptionVec {
        OptionVec {
            inner
        } 
    }
}

#[wasm_bindgen]
impl JsMap {
    /// Gets the value for a single key, or `None` if the key was proven to not
    /// exist in the tree. If the proof does not include the data and also does
    /// not prove that the key is absent in the tree (meaning the proof is not
    /// valid), an error will be returned.
    pub fn get(&self, key: &[u8]) -> OptionVec {
        // if key is in proof just get from entries
        if let Some((_, value)) = self.entries.get(key) {
            return OptionVec::new(Ok(Some(value.clone())));
        }

        // otherwise, use range which only includes exact key match to check
        // absence proof
        let entry = match self
            .range(key.into(), key.into())
            .next()
            .transpose() {
                Ok(v) => v,
                Err(e) => {
                    return OptionVec::new(Err(e));
                }
            }.map(|(_, value)| value.to_vec());
        OptionVec::new(Ok(entry))
    }
    
    /// Returns an iterator over all (key, value) entries in the requested range
    /// of keys. If during iteration we encounter a gap in the data (e.g. the
    /// proof did not include all nodes within the range), the iterator will
    /// yield an error.
    pub fn range(
        self, 
        start_bound: Vec<u8>, 
        end_bound: Vec<u8>) ->  JsFlatMap {

        let start_bound = Bound::Included(start_bound);
        let end_bound = Bound::Included(end_bound);
        let start_key = bound_to_inner(start_bound).map(|x| (*x).into());
        let bounds = bounds_to_vec(start_key.unwrap(), end_bound);
        
        self.entries.range(bounds).collect()
    }
}

/// Returns `None` for `Bound::Unbounded`, or the inner key value for
/// `Bound::Included` and `Bound::Excluded`.
fn bound_to_inner<T>(bound: Bound<T>) -> Option<T> {
    match bound {
        Bound::Unbounded => None,
        Bound::Included(key) | Bound::Excluded(key) => Some(key),
    }
}

fn bound_to_vec(bound: Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
        Bound::Included(k) => Bound::Included(k.to_vec()),
    }
}

fn bounds_to_vec(start_bound: Bound<Vec<u8>>, end_bound: Bound<Vec<u8>>) -> impl RangeBounds<Vec<u8>> {
    (
        bound_to_vec(start_bound),
        bound_to_vec(end_bound),
    )
}

/// Returns an error if the proof does not properly prove the end of the
/// range.
fn check_end_bound(prev_key: Option<Vec<u8>>, map: JsMap) -> Result<()> {
    let excluded_data = match prev_key {
        // unbounded end, ensure proof has not excluded data at global right
        // edge of tree
        None => !map.right_edge,

        // bounded end (inclusive or exclusive), ensure we had an exact
        // match or next node is contiguous
        Some(ref key) => {
            // get neighboring node to the right (if any)
            let range = (Bound::Excluded(key.to_vec()), Bound::<Vec<u8>>::Unbounded);
            let maybe_end_node = map.entries.range(range).next();

            match maybe_end_node {
                // reached global right edge of tree
                None => !map.right_edge,

                // got end node, must be contiguous
                Some((_, (contiguous, _))) => !contiguous,
            }
        }
    };

    if excluded_data {
        bail!("Proof is missing data for query");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HASH_LENGTH;

    #[test]
    #[should_panic(expected = "Expected nodes to be in increasing key order")]
    fn mapbuilder_insert_out_of_order() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 2], vec![])).unwrap();
    }

    #[test]
    #[should_panic(expected = "Expected nodes to be in increasing key order")]
    fn mapbuilder_insert_dupe() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![])).unwrap();
    }

    #[test]
    fn mapbuilder_insert_including_edge() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![])).unwrap();

        assert!(builder.0.right_edge);
    }

    #[test]
    fn mapbuilder_insert_abridged_edge() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![])).unwrap();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();

        assert!(!builder.0.right_edge);
    }

    #[test]
    fn mapbuilder_build() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();

        let map = builder.build();
        let mut entries = map.entries.iter();
        assert_eq!(entries.next(), Some((&vec![1, 2, 3], &(true, vec![1]))));
        assert_eq!(entries.next(), Some((&vec![1, 2, 4], &(false, vec![2]))));
        assert_eq!(entries.next(), None);
        assert!(map.right_edge);
    }

    #[test]
    fn map_get_included() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();

        let map = builder.build();
        assert_eq!(map.get(&[1, 2, 3]).inner.unwrap().unwrap(), vec![1],);
        assert_eq!(map.get(&[1, 2, 4]).inner.unwrap().unwrap(), vec![2],);
    }

    #[test]
    #[should_panic(expected = "Proof is missing data for query")]
    fn map_get_missing_absence_proof() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();

        let map = builder.build();
        map.get(&[1, 2, 3, 4]).inner.unwrap();
    }

    #[test]
    fn map_get_valid_absence_proof() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();

        let map = builder.build();
        assert!(map.get(&[1, 2, 3, 4]).inner.unwrap().is_none());
    }

    #[test]
    #[should_panic(expected = "Proof is missing data for query")]
    fn range_abridged() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::Hash([0; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();

        let map = builder.build();
        let mut range = map.range(vec![1u8, 2, 3], vec![1u8, 2, 4]);
        assert_eq!(range.next().unwrap().unwrap(), (vec![1, 2, 3], vec![1]));
        range.next().unwrap().unwrap();
    }

    #[test]
    fn range_ok() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![2])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 5], vec![3])).unwrap();

        let map = builder.build();
        let mut range = map.range(vec![1u8, 2, 3], vec![1u8, 2, 5]);
        assert_eq!(range.next().unwrap().unwrap(), (vec![1, 2, 3], vec![1]));
        assert_eq!(range.next().unwrap().unwrap(), (vec![1, 2, 4], vec![2]));
        assert!(range.next().is_none());
    }
    /*
    #[test]
    #[should_panic(expected = "Proof is missing data for query")]
    fn range_lower_unbounded_map_non_contiguous() {
        let mut builder = JsMapBuilder::new();
        builder.insert(&Node::KV(vec![1, 2, 3], vec![1])).unwrap();
        builder.insert(&Node::Hash([1; HASH_LENGTH])).unwrap();
        builder.insert(&Node::KV(vec![1, 2, 4], vec![1])).unwrap();

        let map = builder.build();

        let mut range = map.range(..&[1u8, 2, 5][..]);
        range.next().unwrap().unwrap();
        assert_eq!(range.next().unwrap().unwrap(), (vec![1], vec![1]));
    }
    */
}
