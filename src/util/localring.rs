//! A ring buffer whose elements are built once and then recycled forever.
//!
//! [`LocalRingBuffer`] pre-builds every slot at construction and never
//! creates or drops one again. Pushing does not take a value — it hands back
//! a `&mut` to the slot that just became current, for the caller to fill in
//! place. That is what lets an element own a heap buffer (a line, a chunk)
//! and keep it across the whole life of the ring, so a full ring allocates
//! nothing.
//!
//! It is `Local` in the sense of single-owner: everything goes through
//! `&mut self`, so there is no locking and no synchronisation. Sharing one
//! between tasks is the job of whatever wraps it.
//!
//! # Recycled, not cleared
//!
//! [`push`](LocalRingBuffer::push) returns the previous occupant of the slot
//! with its data intact — the ring cannot reset it, since `T` is any type.
//! Resetting is the caller's first move.
//!
//! ```ignore
//! let slot = ring.push();
//! slot.clear();
//! slot.fill_from(&bytes);
//! ```

/// A fixed-capacity ring of pre-built, recycled elements.
///
/// # Offsets
///
/// The two offsets count positions, not indices: `end_offset - start_offset`
/// is the length, which is what tells a full ring from an empty one when
/// both land on the same slot. They are kept normalised below twice the
/// capacity, so they never grow without bound and never wrap.
pub struct LocalRingBuffer<T> {
    items: Box<[T]>,
    start_offset: usize,
    end_offset: usize,
}

impl<T> LocalRingBuffer<T> {
    pub fn new(capacity: usize) -> Self
    where
        T: Default,
    {
        Self::new_with(capacity, T::default)
    }

    /// Build every slot up front with `f`.
    ///
    /// `f` is `FnMut` so a factory can carry state — handing each slot its
    /// index into a shared arena, for instance.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero. A ring with nowhere to put anything could not
    /// honour [`push`](LocalRingBuffer::push), which always returns a slot.
    pub fn new_with(capacity: usize, mut f: impl FnMut() -> T) -> Self {
        assert!(capacity > 0, "a LocalRingBuffer needs a non-zero capacity");
        let mut items = Vec::with_capacity(capacity);
        items.resize_with(capacity, &mut f);
        Self {
            items: items.into_boxed_slice(),
            start_offset: 0,
            end_offset: 0,
        }
    }

    /// Pops the element and give a mutable reference to mutate it
    ///
    /// Takes from the front, so this is the oldest element. The slot is not
    /// destroyed, only released: the reference is to storage the ring still
    /// owns and will hand out again on a later
    /// [`push`](LocalRingBuffer::push). Holding it keeps the ring borrowed,
    /// so that reuse cannot happen underneath the caller.
    pub fn pop(&mut self) -> Option<&mut T> {
        if self.is_empty() {
            return None;
        }
        let index = self.index_of(self.start_offset);
        self.start_offset += 1;
        self.normalize();
        Some(&mut self.items[index])
    }

    /// Push the element in the local buffer, if it is full, it moves the ring and return a reference to the next
    /// item
    ///
    /// The slot comes back holding whatever the element it displaced left
    /// there; see the [module docs](self) on resetting it.
    pub fn push(&mut self) -> &mut T {
        if self.len() == self.items.len() {
            // Full: the oldest element is the one being recycled.
            self.start_offset += 1;
        }
        let index = self.index_of(self.end_offset);
        self.end_offset += 1;
        self.normalize();
        &mut self.items[index]
    }

    /// Get the lenght of the buffer
    pub fn len(&self) -> usize {
        self.end_offset - self.start_offset
    }

    /// Whether the ring holds no elements.
    pub fn is_empty(&self) -> bool {
        self.start_offset == self.end_offset
    }

    /// How many elements the ring can hold, which is also how many were
    /// built at construction.
    pub fn capacity(&self) -> usize {
        self.items.len()
    }

