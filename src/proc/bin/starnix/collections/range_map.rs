// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Borrow;
use std::cmp::{Eq, Ord, Ordering, PartialOrd};
use std::collections::BTreeMap;
use std::iter::Iterator;
use std::ops::Bound;
use std::ops::Range;

/// Keys for the map inside RangeMap.
///
/// This object holds a Range but implements the ordering traits according to
/// the start of the range. Using this struct lets us store both ends of the
/// range in the BTreeMap and recover ranges by querying for their start point.
struct RangeStart<T> {
    range: Range<T>,
}

impl<T> RangeStart<T>
where
    T: Clone,
{
    /// Wrap the given range in a RangeStart.
    ///
    /// Used in the BTreeMap to order the entries by the start of the range but
    /// also remember the end of the range.
    fn new(range: Range<T>) -> Self {
        RangeStart { range }
    }

    /// An empty range with both endpoints at the start.
    ///
    /// Used for queries into the BTreeMap, but never stored in the BTreeMap.
    fn from_point(point: &T) -> Self {
        RangeStart { range: Range { start: point.clone(), end: point.clone() } }
    }
}

impl<T> Clone for RangeStart<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        RangeStart { range: self.range.clone() }
    }
}

/// PartialEq according to the start of the Range.
impl<T> PartialEq for RangeStart<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.range.start.eq(&other.range.start)
    }
}

/// Eq according to the start of the Range.
impl<T> Eq for RangeStart<T> where T: Eq {}

/// PartialOrd according to the start of the Range.
impl<T> PartialOrd for RangeStart<T>
where
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.range.start.partial_cmp(&other.range.start)
    }
}

/// Ord according to the start of the Range.
impl<T> Ord for RangeStart<T>
where
    T: Ord,
{
    fn cmp(&self, other: &Self) -> Ordering {
        self.range.start.cmp(&other.range.start)
    }
}

/// A map from a range of keys to values.
///
/// At any given time, the map contains a set of non-overlapping, non-empty
/// ranges of type K that are associated with values of type V.
///
/// A given range can be split into two separate ranges if some of the
/// intermediate values are removed from the map of if another value is
/// inserted over the intermediate values. When that happens, the value
/// for the split range is cloned using the Clone trait.
///
/// Adjacent ranges are not merged. Even if the value is "the same" (for some
/// definition of "the same"), the ranges are kept separately.
///
/// Querying a point in the map returns not only the value stored at that point
/// but also the range that value occupies in the map.
pub struct RangeMap<K, V> {
    map: BTreeMap<RangeStart<K>, V>,
}

