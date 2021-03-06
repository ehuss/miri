#![allow(unused)]

//! Implements a map from integer indices to data.
//! Rather than storing data for every index, internally, this maps entire ranges to the data.
//! To this end, the APIs all work on ranges, not on individual integers. Ranges are split as
//! necessary (e.g. when [0,5) is first associated with X, and then [1,2) is mutated).
//! Users must not depend on whether a range is coalesced or not, even though this is observable
//! via the iteration APIs.
use std::collections::BTreeMap;
use std::ops;

use rustc::ty::layout::Size;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeMap<T> {
    map: BTreeMap<Range, T>,
}

// The derived `Ord` impl sorts first by the first field, then, if the fields are the same,
// by the second field.
// This is exactly what we need for our purposes, since a range query on a BTReeSet/BTreeMap will give us all
// `MemoryRange`s whose `start` is <= than the one we're looking for, but not > the end of the range we're checking.
// At the same time the `end` is irrelevant for the sorting and range searching, but used for the check.
// This kind of search breaks, if `end < start`, so don't do that!
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
struct Range {
    start: u64,
    end: u64, // Invariant: end > start
}

impl Range {
    /// Compute a range of ranges that contains all ranges overlaping with [offset, offset+len)
    fn range(offset: u64, len: u64) -> ops::Range<Range> {
        if len == 0 {
            // We can produce an empty range, nothing overlaps with this.
            let r = Range { start: 0, end: 1 };
            return r..r;
        }
        // We select all elements that are within
        // the range given by the offset into the allocation and the length.
        // This is sound if all ranges that intersect with the argument range, are in the
        // resulting range of ranges.
        let left = Range {
            // lowest range to include `offset`
            start: 0,
            end: offset + 1,
        };
        let right = Range {
            // lowest (valid) range not to include `offset+len`
            start: offset + len,
            end: offset + len + 1,
        };
        left..right
    }

    /// Tests if any element of [offset, offset+len) is contained in this range.
    #[inline(always)]
    fn overlaps(&self, offset: u64, len: u64) -> bool {
        if len == 0 {
            // `offset` totally does not matter, we cannot overlap with an empty interval
            false
        } else {
            offset < self.end && offset.checked_add(len).unwrap() >= self.start
        }
    }
}

impl<T> RangeMap<T> {
    /// Create a new RangeMap for the given size, and with the given initial value used for
    /// the entire range.
    #[inline(always)]
    pub fn new(size: Size, init: T) -> RangeMap<T> {
        let mut map = RangeMap { map: BTreeMap::new() };
        if size.bytes() > 0 {
            map.map.insert(Range { start: 0, end: size.bytes() }, init);
        }
        map
    }