    /// Iterator for the buffer
    ///
    /// Oldest first.
    pub fn iter(&self) -> LocalRingBufferIter<'_, T> {
        let (head, tail) = self.as_slices();
        LocalRingBufferIter {
            head: head.iter(),
            tail: tail.iter(),
        }
    }

    /// Mutable iterator for every item on the buffer
    ///
    /// Oldest first. Only the elements currently in the ring are visited;
    /// recycled slots waiting to be pushed again are not.
    pub fn iter_mut(&mut self) -> LocalRingBufferIterMut<'_, T> {
        let (head, tail) = self.as_mut_slices();
        LocalRingBufferIterMut {
            head: head.iter_mut(),
            tail: tail.iter_mut(),
        }
    }

    /// Physical slot holding the element at `offset`.
    fn index_of(&self, offset: usize) -> usize {
        offset % self.items.len()
    }

    /// Keep the offsets small.
    ///
    /// Subtracting the capacity from both leaves the length untouched and
    /// leaves every index unchanged, so this is invisible from the outside —
    /// it only stops the counters from ever reaching `usize::MAX`, where the
    /// modulo would jump and scramble the order.
    fn normalize(&mut self) {
        let capacity = self.items.len();
        if self.start_offset >= capacity {
            self.start_offset -= capacity;
            self.end_offset -= capacity;
        }
    }

    /// The contents as two slices, oldest first.
    ///
    /// Same shape and meaning as [`VecDeque::as_slices`]: a ring that has
    /// wrapped is split across the end of the array, so no single slice can
    /// cover it. The first slice runs from the oldest element to the end of
    /// the array, the second holds whatever wrapped; either may be empty.
    ///
    /// Useful for handing the whole window somewhere at once — a vectored
    /// write, or a renderer that wants slices rather than an iterator.
    ///
    /// [`VecDeque::as_slices`]: std::collections::VecDeque::as_slices
    pub fn as_slices(&self) -> (&[T], &[T]) {
        let start = self.index_of(self.start_offset);
        let (left, right) = self.items.split_at(start);
        let head = right.len().min(self.len());
        (&right[..head], &left[..self.len() - head])
    }

    /// [`as_slices`](Self::as_slices), mutably.
    ///
    /// Same shape as [`VecDeque::as_mut_slices`].
    ///
    /// [`VecDeque::as_mut_slices`]: std::collections::VecDeque::as_mut_slices
    pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T]) {
        let len = self.len();
        let start = self.index_of(self.start_offset);
        let (left, right) = self.items.split_at_mut(start);
        let head = right.len().min(len);
        (&mut right[..head], &mut left[..len - head])
    }
}

impl<'a, T> IntoIterator for &'a LocalRingBuffer<T> {
    type Item = &'a T;
    type IntoIter = LocalRingBufferIter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut LocalRingBuffer<T> {
    type Item = &'a mut T;
    type IntoIter = LocalRingBufferIterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

pub struct LocalRingBufferIter<'a, T> {
    head: std::slice::Iter<'a, T>,
    tail: std::slice::Iter<'a, T>,
}

impl<'a, T> Iterator for LocalRingBufferIter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        self.head.next().or_else(|| self.tail.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.head.len() + self.tail.len();
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for LocalRingBufferIter<'_, T> {}

pub struct LocalRingBufferIterMut<'a, T> {
    head: std::slice::IterMut<'a, T>,
    tail: std::slice::IterMut<'a, T>,
}

