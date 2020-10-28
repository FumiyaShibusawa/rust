//! Implements a map from integer indices to data.
//! Rather than storing data for every index, internally, this maps entire ranges to the data.
//! To this end, the APIs all work on ranges, not on individual integers. Ranges are split as
//! necessary (e.g., when [0,5) is first associated with X, and then [1,2) is mutated).
//! Users must not depend on whether a range is coalesced or not, even though this is observable
//! via the iteration APIs.

use std::ops;

use rustc_target::abi::Size;

#[derive(Clone, Debug)]
struct Elem<T> {
    /// The range covered by this element; never empty.
    range: ops::Range<u64>,
    /// The data stored for this element.
    data: T,
}
#[derive(Clone, Debug)]
pub struct RangeMap<T> {
    v: Vec<Elem<T>>,
}

impl<T> RangeMap<T> {
    /// Creates a new `RangeMap` for the given size, and with the given initial value used for
    /// the entire range.
    #[inline(always)]
    pub fn new(size: Size, init: T) -> RangeMap<T> {
        let size = size.bytes();
        let mut map = RangeMap { v: Vec::new() };
        if size > 0 {
            map.v.push(Elem { range: 0..size, data: init });
        }
        map
    }

    /// Finds the index containing the given offset.
    fn find_offset(&self, offset: u64) -> usize {
        // We do a binary search.
        let mut left = 0usize; // inclusive
        let mut right = self.v.len(); // exclusive
        loop {
            debug_assert!(left < right, "find_offset: offset {} is out-of-bounds", offset);
            let candidate = left.checked_add(right).unwrap() / 2;
            let elem = &self.v[candidate];
            if offset < elem.range.start {
                // We are too far right (offset is further left).
                debug_assert!(candidate < right); // we are making progress
                right = candidate;
            } else if offset >= elem.range.end {
                // We are too far left (offset is further right).
                debug_assert!(candidate >= left); // we are making progress
                left = candidate + 1;
            } else {
                // This is it!
                return candidate;
            }
        }
    }

