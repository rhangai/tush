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
//! # A capacity that moves
//!
//! The slots are built once and never again, but the capacity — how many of
//! them the window may use — is
//! [`set_capacity`](LocalRingBuffer::set_capacity)'s to change: a ring can be
//! made shorter and longer again without building, dropping or reallocating
//! anything, up to the [`max_capacity`](LocalRingBuffer::max_capacity) it was
//! built with. That is for an owner that has to stand in for several sizes
//! over its life and cannot pay an allocation each time it changes.
//!
//! # Recycled, not cleared
//!
//! [`push`](LocalRingBuffer::push) hands the slot back with whatever was in
//! it still there — the ring cannot reset it, since `T` is any type.
//! Resetting is the caller's first move.
//!
//! ```ignore
//! let slot = ring.push();
//! slot.clear();
//! slot.fill_from(&bytes);
//! ```

/// A ring of pre-built, recycled elements, as long as its slots or shorter.
///
/// # Offsets
///
/// The two offsets count positions, not indices: `end_offset - start_offset`
/// is the length, which is what tells a full ring from an empty one when
/// both land on the same slot. They are kept normalised below twice
/// [`max_capacity`](LocalRingBuffer::max_capacity) — the slots, not the
/// capacity, since that is what they are turned into indices against — so
/// they never grow without bound and never wrap.
pub struct LocalRingBuffer<T> {
    /// Every slot, built at construction and never created or dropped again.
    /// A slot outside the current window still holds whatever the element
    /// that left it did — that is what makes a push allocation free.
    items: Box<[T]>,
    /// Position of the oldest element. A position, not an index: it is turned
    /// into one by [`index_of`](LocalRingBuffer::index_of).
    start_offset: usize,
    /// Position one past the newest. The gap to `start_offset` is the length,
    /// which is what tells a full ring from an empty one when both land on
    /// the same slot.
    end_offset: usize,
    /// How many slots the window may use, never above `items.len()`.
    ///
    /// The capacity in every sense but the allocation: a ring of a thousand
    /// slots held at a hundred *is* a ring of a hundred, and the other nine
    /// hundred only mean that growing back costs nothing.
    capacity: usize,
}

impl<T> LocalRingBuffer<T> {
    /// A ring of `max_capacity` slots, every one already built.
    ///
    /// It starts at that capacity too; [`set_capacity`](Self::set_capacity)
    /// is what holds it under.
    pub fn new(max_capacity: usize) -> Self
    where
        T: Default,
    {
        Self::new_with(max_capacity, T::default)
    }

    /// Build every slot up front with `f`.
    ///
    /// `f` is `FnMut` so a factory can carry state — handing each slot its
    /// index into a shared arena, for instance.
    ///
    /// # Panics
    ///
    /// If `max_capacity` is zero. A ring with nowhere to put anything could
    /// not honour [`push`](LocalRingBuffer::push), which always returns a
    /// slot.
    pub fn new_with(max_capacity: usize, mut f: impl FnMut() -> T) -> Self {
        assert!(
            max_capacity > 0,
            "a LocalRingBuffer needs a non-zero capacity"
        );
        let mut items = Vec::with_capacity(max_capacity);
        items.resize_with(max_capacity, &mut f);
        Self {
            capacity: items.len(),
            items: items.into_boxed_slice(),
            start_offset: 0,
            end_offset: 0,
        }
    }

    /// Take the next slot, dropping the oldest element if the ring is full.
    ///
    /// What comes back is a slot and not an empty one: under a reduced
    /// capacity it holds neither the element just dropped nor nothing, but
    /// whatever sat there a whole lap of the slots ago. Resetting it is the
    /// caller's first move — see the [module docs](self).
    pub fn push(&mut self) -> &mut T {
        if self.len() == self.capacity {
            // Full: the oldest element is the one being recycled. Full short
            // of the slots, so the one handed back is not the element just
            // evicted but whatever sat there a whole lap ago — the same to a
            // caller that overwrites it.
            self.start_offset += 1;
        }
        let index = self.index_of(self.end_offset);
        self.end_offset += 1;
        self.normalize();
        &mut self.items[index]
    }

    /// Hold the ring to `capacity` elements from now on.
    ///
    /// Lowering it drops the oldest down to the new length, which is what
    /// pushing past it would have done anyway. Raising it loses nothing and
    /// allocates nothing: the slots were always there, so the window simply
    /// has further to grow.
    ///
    /// Asking for more than there are slots gives the slots — a caller
    /// growing a ring back is asking for room and the answer is however much
    /// there is, which it can check against
    /// [`max_capacity`](Self::max_capacity) if it needs a bigger ring than
    /// this one. Zero gives one, for the reason
    /// [`new_with`](Self::new_with) refuses it: a ring with nowhere to put
    /// anything could not honour [`push`](Self::push).
    pub fn set_capacity(&mut self, capacity: usize) {
        let capacity = capacity.clamp(1, self.items.len());
        self.capacity = capacity;
        if self.len() > capacity {
            self.start_offset = self.end_offset - capacity;
            self.normalize();
        }
    }