impl<'a, T> Iterator for LocalRingBufferIterMut<'a, T> {
    type Item = &'a mut T;
    fn next(&mut self) -> Option<Self::Item> {
        self.head.next().or_else(|| self.tail.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.head.len() + self.tail.len();
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for LocalRingBufferIterMut<'_, T> {}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::VecDeque;

    /// Push a value by filling the slot the ring hands back.
    fn push(ring: &mut LocalRingBuffer<u32>, value: u32) {
        *ring.push() = value;
    }

    fn collect(ring: &LocalRingBuffer<u32>) -> Vec<u32> {
        ring.iter().copied().collect()
    }

    #[test]
    fn starts_empty_with_every_slot_built() {
        let ring: LocalRingBuffer<u32> = LocalRingBuffer::new(4);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.capacity(), 4);
        assert!(ring.is_empty());
        assert_eq!(collect(&ring), Vec::<u32>::new());
    }

    #[test]
    fn new_with_builds_each_slot() {
        let mut n = 0;
        let ring = LocalRingBuffer::new_with(3, || {
            n += 1;
            String::with_capacity(64)
        });
        assert_eq!(ring.capacity(), 3);
        // Every slot exists up front, so nothing allocates later.
        assert!(ring.items.iter().all(|s| s.capacity() >= 64));
    }

    #[test]
    #[should_panic(expected = "non-zero capacity")]
    fn zero_capacity_is_rejected() {
        LocalRingBuffer::<u32>::new(0);
    }

    #[test]
    fn fills_up_to_capacity() {
        let mut ring = LocalRingBuffer::new(3);
        for i in 1..=3 {
            push(&mut ring, i);
            assert_eq!(ring.len(), i as usize);
        }
        assert_eq!(collect(&ring), [1, 2, 3]);
    }

    #[test]
    fn pushing_past_capacity_drops_the_oldest() {
        let mut ring = LocalRingBuffer::new(3);
        for i in 1..=6 {
            push(&mut ring, i);
            assert!(ring.len() <= 3, "the ring grew past its capacity");
        }
        assert_eq!(collect(&ring), [4, 5, 6]);
    }

    /// The contract that makes the ring allocation free: the slot handed
    /// back still holds what the displaced element left in it.
    #[test]
    fn push_hands_back_the_displaced_slot() {
        let mut ring = LocalRingBuffer::new(2);
        push(&mut ring, 10);
        push(&mut ring, 20);

        let recycled = ring.push();
        assert_eq!(*recycled, 10, "the slot was reset instead of recycled");
        *recycled = 30;
        assert_eq!(collect(&ring), [20, 30]);
    }

    /// The same, for a type where recycling is the whole point.
    #[test]
    fn a_recycled_slot_keeps_its_allocation() {
        let mut ring = LocalRingBuffer::new_with(2, || String::with_capacity(32));
        for text in ["primeiro", "segundo"] {
            let slot = ring.push();
            slot.clear();
            slot.push_str(text);
        }
        let addresses: Vec<*const u8> = ring.iter().map(|s| s.as_ptr()).collect();

        for text in ["terceiro", "quarto", "quinto"] {
            let slot = ring.push();
            slot.clear();
            slot.push_str(text);
        }
        let after: Vec<*const u8> = ring.iter().map(|s| s.as_ptr()).collect();
        assert_eq!(
            addresses.iter().collect::<std::collections::HashSet<_>>(),
            after.iter().collect::<std::collections::HashSet<_>>(),
            "the strings were reallocated instead of refilled"
        );
    }

    #[test]
    fn pop_takes_the_oldest() {
        let mut ring = LocalRingBuffer::new(3);
        for i in 1..=3 {
            push(&mut ring, i);
        }
        assert_eq!(ring.pop().copied(), Some(1));
        assert_eq!(ring.len(), 2);
        assert_eq!(collect(&ring), [2, 3]);
        assert_eq!(ring.pop().copied(), Some(2));
        assert_eq!(ring.pop().copied(), Some(3));
        assert_eq!(ring.pop().copied(), None);
        assert!(ring.is_empty());
    }

    #[test]
    fn pop_on_an_empty_ring_is_none() {
        let mut ring: LocalRingBuffer<u32> = LocalRingBuffer::new(4);
        assert!(ring.pop().is_none());
    }

    /// Emptying and refilling has to keep working after the offsets have
    /// moved off zero.
    #[test]
    fn drains_and_refills() {
        let mut ring = LocalRingBuffer::new(3);
        for round in 0..4u32 {
            for i in 0..3 {
                push(&mut ring, round * 10 + i);
            }
            assert_eq!(collect(&ring), [round * 10, round * 10 + 1, round * 10 + 2]);
            while ring.pop().is_some() {}
            assert!(ring.is_empty());
        }
    }

    /// Iteration is oldest first, including once the contents wrap around
    /// the end of the backing array.
    #[test]
    fn iterates_in_order_across_the_wrap() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=4 {
            push(&mut ring, i);
        }
        assert_eq!(collect(&ring), [1, 2, 3, 4]);

        // Now the contents straddle the end of the array.
        push(&mut ring, 5);
        push(&mut ring, 6);
        assert_eq!(collect(&ring), [3, 4, 5, 6]);
    }

    #[test]
    fn iter_mut_edits_in_place_and_in_order() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        assert_eq!(collect(&ring), [3, 4, 5, 6]);

