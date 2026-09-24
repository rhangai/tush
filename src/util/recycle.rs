//! A list emptied and refilled over and over, keeping its elements' buffers.
//!
//! [`RecycleVec`] does not drop the elements it builds: emptying it resets
//! each one in place, so a caller refilling it with a screenful of lines
//! every frame writes into the same `String`s instead of freeing and
//! rebuilding them. Pushing takes no value for the same reason — it hands
//! back the next slot for the caller to fill.
//!
//! [`LocalRingBuffer`](crate::util::localring::LocalRingBuffer) is the other
//! half of that idea and the one to reach for when the buffer is a queue: it
//! is bounded and evicts the oldest to make room, where this one grows and is
//! refilled from zero. They differ on who resets a slot, too — the ring hands
//! one back as it was left, and this one recycles on the way out, so a push
//! here always finds a clean element.

/// A vec of recycled elements, as long as the slots it has built or shorter.
///
/// The elements past [`len`](RecycleVec::len) are not gone: they are built,
/// already recycled, and waiting for the next [`push`](RecycleVec::push).
/// Two lengths, and only the live one moves as the vec is filled and
/// emptied.
pub struct RecycleVec<T> {
    /// Every slot built so far, live or waiting. Its length is the high
    /// water mark: slots are built as pushes need them and never dropped.
    vec: Vec<T>,
    /// How many of them are live. Never past `vec.len()`.
    len: usize,
}

impl<T: Recyclable> RecycleVec<T> {
    /// Empty, with nothing built yet.
    pub fn new() -> Self {
        Self {
            vec: Vec::new(),
            len: 0,
        }
    }

    /// Build `capacity` slots up front, for a caller that knows how many it
    /// will fill and would rather not grow into it.
    pub fn with_capacity(capacity: usize) -> Self {
        let mut vec = Self {
            vec: Vec::with_capacity(capacity),
            len: 0,
        };
        vec.reserve(capacity);
        vec
    }

    /// How many elements are live, which is not how many slots are built.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is live. The slots behind it may still be there.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build slots ahead of the pushes that will use them, up to `len` of
    /// them in all.
    pub fn reserve(&mut self, len: usize) {
        if len > self.len {
            self.vec.resize_with(len, <T as Recyclable>::new_element);
        }
    }

    /// Empty it, recycling every live element.
    ///
    /// The slots stay built and keep their buffers, so the next refill writes
    /// into them rather than building them again. That is paid for here: this
    /// walks every live element, where dropping the lot would not.
    pub fn clear(&mut self) {
        for item in self.as_mut_slice() {
            item.recycle();
        }
        self.len = 0;
    }

    /// The live elements in push order. The recycled slots behind them are
    /// not part of it.
    pub fn as_slice(&self) -> &[T] {
        &self.vec[0..self.len]
    }

    /// The live elements, to write into. Same window as
    /// [`as_slice`](RecycleVec::as_slice).
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.vec[0..self.len]
    }

    /// The next slot, recycled and ready to fill, building one if none is
    /// waiting.
    ///
    /// Takes no value: a `T` handed in would be a `T` built somewhere else,
    /// which is the allocation this type exists to avoid.
    pub fn push(&mut self) -> &mut T {
        if self.len == self.vec.len() {
            let factor = (self.len / 2).max(1);
            self.reserve(self.len + factor);
        }
        let index = self.len;
        self.len += 1;
        &mut self.vec[index]
    }

    /// Drop the last element, recycling it, and do nothing when there is
    /// none.
    ///
    /// Nothing comes back: it is reset on the way out, so there would be
    /// nothing left to hand over.
    pub fn pop(&mut self) {
        if self.len > 0 {
            self.len -= 1;
            self.vec[self.len].recycle();
        }
    }
}

/// How an element is built and reset, for a [`RecycleVec`] to hold it.
///
/// A factory on the trait rather than a closure taken at construction, the
/// way [`LocalRingBuffer::new_with`](crate::util::localring::LocalRingBuffer::new_with)
/// takes one: a `RecycleVec` grows, so it has to build an element long after
/// it was made.
pub trait Recyclable {
    /// A fresh element, for a slot that has to be built.
    ///
    /// Separate from [`Default`] so a type with no sensible default, or one
    /// wanting its slots sized for the job, can still be held here.
    fn new_element() -> Self;

    /// Reset for reuse, keeping whatever the element has already allocated —
    /// a clear and not a replacement, which is the whole saving.
    fn recycle(&mut self);
}

impl Recyclable for String {
    fn new_element() -> Self {
        String::new()
    }

    fn recycle(&mut self) {
        self.clear()
    }
}

impl<T: Recyclable> Recyclable for RecycleVec<T> {
    fn new_element() -> Self {
        Self::new()
    }

    fn recycle(&mut self) {
        self.clear()
    }
}