    /// Drop everything, keeping every slot.
    ///
    /// For an owner reusing a ring for something unrelated to what was in it:
    /// the elements stay where they are, unread and waiting to be overwritten
    /// by a push, which is what makes emptying free.
    pub fn clear(&mut self) {
        self.start_offset = self.end_offset;
    }

    /// How many elements are in the window, which is never past
    /// [`capacity`](Self::capacity).
    pub fn len(&self) -> usize {
        self.end_offset - self.start_offset
    }

    /// Whether the ring holds no elements.
    pub fn is_empty(&self) -> bool {
        self.start_offset == self.end_offset
    }

    /// How many elements the ring can hold.
    ///
    /// What [`set_capacity`](Self::set_capacity) last said, which is what the
    /// ring behaves as; [`max_capacity`](Self::max_capacity) is the
    /// allocation behind it.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many slots were built at construction.
    ///
    /// The ceiling [`set_capacity`](Self::set_capacity) clamps to,
    /// and so what a caller asks before deciding whether this ring can stand
    /// in for the size it needs or has to be replaced.
    pub fn max_capacity(&self) -> usize {
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

/// Oldest to newest, in two slices because the contents may wrap.
pub struct LocalRingBufferIter<'a, T> {
    /// From the oldest element to the end of the array.
    head: std::slice::Iter<'a, T>,
    /// Whatever wrapped past the end, or nothing if the contents do not.
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

/// The same, borrowing each element mutably.
pub struct LocalRingBufferIterMut<'a, T> {
    /// From the oldest element to the end of the array.
    head: std::slice::IterMut<'a, T>,
    /// Whatever wrapped past the end, or nothing if the contents do not.
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

    /// Held short, a ring is short — the slots past the capacity are
    /// not a longer ring, they are room to grow back into.
    #[test]
    fn a_set_capacity_is_the_capacity() {
        let mut ring = LocalRingBuffer::new(8);
        ring.set_capacity(3);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        assert_eq!(collect(&ring), [4, 5, 6]);
        assert_eq!(ring.capacity(), 3);
        assert_eq!(ring.max_capacity(), 8, "the slots are all still there");
    }

    #[test]
    fn raising_it_loses_nothing() {
        let mut ring = LocalRingBuffer::new(8);
        ring.set_capacity(2);
        for i in 1..=3 {
            push(&mut ring, i);
        }
        assert_eq!(collect(&ring), [2, 3]);

        ring.set_capacity(4);
        push(&mut ring, 4);
        push(&mut ring, 5);
        assert_eq!(collect(&ring), [2, 3, 4, 5]);
    }

    /// Lowering it does what pushing past it would have done, and at once:
    /// the ring behaves as its new capacity from the call, not from the next
    /// push.
    #[test]
    fn lowering_it_drops_the_oldest() {
        let mut ring = LocalRingBuffer::new(8);
        for i in 1..=5 {
            push(&mut ring, i);
        }
        ring.set_capacity(2);
        assert_eq!(collect(&ring), [4, 5]);
        push(&mut ring, 6);
        assert_eq!(collect(&ring), [5, 6]);
    }

    #[test]
    fn clearing_keeps_the_slots_and_the_order() {
        let mut ring = LocalRingBuffer::new(4);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.max_capacity(), 4, "the slots are all still there");

        for i in 7..=9 {
            push(&mut ring, i);
        }
        assert_eq!(
            collect(&ring),
            [7, 8, 9],
            "it kept counting from where it was"
        );
    }

    /// The window moves over the slots and the slots do not move, so a ring
    /// held shorter than its array wraps at the array and is still in order.
    #[test]
    fn a_shortened_ring_wraps_in_order() {
        let mut ring = LocalRingBuffer::new(4);
        ring.set_capacity(3);
        for i in 0..20 {
            push(&mut ring, i);
            let held = collect(&ring);
            let from = (i + 1).saturating_sub(3);
            assert_eq!(held, (from..=i).collect::<Vec<_>>(), "after {i} pushes");
        }
    }

    /// Past the slots is the slots: the ring cannot lend out memory it never
    /// built, and a caller growing one back wants what there is.
    #[test]
    fn a_capacity_past_the_slots_is_the_slots() {
        let mut ring = LocalRingBuffer::new(4);
        ring.set_capacity(9);
        assert_eq!(ring.capacity(), 4);
        for i in 1..=6 {
            push(&mut ring, i);
        }
        assert_eq!(collect(&ring), [3, 4, 5, 6]);
    }

    #[test]
    fn a_zero_capacity_is_one() {
        let mut ring = LocalRingBuffer::new(4);
        ring.set_capacity(0);
        assert_eq!(ring.capacity(), 1);
        push(&mut ring, 1);
        push(&mut ring, 2);
        assert_eq!(collect(&ring), [2]);
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
                ring.end_offset < 2 * ring.max_capacity(),
                "offsets grew without bound: {} after {i} pushes",
                ring.end_offset
            );
        }
        assert_eq!(collect(&ring), [9997, 9998, 9999]);
    }
}