        let mut seen = Vec::new();
        for item in ring.iter_mut() {
            seen.push(*item);
            *item *= 10;
        }
        assert_eq!(seen, [3, 4, 5, 6], "iter_mut visited out of order");
        assert_eq!(collect(&ring), [30, 40, 50, 60]);
    }

    /// Slots that are not currently occupied must not show up.
    #[test]
    fn iteration_skips_unoccupied_slots() {
        let mut ring = LocalRingBuffer::new(8);
        push(&mut ring, 1);
        push(&mut ring, 2);
        assert_eq!(ring.iter().count(), 2);
        assert_eq!(ring.iter_mut().count(), 2);
    }

    /// The slice pair matches iteration, and shows the split exactly where
    /// the contents wrap.
    #[test]
    fn as_slices_matches_iteration() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=3 {
            push(&mut ring, i);
        }
        // Nothing has wrapped yet: it all fits in the first slice.
        assert_eq!(ring.as_slices(), (&[1u32, 2, 3][..], &[][..]));

        for i in 4..=6 {
            push(&mut ring, i);
        }
        // Now it straddles the end of the array.
        let (head, tail) = ring.as_slices();
        assert_eq!((head, tail), (&[3u32, 4][..], &[5u32, 6][..]));
        assert_eq!(
            [head, tail].concat(),
            collect(&ring),
            "the slices disagree with the iterator"
        );
    }

    #[test]
    fn as_mut_slices_edits_in_place() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        let (head, tail) = ring.as_mut_slices();
        for item in head.iter_mut().chain(tail) {
            *item *= 10;
        }
        assert_eq!(collect(&ring), [30, 40, 50, 60]);
    }

    #[test]
    fn as_slices_on_an_empty_ring_are_both_empty() {
        let ring: LocalRingBuffer<u32> = LocalRingBuffer::new(4);
        assert_eq!(ring.as_slices(), (&[][..], &[][..]));
    }

    #[test]
    fn iterators_report_their_length() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        assert_eq!(ring.iter().len(), 4);
        assert_eq!(ring.iter().size_hint(), (4, Some(4)));
        assert_eq!(ring.iter_mut().len(), 4);
    }

    #[test]
    fn borrows_can_be_iterated_directly() {
        let mut ring = LocalRingBuffer::new(3);
        push(&mut ring, 1);
        push(&mut ring, 2);
        assert_eq!((&ring).into_iter().copied().collect::<Vec<_>>(), [1, 2]);
        for item in &mut ring {
            *item += 1;
        }
        assert_eq!(collect(&ring), [2, 3]);
    }

    /// The offsets are normalised, so they stay small no matter how long
    /// the ring runs. Without that they would eventually reach `usize::MAX`
    /// and the modulo would scramble the order.
    #[test]
    fn offsets_stay_bounded() {
        let mut ring = LocalRingBuffer::new(3);
        for i in 0..10_000 {
            push(&mut ring, i);
            assert!(
                ring.end_offset < 2 * ring.capacity(),
                "offsets grew without bound: {} after {i} pushes",
                ring.end_offset
            );
        }
        assert_eq!(collect(&ring), [9997, 9998, 9999]);
    }

    /// Compare against a plain `VecDeque` driven with the same policy, over
    /// a long mixed run of pushes and pops.
    #[test]
    fn matches_a_vecdeque_model() {
        for capacity in 1..=6usize {
            let mut ring = LocalRingBuffer::new(capacity);
            let mut model: VecDeque<u32> = VecDeque::new();

            // A small xorshift, so a failure is reproducible.
            let mut state = 0x2545_f491_4f6c_dd1du64;
            for value in 0..5_000u32 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;

                if state.is_multiple_of(3) {
                    assert_eq!(
                        ring.pop().copied(),
                        model.pop_front(),
                        "pop diverged at {value} with capacity {capacity}"
                    );
                } else {
                    push(&mut ring, value);
                    if model.len() == capacity {
                        model.pop_front();
                    }
                    model.push_back(value);
                }

                assert_eq!(ring.len(), model.len(), "length diverged");
                assert_eq!(
                    collect(&ring),
                    model.iter().copied().collect::<Vec<_>>(),
                    "contents diverged at {value} with capacity {capacity}"
                );
            }
        }
    }
}