impl<K, V> Default for RangeMap<K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    /// By default, a RangeMap is empty.
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> RangeMap<K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    /// Returns an empty RangeMap.
    pub fn new() -> Self {
        RangeMap { map: BTreeMap::new() }
    }

    /// Returns the range (and associated value) that contains the given point,
    /// if any.
    ///
    /// At most one range and value can exist at a given point because the
    /// ranges in the map are non-overlapping.
    ///
    /// Empty ranges do not contain any points and therefore cannot be found
    /// using this method. Rather than being stored in the map, values
    /// associated with empty ranges are dropped.
    pub fn get(&self, point: &K) -> Option<(&Range<K>, &V)> {
        self.map
            .range((Bound::Unbounded, Bound::Included(RangeStart::from_point(point))))
            .next_back()
            .filter(|(k, _)| k.range.contains(point))
            .map(|(k, v)| (&k.range, v))
    }

    /// Returns the range (and associated mutable value) that contains the
    /// given point, if any.
    ///
    /// Similar to "get", but the value is returned as a mutable reference,
    /// which lets the caller modify the value stored in the map.
    pub fn get_mut(&mut self, point: &K) -> Option<(&Range<K>, &mut V)> {
        self.map
            .range_mut((Bound::Unbounded, Bound::Included(RangeStart::from_point(point))))
            .next_back()
            .filter(|(k, _)| k.range.contains(point))
            .map(|(k, v)| (&k.range, v))
    }

    /// Inserts a range with the given value.
    ///
    /// The keys included in the given range are now associated with the given
    /// value. If those keys were previously associated with another value,
    /// are no longer associated with that previous value.
    ///
    /// This method can cause one or more values in the map to be dropped if
    /// the all of the keys associated with those values are contained within
    /// the given range.
    pub fn insert(&mut self, range: Range<K>, value: V) {
        if range.start < range.end {
            self.remove(&range);
            self.map.insert(RangeStart::new(range), value);
        }
    }

    /// Remove the given range from the map.
    ///
    /// The keys included in the given range are no longer associated with any
    /// values.
    ///
    /// This method can cause one or more values in the map to be dropped if
    /// the all of the keys associated with those values are contained within
    /// the given range.
    pub fn remove(&mut self, range: &Range<K>) {
        // If the given range is empty, there is nothing to do.
        if range.end <= range.start {
            return;
        }

        // Find the range (if any) that intersects the start of range.
        //
        // There can be at most one such range because we maintain the
        // invariant that the ranges stored in the map are non-overlapping.
        if let Some((old_range, v)) =
            self.get(&range.start).map(|(range, v)| (range.clone(), v.clone()))
        {
            // Remove that range from the map.
            self.remove_exact_range(&old_range);

            // If the removed range extends after the end of the given range,
            // re-insert the part of the old range that extends beyond the end
            // of the given range.
            if old_range.end > range.end {
                self.insert_into_empty_range(range.end.clone()..old_range.end, v.clone());
            }

            // If the removed range extends before the start of the given
            // range, re-insert the part of the old range that extends before
            // the start of the given range.
            if old_range.start < range.start {
                self.insert_into_empty_range(old_range.start..range.start.clone(), v);
            }

            // Notice that we can end up splitting the old range into two
            // separate ranges if the old range extends both beyond the given
            // range and before the given range.
        }

        // Find the range (if any) that intersects the end of range.
        //
        // There can be at most one such range because we maintain the
        // invariant that the ranges stored in the map are non-overlapping.
        //
        // We exclude the end of the given range because a range that starts
        // exactly at the end of the given range does not overalp the given
        // range.
        if let Some((old_range, v)) = self
            .map
            .range((
                Bound::Included(RangeStart::from_point(&range.start)),
                Bound::Excluded(RangeStart::from_point(&range.end)),
            ))
            .next_back()
            .filter(|(k, _)| k.range.contains(&range.end))
            .map(|(k, v)| (k.range.clone(), v.clone()))
        {
            // Remove that range from the map.
            self.remove_exact_range(&old_range);

            // If the removed range extends after the end of the given range,
            // re-insert the part of the old range that extends beyond the end
            // of the given range.
            if old_range.end > range.end {
                self.insert_into_empty_range(range.end.clone()..old_range.end, v);
            }
        }

        // Remove any remaining ranges that are contained within the range.
        //
        // These ranges cannot possibly extend beyond the given range because
        // we will have already removed them from the map at this point.
        //
        // We collect the doomed keys into a Vec to avoid mutating the map
        // during the iteration.
        let doomed: Vec<_> = self
            .map
            .range((
                Bound::Included(RangeStart::from_point(&range.start)),
                Bound::Excluded(RangeStart::from_point(&range.end)),
            ))
            .map(|(k, _)| k.clone())
            .collect();

        for key in &doomed {
            self.map.remove(key);
        }
    }

    /// Iterate over the ranges in the map.
    pub fn iter(&self) -> impl Iterator<Item = (&Range<K>, &V)> {
        self.map.iter().map(|(k, value)| (&k.range, value))
    }

    /// Iterate over the ranges in the map, starting at the first range starting after or at the given point.
    pub fn iter_starting_at(&self, point: &K) -> impl Iterator<Item = (&Range<K>, &V)> {
        self.map
            .range((Bound::Included(RangeStart::from_point(point)), Bound::Unbounded))
            .map(|(k, value)| (&k.range, value))
    }

    /// Iterate over the ranges in the map that intersect the requested range.
    pub fn intersection<R>(&self, range: R) -> impl Iterator<Item = (&Range<K>, &V)>
    where
        R: Borrow<Range<K>>,
    {
        let range = range.borrow();
        let start = self.get(&range.start).map(|(r, _)| &r.start).unwrap_or(&range.start);
        self.map
            .range((
                Bound::Included(RangeStart::from_point(start)),
                Bound::Excluded(RangeStart::from_point(&range.end)),
            ))
            .map(|(k, value)| (&k.range, value))
    }

    /// Associate the keys in the given range with the given value.
    ///
    /// Callers must ensure that the keys in the given range are not already
    /// associated with any values.
    fn insert_into_empty_range(&mut self, range: Range<K>, value: V) {
        self.map.insert(RangeStart::new(range), value);
    }

    /// Remove the given range from the map.
    ///
    /// Callers must ensure that the exact range provided as an argument is
    /// contained in the map.
    fn remove_exact_range(&mut self, range: &Range<K>) {
        self.map.remove(&RangeStart::new(range.clone()));
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_empty() {
        let mut map = RangeMap::<u32, i32>::new();

        assert!(map.get(&12).is_none());
        map.remove(&(10..34));
        map.remove(&(34..10));
    }

    #[test]
    fn test_insert_into_empty() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(10..34, -14);

        assert_eq!((&(10..34), &-14), map.get(&12).unwrap());
        assert_eq!((&(10..34), &-14), map.get(&10).unwrap());
        assert_eq!((&(10..34), &mut -14), map.get_mut(&10).unwrap());
        assert!(map.get(&9).is_none());
        assert_eq!((&(10..34), &-14), map.get(&33).unwrap());
        assert!(map.get(&34).is_none());
    }

    #[test]
    fn test_iter() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(10..34, -14);
        map.insert(74..92, -12);

        let mut iter = map.iter();

        assert_eq!(iter.next().expect("missing elem"), (&(10..34), &-14));
        assert_eq!(iter.next().expect("missing elem"), (&(74..92), &-12));

        assert!(iter.next().is_none());

        let mut iter = map.iter_starting_at(&10);
        assert_eq!(iter.next().expect("missing elem"), (&(10..34), &-14));
        let mut iter = map.iter_starting_at(&11);
        assert_eq!(iter.next().expect("missing elem"), (&(74..92), &-12));
        let mut iter = map.iter_starting_at(&74);
        assert_eq!(iter.next().expect("missing elem"), (&(74..92), &-12));
        let mut iter = map.iter_starting_at(&75);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_remove_overlapping_edge() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(10..34, -14);

        map.remove(&(2..11));
        assert_eq!((&(11..34), &-14), map.get(&11).unwrap());

        map.remove(&(33..42));
        assert_eq!((&(11..33), &-14), map.get(&12).unwrap());
    }

    #[test]
    fn test_remove_middle_splits_range() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(10..34, -14);
        map.remove(&(15..18));

        assert_eq!((&(10..15), &-14), map.get(&12).unwrap());
        assert_eq!((&(18..34), &-14), map.get(&20).unwrap());
    }

    #[test]
    fn test_remove_upper_half_of_split_range_leaves_lower_range() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(10..34, -14);
        map.remove(&(15..18));
        map.insert(2..7, -21);
        map.remove(&(20..42));

        assert_eq!((&(2..7), &-21), map.get(&5).unwrap());
        assert_eq!((&(10..15), &-14), map.get(&12).unwrap());
    }

    #[test]
    fn test_range_map_overlapping_insert() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(2..7, -21);
        map.insert(5..9, -42);
        map.insert(1..3, -43);
        map.insert(6..8, -44);

        assert_eq!((&(1..3), &-43), map.get(&2).unwrap());
        assert_eq!((&(3..5), &-21), map.get(&4).unwrap());
        assert_eq!((&(5..6), &-42), map.get(&5).unwrap());
        assert_eq!((&(6..8), &-44), map.get(&7).unwrap());
    }

    #[test]
    fn test_intersect_single() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(2..7, -10);

        let mut iter = map.intersection(3..4);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), None);

        let mut iter = map.intersection(2..3);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), None);

        let mut iter = map.intersection(1..4);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), None);

        let mut iter = map.intersection(1..2);
        assert_eq!(iter.next(), None);

        let mut iter = map.intersection(6..7);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_intersect_multiple() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(2..7, -10);
        map.insert(7..9, -20);
        map.insert(10..11, -30);

        let mut iter = map.intersection(3..8);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), Some((&(7..9), &-20)));
        assert_eq!(iter.next(), None);

        let mut iter = map.intersection(3..11);
        assert_eq!(iter.next(), Some((&(2..7), &-10)));
        assert_eq!(iter.next(), Some((&(7..9), &-20)));
        assert_eq!(iter.next(), Some((&(10..11), &-30)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_intersect_no_gaps() {
        let mut map = RangeMap::<u32, i32>::new();

        map.insert(0..1, -10);
        map.insert(1..2, -20);
        map.insert(2..3, -30);

        let mut iter = map.intersection(0..3);
        assert_eq!(iter.next(), Some((&(0..1), &-10)));
        assert_eq!(iter.next(), Some((&(1..2), &-20)));
        assert_eq!(iter.next(), Some((&(2..3), &-30)));
        assert_eq!(iter.next(), None);
    }
}