    fn iter_with_range<'a>(
        &'a self,
        offset: u64,
        len: u64,
    ) -> impl Iterator<Item = (&'a Range, &'a T)> + 'a {
        self.map.range(Range::range(offset, len)).filter_map(
            move |(range, data)| {
                debug_assert!(len > 0);
                if range.overlaps(offset, len) {
                    Some((range, data))
                } else {
                    None
                }
            },
        )
    }

    /// Provide read-only iteration over everything in the given range.  This does
    /// *not* split items if they overlap with the edges.  Do not use this to mutate
    /// through interior mutability.
    pub fn iter<'a>(&'a self, offset: Size, len: Size) -> impl Iterator<Item = &'a T> + 'a {
        self.iter_with_range(offset.bytes(), len.bytes()).map(|(_, data)| data)
    }

    pub fn iter_mut_all<'a>(&'a mut self) -> impl Iterator<Item = &'a mut T> + 'a {
        self.map.values_mut()
    }

    fn split_entry_at(&mut self, offset: u64)
    where
        T: Clone,
    {
        let range = match self.iter_with_range(offset, 1).next() {
            Some((&range, _)) => range,
            None => return,
        };
        assert!(
            range.start <= offset && range.end > offset,
            "We got a range that doesn't even contain what we asked for."
        );
        // There is an entry overlapping this position, see if we have to split it
        if range.start < offset {
            let data = self.map.remove(&range).unwrap();
            let old = self.map.insert(
                Range {
                    start: range.start,
                    end: offset,
                },
                data.clone(),
            );
            assert!(old.is_none());
            let old = self.map.insert(
                Range {
                    start: offset,
                    end: range.end,
                },
                data,
            );
            assert!(old.is_none());
        }
    }

    /// Provide mutable iteration over everything in the given range.  As a side-effect,
    /// this will split entries in the map that are only partially hit by the given range,
    /// to make sure that when they are mutated, the effect is constrained to the given range.
    pub fn iter_mut<'a>(
        &'a mut self,
        offset: Size,
        len: Size,
    ) -> impl Iterator<Item = &'a mut T> + 'a
    where
        T: Clone,
    {
        let offset = offset.bytes();
        let len = len.bytes();

        if len > 0 {
            // Preparation: Split first and last entry as needed.
            self.split_entry_at(offset);
            self.split_entry_at(offset + len);
        }
        // Now we can provide a mutable iterator
        self.map.range_mut(Range::range(offset, len)).filter_map(
            move |(&range, data)| {
                debug_assert!(len > 0);
                if range.overlaps(offset, len) {
                    assert!(
                        offset <= range.start && offset + len >= range.end,
                        "The splitting went wrong"
                    );
                    Some(data)
                } else {
                    // Skip this one
                    None
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Query the map at every offset in the range and collect the results.
    fn to_vec<T: Copy>(map: &RangeMap<T>, offset: u64, len: u64) -> Vec<T> {
        (offset..offset + len)
            .into_iter()
            .map(|i| map
                .iter(Size::from_bytes(i), Size::from_bytes(1))
                .next()
                .map(|&t| t)
                .unwrap()
            )
            .collect()
    }

    #[test]
    fn basic_insert() {
        let mut map = RangeMap::<i32>::new(Size::from_bytes(20), -1);
        // Insert
        for x in map.iter_mut(Size::from_bytes(10), Size::from_bytes(1)) {
            *x = 42;
        }
        // Check
        assert_eq!(to_vec(&map, 10, 1), vec![42]);

        // Insert with size 0
        for x in map.iter_mut(Size::from_bytes(10), Size::from_bytes(0)) {
            *x = 19;
        }
        for x in map.iter_mut(Size::from_bytes(11), Size::from_bytes(0)) {
            *x = 19;
        }
        assert_eq!(to_vec(&map, 10, 2), vec![42, -1]);
    }

    #[test]
    fn gaps() {
        let mut map = RangeMap::<i32>::new(Size::from_bytes(20), -1);
        for x in map.iter_mut(Size::from_bytes(11), Size::from_bytes(1)) {
            *x = 42;
        }
        for x in map.iter_mut(Size::from_bytes(15), Size::from_bytes(1)) {
            *x = 43;
        }
        assert_eq!(
            to_vec(&map, 10, 10),
            vec![-1, 42, -1, -1, -1, 43, -1, -1, -1, -1]
        );

        for x in map.iter_mut(Size::from_bytes(10), Size::from_bytes(10)) {
            if *x < 42 {
                *x = 23;
            }
        }

        assert_eq!(
            to_vec(&map, 10, 10),
            vec![23, 42, 23, 23, 23, 43, 23, 23, 23, 23]
        );
        assert_eq!(to_vec(&map, 13, 5), vec![23, 23, 43, 23, 23]);

        // Now request a range that goes beyond the initial size
        for x in map.iter_mut(Size::from_bytes(15), Size::from_bytes(10)) {
            *x = 19;
        }
        assert_eq!(map.iter(Size::from_bytes(19), Size::from_bytes(1))
            .map(|&t| t).collect::<Vec<_>>(), vec![19]);
        assert_eq!(map.iter(Size::from_bytes(20), Size::from_bytes(1))
            .map(|&t| t).collect::<Vec<_>>(), vec![]);
    }
}