    /// Provides read-only iteration over everything in the given range. This does
    /// *not* split items if they overlap with the edges. Do not use this to mutate
    /// through interior mutability.
    ///
    /// The iterator also provides the offset of the given element.
    pub fn iter<'a>(&'a self, offset: Size, len: Size) -> impl Iterator<Item = (Size, &'a T)> + 'a {
        let offset = offset.bytes();
        let len = len.bytes();
        // Compute a slice starting with the elements we care about.
        let slice: &[Elem<T>] = if len == 0 {
            // We just need any empty iterator. We don't even want to
            // yield the element that surrounds this position.
            &[]
        } else {
            let first_idx = self.find_offset(offset);
            &self.v[first_idx..]
        };
        // The first offset that is not included any more.
        let end = offset + len;
        slice.iter().take_while(move |elem| elem.range.start < end).map(|elem| (Size::from_bytes(elem.range.start), &elem.data))
    }

    pub fn iter_mut_all<'a>(&'a mut self) -> impl Iterator<Item = &'a mut T> + 'a {
        self.v.iter_mut().map(|elem| &mut elem.data)
    }

    // Splits the element situated at the given `index`, such that the 2nd one starts at offset
    // `split_offset`. Do nothing if the element already starts there.
    // Returns whether a split was necessary.
    fn split_index(&mut self, index: usize, split_offset: u64) -> bool
    where
        T: Clone,
    {
        let elem = &mut self.v[index];
        if split_offset == elem.range.start || split_offset == elem.range.end {
            // Nothing to do.
            return false;
        }
        debug_assert!(
            elem.range.contains(&split_offset),
            "the `split_offset` is not in the element to be split"
        );

        // Now we really have to split. Reduce length of first element.
        let second_range = split_offset..elem.range.end;
        elem.range.end = split_offset;
        // Copy the data, and insert second element.
        let second = Elem { range: second_range, data: elem.data.clone() };
        self.v.insert(index + 1, second);
        return true;
    }

    /// Provides mutable iteration over everything in the given range. As a side-effect,
    /// this will split entries in the map that are only partially hit by the given range,
    /// to make sure that when they are mutated, the effect is constrained to the given range.
    /// Moreover, this will opportunistically merge neighbouring equal blocks.
    ///
    /// The iterator also provides the offset of the given element.
    pub fn iter_mut<'a>(
        &'a mut self,
        offset: Size,
        len: Size,
    ) -> impl Iterator<Item = (Size, &'a mut T)> + 'a
    where
        T: Clone + PartialEq,
    {
        let offset = offset.bytes();
        let len = len.bytes();
        // Compute a slice containing exactly the elements we care about
        let slice: &mut [Elem<T>] = if len == 0 {
            // We just need any empty iterator. We don't even want to
            // yield the element that surrounds this position, nor do
            // any splitting.
            &mut []
        } else {
            // Make sure we got a clear beginning
            let mut first_idx = self.find_offset(offset);
            if self.split_index(first_idx, offset) {
                // The newly created 2nd element is ours
                first_idx += 1;
            }
            // No more mutation.
            let first_idx = first_idx;
            // Find our end. Linear scan, but that's ok because the iteration
            // is doing the same linear scan anyway -- no increase in complexity.
            // We combine this scan with a scan for duplicates that we can merge, to reduce
            // the number of elements.
            // We stop searching after the first "block" of size 1, to avoid spending excessive
            // amounts of time on the merging.
            let mut equal_since_idx = first_idx;
            // Once we see too many non-mergeable blocks, we stop.
            // The initial value is chosen via... magic. Benchmarking and magic.
            let mut successful_merge_count = 3usize;
            // When the loop is done, this is the first excluded element.
            let mut end_idx = first_idx;
            loop {
                // Compute if `end` is the last element we need to look at.
                let done = self.v[end_idx].range.end >= offset + len;
                // We definitely need to include `end`, so move the index.
                end_idx += 1;
                debug_assert!(
                    done || end_idx < self.v.len(),
                    "iter_mut: end-offset {} is out-of-bounds",
                    offset + len
                );
                // see if we want to merge everything in `equal_since..end` (exclusive at the end!)
                if successful_merge_count > 0 {
                    if done || self.v[end_idx].data != self.v[equal_since_idx].data {
                        // Everything in `equal_since..end` was equal. Make them just one element covering
                        // the entire range.
                        let removed_elems = end_idx - equal_since_idx - 1; // number of elements that we would remove
                        if removed_elems > 0 {
                            // Adjust the range of the first element to cover all of them.
                            let equal_until = self.v[end_idx - 1].range.end; // end of range of last of the equal elements
                            self.v[equal_since_idx].range.end = equal_until;
                            // Delete the rest of them.
                            self.v.splice(equal_since_idx + 1..end_idx, std::iter::empty());
                            // Adjust `end_idx` because we made the list shorter.
                            end_idx -= removed_elems;
                            // Adjust the count for the cutoff.
                            successful_merge_count += removed_elems;
                        } else {
                            // Adjust the count for the cutoff.
                            successful_merge_count -= 1;
                        }
                        // Go on scanning for the next block starting here.
                        equal_since_idx = end_idx;
                    }
                }
                // Leave loop if this is the last element.
                if done {
                    break;
                }
            }
            // Move to last included instead of first excluded index.
            let end_idx = end_idx - 1;
            // We need to split the end as well. Even if this performs a
            // split, we don't have to adjust our index as we only care about
            // the first part of the split.
            self.split_index(end_idx, offset + len);
            // Now we yield the slice. `end` is inclusive.
            &mut self.v[first_idx..=end_idx]
        };
        slice.iter_mut().map(|elem| (Size::from_bytes(elem.range.start), &mut elem.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Query the map at every offset in the range and collect the results.
    fn to_vec<T: Copy>(map: &RangeMap<T>, offset: u64, len: u64) -> Vec<T> {
        (offset..offset + len)
            .into_iter()
            .map(|i| map.iter(Size::from_bytes(i), Size::from_bytes(1)).next().map(|(_, &t)| t).unwrap())
            .collect()
    }

    #[test]
    fn basic_insert() {
        let mut map = RangeMap::<i32>::new(Size::from_bytes(20), -1);
        // Insert.
        for (_, x) in map.iter_mut(Size::from_bytes(10), Size::from_bytes(1)) {
            *x = 42;
        }
        // Check.
        assert_eq!(to_vec(&map, 10, 1), vec![42]);
        assert_eq!(map.v.len(), 3);

        // Insert with size 0.
        for (_, x) in map.iter_mut(Size::from_bytes(10), Size::from_bytes(0)) {
            *x = 19;
        }
        for (_, x) in map.iter_mut(Size::from_bytes(11), Size::from_bytes(0)) {
            *x = 19;
        }
        assert_eq!(to_vec(&map, 10, 2), vec![42, -1]);
        assert_eq!(map.v.len(), 3);
    }

    #[test]
    fn gaps() {
        let mut map = RangeMap::<i32>::new(Size::from_bytes(20), -1);
        for (_, x) in map.iter_mut(Size::from_bytes(11), Size::from_bytes(1)) {
            *x = 42;
        }
        for (_, x) in map.iter_mut(Size::from_bytes(15), Size::from_bytes(1)) {
            *x = 43;
        }
        assert_eq!(map.v.len(), 5);
        assert_eq!(to_vec(&map, 10, 10), vec![-1, 42, -1, -1, -1, 43, -1, -1, -1, -1]);

        for (_, x) in map.iter_mut(Size::from_bytes(10), Size::from_bytes(10)) {
            if *x < 42 {
                *x = 23;
            }
        }
        assert_eq!(map.v.len(), 6);
        assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 43, 23, 23, 23, 23]);
        assert_eq!(to_vec(&map, 13, 5), vec![23, 23, 43, 23, 23]);

        for (_, x) in map.iter_mut(Size::from_bytes(15), Size::from_bytes(5)) {
            *x = 19;
        }
        assert_eq!(map.v.len(), 6);
        assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
        // Should be seeing two blocks with 19.
        assert_eq!(
            map.iter(Size::from_bytes(15), Size::from_bytes(2)).map(|(_, &t)| t).collect::<Vec<_>>(),
            vec![19, 19]
        );

        // A NOP `iter_mut` should trigger merging.
        for _ in map.iter_mut(Size::from_bytes(15), Size::from_bytes(5)) {}
        assert_eq!(map.v.len(), 5);
        assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
    }
}
