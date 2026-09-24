//! A list of short lists sharing one run of items, for the places where a
//! `Vec<Vec<_>>` would pay a header and an allocation per row.

use std::{alloc::Layout, marker::PhantomData, mem::MaybeUninit, ptr::NonNull};

use replace_with::replace_with_or_abort;

/// Why a `try_` call refused.
///
/// Which of the two capacities ran out is the whole of what `grow` needs to
/// widen that one and leave the other alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JaggedVecError {
    /// A storage offered rows that do not sit inside the data it offered.
    InvalidConsumer,
    /// No room for another row.
    FullRows,
    /// No room for another item.
    FullData,
}

/// Rows of `T` laid end to end in one run of items, inline until they outgrow
/// `N` items or `R` rows and on the heap after that.
///
/// A row is a pair of offsets into that run rather than a `Vec` of its own,
/// because these rows are short and a vec each would be an allocation and a
/// pointer chase per row.
///
/// `N` and `R` are close to free until the inline variant reaches the size of
/// the heap one, since the enum is as large as its larger arm either way —
/// `StorageHeap` is 32 bytes, so an inline variant under that is paying for
/// space it does not use. There is no assert for it because a ZST can never
/// reach 32 bytes and would be locked out.
pub struct JaggedVec<T, const N: usize, const R: usize> {
    storage: Storage<T, N, R>,
}

impl<T, const N: usize, const R: usize> JaggedVec<T, N, R> {
    /// Empty and inline: nothing is allocated until the rows outgrow `N` or `R`.
    pub fn new() -> Self {
        Self::with_capacity(N, R)
    }

    /// Room for `capacity_data` items across `capacity_rows` rows, on the heap
    /// unless both fit inline.
    ///
    /// Panics past `u32::MAX`, which is the width the heap indexes at.
    pub fn with_capacity(capacity_data: usize, capacity_rows: usize) -> Self {
        Self {
            storage: Self::storage(as_capacity(capacity_data), as_capacity(capacity_rows)),
        }
    }

    /// Whether the rows have spilled out of the inline storage, which is a
    /// sizing question rather than a behavioural one — both arms hold the same
    /// rows.
    pub fn is_heap(&self) -> bool {
        matches!(&self.storage, Storage::Heap(_))
    }

    /// Whether [`Self::push_data`] has left items that no [`Self::commit_row`]
    /// has closed, which is what a caller checks to keep from committing a row
    /// it never started.
    pub fn is_row_open(&self) -> bool {
        match &self.storage {
            Storage::Stack(stack) => stack.data_len > stack.last_end(),
            Storage::Heap(heap) => heap.data_len > heap.last_end(),
        }
    }

    /// The number of rows, not of items: [`Self::as_flat`] counts the items.
    pub fn len(&self) -> usize {
        self.lens().1 as usize
    }

    /// No rows at all, whether or not one is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One row, borrowed out of the run it shares with the others, so reading a
    /// row is neither a copy nor an indirection.
    pub fn get_row(&self, index: usize) -> Option<&[T]> {
        match &self.storage {
            Storage::Stack(stack) => stack.get_row(index),
            Storage::Heap(heap) => heap.get_row(index),
        }
    }

    /// Every item of every row, in row order.
    pub fn as_flat(&self) -> &[T] {
        match &self.storage {
            Storage::Stack(stack) => stack.data(),
            Storage::Heap(heap) => heap.data(),
        }
    }

    /// Makes room for `rows` more rows of `data` more items in total, so a run
    /// of pushes of a known size reallocates once instead of once per doubling.
    pub fn reserve(&mut self, data: usize, rows: usize) {
        let (data_len, row_count) = self.lens();
        let (capacity_data, capacity_rows) = self.capacities();
        let need_data = as_capacity(data_len as usize + data);
        let need_rows = as_capacity(row_count as usize + rows);
        if need_data <= capacity_data && need_rows <= capacity_rows {
            return;
        }
        self.reallocate(need_data.max(capacity_data), need_rows.max(capacity_rows));
    }

    /// Gives back what the doubling in [`Self::grow`] took and the rows never
    /// used, which is up to half the allocation after a run of pushes.
    ///
    /// Drops back to the inline storage when the rows now fit it, so a vec that
    /// grew and was rebuilt smaller stops holding an allocation at all.
    pub fn shrink_to_fit(&mut self) {
        let lens = self.lens();
        // The inline buffer is part of the struct, so there is nothing to give
        // back while the vec is still in it.
        if !self.is_heap() || lens == self.capacities() {
            return;
        }
        self.reallocate(lens.0, lens.1);
    }

    /// Appends a row if all of it fits, and leaves the vec as it was if it does
    /// not — a refusal is not a partial row.
    pub fn try_push(&mut self, row: impl IntoIterator<Item = T>) -> Result<(), JaggedVecError> {
        let mut iter = row.into_iter();
        let guard = RowGuard { vec: self };
        match guard.vec.try_push_inner(&mut iter, None) {
            Ok(()) => Ok(()),
            // Dropping the refused item, and the guard truncating what did get
            // in, is what makes a failed push leave no trace.
            Err((err, _refused)) => Err(err),
        }
    }

    /// Appends a row, growing as many times as it takes.
    ///
    /// Finishes a row left open by [`Self::push_data`] rather than starting a
    /// new one, since a partial row and a pushed one are the same thing to the
    /// storage underneath.
    pub fn push<I>(&mut self, row: I)
    where
        I: IntoIterator,
        I::Item: Into<T>,
    {
        let mut iter = row.into_iter();
        let (size, _) = iter.size_hint();
        let guard = RowGuard { vec: self };
        let mut item: Option<T> = None;
        loop {
            match guard.vec.try_push_inner(&mut iter, item) {
                Ok(()) => break,
                Err((full, refused)) => {
                    // Items already moved in sit past the last row end, where
                    // `grow` carries them over, so the retry resumes mid-row.
                    guard.vec.grow(full, size);
                    item = refused;
                }
            }
        }
    }

    /// Adds one item to the row being built, growing as many times as it takes.
    ///
    /// Not a row until [`Self::commit_row`]: until then the item sits past the
    /// last row end, where [`Self::get_row`] and [`Self::iter`] do not see it.
    pub fn push_data(&mut self, data: T) {
        let mut data = Some(data);
        while let Some(item) = data {
            match self.try_push_data(item) {
                Ok(()) => break,
                Err((full, refused)) => {
                    self.grow(full, 1);
                    data = refused;
                }
            }
        }
    }

    /// Closes the row built by [`Self::push_data`], growing if the rows are full.
    ///
    /// Commits an empty row when nothing was pushed, exactly as `push([])` does.
    /// Whether an empty row means anything belongs to the caller, and a
    /// container that quietly dropped one would disagree with its own `push`.
    pub fn commit_row(&mut self) {
        loop {
            match self.try_commit_row() {
                Ok(()) => break,
                Err(full) => {
                    self.grow(full, 0);
                }
            }
        }
    }

    /// [`Self::commit_row`] without the growing, for a caller that would rather
    /// be told the rows are full.
    pub fn try_commit_row(&mut self) -> Result<(), JaggedVecError> {
        match &mut self.storage {
            Storage::Stack(stack) => stack.commit_row(),
            Storage::Heap(heap) => heap.commit_row(),
        }
    }

    /// [`Self::push_data`] without the growing, handing the item back when there
    /// is no room for it.
    pub fn try_push_data(&mut self, data: T) -> Result<(), (JaggedVecError, Option<T>)> {
        match &mut self.storage {
            Storage::Stack(stack) => stack.push_data(data),
            Storage::Heap(heap) => heap.push_data(data),
        }
    }

    /// Appends `remaining` and then as much of `row` as fits, reporting the
    /// first item that did not.
    fn try_push_inner<I>(
        &mut self,
        row: &mut I,
        remaining: Option<T>,
    ) -> Result<(), (JaggedVecError, Option<T>)>
    where
        I: Iterator,
        I::Item: Into<T>,
    {
        match &mut self.storage {
            Storage::Stack(stack) => {
                if let Some(item) = remaining {
                    stack.push_data(item)?;
                };
                stack.push(row)
            }
            Storage::Heap(heap) => {
                if let Some(item) = remaining {
                    heap.push_data(item)?;
                };
                heap.push(row)
            }
        }
    }

    /// Drops the items a half-written row left past the last complete one.
    fn normalize_data(&mut self) {
        match &mut self.storage {
            Storage::Stack(stack) => stack.normalize_data(),
            Storage::Heap(heap) => heap.normalize_data(),
        }
    }

    /// Inline while both capacities fit it, on the heap otherwise.
    fn storage(capacity_data: u32, capacity_rows: u32) -> Storage<T, N, R> {
        if capacity_data <= N as u32 && capacity_rows <= R as u32 {
            Storage::Stack(StorageStack::new())
        } else {
            Storage::Heap(StorageHeap::new(capacity_data, capacity_rows))
        }
    }

    /// Items then rows, the order [`Self::with_capacity`] takes them in.
    fn capacities(&self) -> (u32, u32) {
        match &self.storage {
            Storage::Stack(_) => (N as u32, R as u32),
            Storage::Heap(heap) => (heap.capacity_data, heap.capacity_rows),
        }
    }

    /// Items then rows, counting the open row's items among the items.
    fn lens(&self) -> (u32, u32) {
        match &self.storage {
            Storage::Stack(stack) => (stack.data_len as u32, stack.rows as u32),
            Storage::Heap(heap) => (heap.data_len, heap.rows),
        }
    }

    /// Reallocates with more room for whichever dimension ran out.
    ///
    /// Growing both would double the one that was not the problem, and a jagged
    /// vec is normally lopsided — many short rows, or few long ones.
    fn grow(&mut self, full: JaggedVecError, data_size_hint: usize) {
        let (data_len, rows) = self.lens();
        let (mut capacity_data, mut capacity_rows) = self.capacities();
        match full {
            JaggedVecError::FullRows => capacity_rows = grown(capacity_rows, rows as usize + 1),
            // `FullData`, and `InvalidConsumer`, which no push produces.
            _ => {
                let needed = (data_len as usize + data_size_hint).max(data_len as usize + 1);
                capacity_data = grown(capacity_data, needed);
            }
        }
        self.reallocate(capacity_data, capacity_rows);
    }

    /// Moves every row into a storage of the given capacities, whichever arm
    /// that lands in — which is how a shrink gets back to the inline one.
    fn reallocate(&mut self, capacity_data: u32, capacity_rows: u32) {
        replace_with_or_abort(&mut self.storage, |self_| match self_ {
            Storage::Stack(stack) => {
                Self::storage_from_consumer(capacity_data, capacity_rows, stack.consumer())
            }
            Storage::Heap(heap) => {
                Self::storage_from_consumer(capacity_data, capacity_rows, heap.consumer())
            }
        });
    }

    /// Builds whichever storage the capacities call for out of one being dismantled.
    fn storage_from_consumer(
        capacity_data: u32,
        capacity_rows: u32,
        consumer: impl StorageConsumer<T>,
    ) -> Storage<T, N, R> {
        let result = if capacity_data <= N as u32 && capacity_rows <= R as u32 {
            StorageStack::from_consumer(consumer).map(Storage::Stack)
        } else {
            StorageHeap::from_consumer(capacity_data, capacity_rows, consumer).map(Storage::Heap)
        };
        // Both callers size the target from what the consumer already holds.
        result.expect("target storage must fit the consumer")
    }

    /// Resolves the storage once, so a full walk costs one offset read per row
    /// instead of a [`Self::get_row`] each.
    pub fn iter(&self) -> JaggedVecIter<'_, T> {
        let (data, ends) = match &self.storage {
            Storage::Stack(stack) => (
                stack.data(),
                Ends::Stack(&stack.ends[..stack.rows as usize]),
            ),
            Storage::Heap(heap) => (heap.data(), Ends::Heap(heap.ends())),
        };
        JaggedVecIter {
            data,
            ends,
            index: 0,
            start: 0,
        }
    }
}

impl<T, const N: usize, const R: usize> Default for JaggedVec<T, N, R> {
    /// The same as [`JaggedVec::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// Sized to what is being cloned rather than to the capacity it came from, so
/// a clone of a vec that grew does not carry the doubling with it.
impl<T: Clone, const N: usize, const R: usize> Clone for JaggedVec<T, N, R> {
    /// Exactly the rows it holds, with none of the capacity they grew into.
    fn clone(&self) -> Self {
        let (data_len, rows) = self.lens();
        let mut out = Self::with_capacity(data_len as usize, rows as usize);
        for row in self.iter() {
            out.push(row.iter().cloned());
        }
        out
    }
}

/// Rows and items, never the storage they are in: a vec that spilled to the
/// heap is equal to the inline one holding the same rows.
impl<T: PartialEq, const N: usize, const R: usize> PartialEq for JaggedVec<T, N, R> {
    /// Row by row, item by item.
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<T: Eq, const N: usize, const R: usize> Eq for JaggedVec<T, N, R> {}

impl<T: std::fmt::Debug, const N: usize, const R: usize> std::fmt::Debug for JaggedVec<T, N, R> {
    /// The rows as a list, which is how they were written.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A capacity that fits the `u32` indices the heap storage is built on.
fn as_capacity(capacity: usize) -> u32 {
    u32::try_from(capacity).expect("capacity overflow")
}

/// Doubles `capacity`, with `needed` as a floor.
///
/// Panics rather than returning a capacity that did not grow: `push` retries
/// until its row fits, so one that cannot grow is an endless loop.
fn grown(capacity: u32, needed: usize) -> u32 {
    let next = capacity.saturating_mul(2).max(as_capacity(needed)).max(1);
    assert!(next > capacity, "capacity overflow");
    next
}

/// Truncates a half-written row back to the last complete one on the way out of
/// a push, however it ends.
///
/// A row is only half-written until the push returns, so an iterator that
/// panics would otherwise leave its items to be adopted by the next row pushed.
struct RowGuard<'a, T, const N: usize, const R: usize> {
    vec: &'a mut JaggedVec<T, N, R>,
}

impl<T, const N: usize, const R: usize> Drop for RowGuard<'_, T, N, R> {
    /// Truncates whatever the push left open.
    fn drop(&mut self) {
        self.vec.normalize_data();
    }
}

/// Walks the rows with the storage resolved once, so a row costs one offset
/// read and no revisit of the enum.
pub struct JaggedVecIter<'a, T> {
    data: &'a [T],
    ends: Ends<'a>,
    index: usize,
    /// End of the previous row, carried so a row does not re-read `ends[i - 1]`.
    start: usize,
}

/// The two storages index their rows at the width each can afford, so an
/// iterator over either has to name both.
enum Ends<'a> {
    Stack(&'a [u8]),
    Heap(&'a [u32]),
}

impl Ends<'_> {
    /// The end of `index`, widened to the one width both arms answer in.
    fn get(&self, index: usize) -> Option<usize> {
        match self {
            Ends::Stack(ends) => ends.get(index).map(|end| *end as usize),
            Ends::Heap(ends) => ends.get(index).map(|end| *end as usize),
        }
    }

    /// How many rows the offsets cover.
    fn len(&self) -> usize {
        match self {
            Ends::Stack(ends) => ends.len(),
            Ends::Heap(ends) => ends.len(),
        }
    }
}

impl<'a, T> Iterator for JaggedVecIter<'a, T> {
    type Item = &'a [T];

    /// One offset read, with the previous one carried in as this row's start.
    fn next(&mut self) -> Option<Self::Item> {
        let end = self.ends.get(self.index)?;
        let start = std::mem::replace(&mut self.start, end);
        self.index += 1;
        Some(&self.data[start..end])
    }

    /// Exact: the rows left to walk.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.ends.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl<T> ExactSizeIterator for JaggedVecIter<'_, T> {}
impl<T> std::iter::FusedIterator for JaggedVecIter<'_, T> {}

impl<'a, T, const N: usize, const R: usize> IntoIterator for &'a JaggedVec<T, N, R> {
    type Item = &'a [T];
    type IntoIter = JaggedVecIter<'a, T>;

    /// The same as [`JaggedVec::iter`].
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Which of the two storages holds the rows.
///
/// The enum is as large as its larger arm whichever one is live, which is what
/// makes `N` and `R` a decision about the size of every instance.
enum Storage<T, const N: usize, const R: usize> {
    Stack(StorageStack<T, N, R>),
    Heap(StorageHeap<T>),
}

/// A storage being dismantled so another one can take what it holds.
///
/// The data comes out as one contiguous slice so a move is a single
/// `copy_nonoverlapping` rather than a `read` per item.
trait StorageConsumer<T> {
    /// How many rows the storage holds.
    fn rows(&self) -> usize;

    /// End offset of `row`, which must be below [`Self::rows`].
    fn end(&self, row: usize) -> usize;

    /// The initialised items, in row order.
    fn data(&self) -> &[T];

    /// Gives up ownership of [`Self::data`] so the source stops dropping it.
    ///
    /// # Safety
    /// Every item of [`Self::data`] must already have been moved out exactly
    /// once.
    unsafe fn release_data(&mut self);
}

/// Rows indexed with `u8`, which is what keeps the inline variant worth having
/// and what `CHECK` holds `N` and `R` under.
struct StorageStack<T, const N: usize, const R: usize> {
    rows: u8,
    ends: [u8; R],
    data_len: u8,
    data: [MaybeUninit<T>; N],
}

impl<T, const N: usize, const R: usize> StorageStack<T, N, R> {
    /// `u8` indices are what keep the inline arm small; these are the bounds
    /// that keep them able to address it.
    const CHECK: () = {
        assert!(N > 0, "N must be greater than 0");
        assert!(N < u8::MAX as usize, "N must be less than u8::MAX");
        assert!(R > 0, "R must be greater than 0");
        assert!(R < u8::MAX as usize, "R must be less than u8::MAX");
    };

    /// Empty, and the one place `CHECK` is forced.
    fn new() -> Self {
        let _: () = Self::CHECK;
        Self {
            rows: 0,
            ends: [0; R],
            data_len: 0,
            data: [const { MaybeUninit::uninit() }; N],
        }
    }

    /// Takes over everything the consumer holds, or refuses without having
    /// touched it.
    fn from_consumer(mut consumer: impl StorageConsumer<T>) -> Result<Self, JaggedVecError> {
        let rows = consumer.rows();
        let data_len = consumer.data().len();
        // Checked up front so a refusal costs nothing.
        if data_len > N {
            return Err(JaggedVecError::FullData);
        }
        if rows > R {
            return Err(JaggedVecError::FullRows);
        }

        let mut stack = Self::new();
        // SAFETY: `data_len <= N`, the two buffers are distinct, and
        // `release_data` runs with no panic point in between, so the items are
        // owned by exactly one of the two throughout.
        unsafe {
            std::ptr::copy_nonoverlapping(
                consumer.data().as_ptr(),
                stack.data.as_mut_ptr().cast::<T>(),
                data_len,
            );
            consumer.release_data();
        }
        stack.data_len = data_len as u8;

        let mut last_end = 0;
        for row in 0..rows {
            let end = consumer.end(row);
            if end < last_end || end > data_len {
                return Err(JaggedVecError::InvalidConsumer);
            }
            stack.ends[row] = end as u8;
            stack.rows = (row + 1) as u8;
            last_end = end;
        }

        Ok(stack)
    }

    /// Where the last complete row ends, and so where an open one begins.
    fn last_end(&self) -> u8 {
        if self.rows == 0 {
            0
        } else {
            self.ends[self.rows as usize - 1]
        }
    }

    /// Drops the items a half-written row left past the last complete one.
    fn normalize_data(&mut self) {
        let last_end = self.last_end();
        if last_end < self.data_len {
            // SAFETY: `last_end..data_len` is inside the initialised prefix.
            unsafe {
                let slice = self.data[last_end as usize..self.data_len as usize].assume_init_mut();
                std::ptr::drop_in_place(slice);
            }
            self.data_len = last_end;
        }
    }

    /// Adds an item past the last row end, where it stays until `commit_row`.
    fn push_data(&mut self, data: T) -> Result<(), (JaggedVecError, Option<T>)> {
        if self.data_len as usize == N {
            return Err((JaggedVecError::FullData, Some(data)));
        }
        self.data[self.data_len as usize].write(data);
        self.data_len += 1;
        Ok(())
    }

    /// Turns everything past the last row end into a row.
    fn commit_row(&mut self) -> Result<(), JaggedVecError> {
        if self.rows as usize == R {
            return Err(JaggedVecError::FullRows);
        }
        self.ends[self.rows as usize] = self.data_len;
        self.rows += 1;
        Ok(())
    }

    /// A `push_data` per item and then a `commit_row`, with the rows checked
    /// first so a full one refuses before the iterator is touched.
    fn push<I>(&mut self, row: &mut I) -> Result<(), (JaggedVecError, Option<T>)>
    where
        I: Iterator,
        I::Item: Into<T>,
    {
        if self.rows as usize == R {
            return Err((JaggedVecError::FullRows, None));
        }
        for item in row {
            self.push_data(item.into())?;
        }
        self.ends[self.rows as usize] = self.data_len;
        self.rows += 1;
        Ok(())
    }

    /// The initialised prefix, which is what the rows are cut from.
    fn data(&self) -> &[T] {
        // SAFETY: `0..data_len` is exactly what was written.
        unsafe { self.data[..self.data_len as usize].assume_init_ref() }
    }

    /// One row out of that prefix.
    fn get_row(&self, row: usize) -> Option<&[T]> {
        if row >= self.rows as usize {
            return None;
        }
        let start = if row > 0 { self.ends[row - 1] } else { 0 } as usize;
        let end = self.ends[row] as usize;
        // SAFETY: ends are non-decreasing and bounded by `data_len`, so
        // `start..end` is inside the initialised prefix.
        Some(unsafe { self.data[start..end].assume_init_ref() })
    }

    /// Hands the storage over whole, to be dismantled.
    fn consumer(self) -> StorageStackConsumer<T, N, R> {
        StorageStackConsumer { stack: self }
    }
}

impl<T, const N: usize, const R: usize> Drop for StorageStack<T, N, R> {
    /// Drops what was written; the rest of the array never was.
    fn drop(&mut self) {
        // SAFETY: `0..data_len` is exactly what was written.
        unsafe {
            let slice = self.data[0..self.data_len as usize].assume_init_mut();
            std::ptr::drop_in_place(slice);
        }
    }
}

/// Row ends and row data in one allocation: the ends first, the data after the
/// padding `Layout::extend` inserts for `T`'s alignment.
///
/// Indexed with `u32` rather than `usize` to keep the header at 32 bytes, and
/// the header is what sets the floor on the whole enum — four billion items is
/// well past where an inline-first type is the right one.
struct StorageHeap<T> {
    ends: NonNull<u32>,
    data: NonNull<T>,
    rows: u32,
    capacity_rows: u32,
    data_len: u32,
    capacity_data: u32,
    /// `StorageHeap` owns its items; `NonNull` alone would not say so.
    marker: PhantomData<T>,
}

// SAFETY: the items are owned exclusively, so the storage is as thread safe as
// they are.
unsafe impl<T: Send> Send for StorageHeap<T> {}
unsafe impl<T: Sync> Sync for StorageHeap<T> {}

impl<T> StorageHeap<T> {
    /// Empty, with the single allocation its capacities call for.
    fn new(capacity_data: u32, capacity_rows: u32) -> Self {
        let (ends, data) = Self::alloc(capacity_data, capacity_rows);
        Self {
            ends,
            data,
            rows: 0,
            capacity_rows,
            data_len: 0,
            capacity_data,
            marker: PhantomData,
        }
    }

    /// Takes over everything the consumer holds, or refuses before allocating.
    fn from_consumer(
        capacity_data: u32,
        capacity_rows: u32,
        mut consumer: impl StorageConsumer<T>,
    ) -> Result<Self, JaggedVecError> {
        let rows = consumer.rows();
        let data_len = consumer.data().len();
        // Checked up front so a refusal costs no allocation.
        if data_len > capacity_data as usize {
            return Err(JaggedVecError::FullData);
        }
        if rows > capacity_rows as usize {
            return Err(JaggedVecError::FullRows);
        }

        let mut heap = Self::new(capacity_data, capacity_rows);
        // SAFETY: `data_len <= capacity_data`, the allocations are distinct, and
        // `release_data` runs with no panic point in between, so the items are
        // owned by exactly one of the two throughout.
        unsafe {
            std::ptr::copy_nonoverlapping(consumer.data().as_ptr(), heap.data.as_ptr(), data_len);
            consumer.release_data();
        }
        heap.data_len = data_len as u32;

        let mut last_end = 0;
        for row in 0..rows {
            let end = consumer.end(row);
            if end < last_end || end > data_len {
                return Err(JaggedVecError::InvalidConsumer);
            }
            // SAFETY: `row < rows <= capacity_rows`.
            unsafe { heap.ends.as_ptr().add(row).write(end as u32) };
            heap.rows = (row + 1) as u32;
            last_end = end;
        }

        Ok(heap)
    }

    /// Derived from the capacities rather than stored, because a `Layout` is 16
    /// bytes in every instance to save two multiplications at drop.
    ///
    /// The capacities never change after construction, so this returns what
    /// [`Self::alloc`] allocated.
    fn layout(capacity_data: u32, capacity_rows: u32) -> (Layout, usize) {
        let ends = Layout::array::<u32>(capacity_rows as usize).expect("capacity overflow");
        let data = Layout::array::<T>(capacity_data as usize).expect("capacity overflow");
        let (layout, data_offset) = ends.extend(data).expect("capacity overflow");
        (layout.pad_to_align(), data_offset)
    }

    /// The one allocation, handed back as the two pointers into it.
    fn alloc(capacity_data: u32, capacity_rows: u32) -> (NonNull<u32>, NonNull<T>) {
        let (layout, data_offset) = Self::layout(capacity_data, capacity_rows);
        // A zero-sized layout must never reach the allocator, and a ZST `T` with
        // no rows produces one.
        if layout.size() == 0 {
            return (NonNull::dangling(), NonNull::dangling());
        }
        // SAFETY: the layout is non-zero-sized, and `data_offset` is inside it.
        unsafe {
            let ptr = std::alloc::alloc(layout);
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            (
                NonNull::new_unchecked(ptr.cast::<u32>()),
                NonNull::new_unchecked(ptr.add(data_offset).cast::<T>()),
            )
        }
    }

    /// Where the last complete row ends, and so where an open one begins.
    fn last_end(&self) -> u32 {
        if self.rows == 0 {
            0
        } else {
            self.ends()[self.rows as usize - 1]
        }
    }

    /// Drops the items a half-written row left past the last complete one.
    fn normalize_data(&mut self) {
        let last_end = self.last_end();
        if last_end < self.data_len {
            // SAFETY: `last_end..data_len` is inside the initialised prefix.
            unsafe { std::ptr::drop_in_place(&mut self.data_mut()[last_end as usize..]) };
            self.data_len = last_end;
        }
    }

    /// Adds an item past the last row end, where it stays until `commit_row`.
    fn push_data(&mut self, item: T) -> Result<(), (JaggedVecError, Option<T>)> {
        if self.data_len >= self.capacity_data {
            return Err((JaggedVecError::FullData, Some(item)));
        }
        // SAFETY: bounds checked just above.
        unsafe { self.data.as_ptr().add(self.data_len as usize).write(item) };
        self.data_len += 1;
        Ok(())
    }

    /// Turns everything past the last row end into a row.
    fn commit_row(&mut self) -> Result<(), JaggedVecError> {
        if self.rows >= self.capacity_rows {
            return Err(JaggedVecError::FullRows);
        }
        unsafe {
            self.ends
                .as_ptr()
                .add(self.rows as usize)
                .write(self.data_len)
        }
        self.rows += 1;
        Ok(())
    }

    /// A `push_data` per item and then a `commit_row`, with the rows checked
    /// first so a full one refuses before the iterator is touched.
    fn push<I>(&mut self, row: &mut I) -> Result<(), (JaggedVecError, Option<T>)>
    where
        I: Iterator,
        I::Item: Into<T>,
    {
        if self.rows >= self.capacity_rows {
            return Err((JaggedVecError::FullRows, None));
        }

        // SAFETY: `data_len <= capacity_data`, so this is at worst one past the
        // end; it is only written after the check inside the loop.
        let mut data = unsafe { self.data.as_ptr().add(self.data_len as usize) };
        for item in row {
            let item = item.into();
            if self.data_len >= self.capacity_data {
                return Err((JaggedVecError::FullData, Some(item)));
            }
            // SAFETY: bounds checked just above.
            data = unsafe {
                data.write(item);
                data.add(1)
            };
            self.data_len += 1;
        }
        // SAFETY: `rows < capacity_rows`, checked on entry.
        unsafe {
            self.ends
                .as_ptr()
                .add(self.rows as usize)
                .write(self.data_len)
        };
        self.rows += 1;
        Ok(())
    }

    /// The row ends written so far.
    fn ends(&self) -> &[u32] {
        // SAFETY: `rows` ends have been written, and a dangling pointer with a
        // length of zero is a valid empty slice.
        unsafe { std::slice::from_raw_parts(self.ends.as_ptr(), self.rows as usize) }
    }

    /// The items written so far, an open row's among them.
    fn data(&self) -> &[T] {
        // SAFETY: `data_len` items have been written.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.data_len as usize) }
    }

    /// [`Self::data`], for the truncating and the dropping.
    fn data_mut(&mut self) -> &mut [T] {
        // SAFETY: as `data`, and `&mut self` makes the borrow unique.
        unsafe { std::slice::from_raw_parts_mut(self.data.as_ptr(), self.data_len as usize) }
    }

    /// One row out of the items written so far.
    fn get_row(&self, row: usize) -> Option<&[T]> {
        let ends = self.ends();
        if row >= ends.len() {
            return None;
        }
        let start = if row > 0 { ends[row - 1] } else { 0 } as usize;
        Some(&self.data()[start..ends[row] as usize])
    }

    /// Hands the storage over whole, to be dismantled.
    fn consumer(self) -> StorageHeapConsumer<T> {
        StorageHeapConsumer { heap: self }
    }
}

impl<T> Drop for StorageHeap<T> {
    /// Drops the items, then the allocation they shared with the ends.
    fn drop(&mut self) {
        // SAFETY: the items are owned, and the layout is the one `alloc` used
        // because the capacities it is derived from have not changed.
        unsafe {
            std::ptr::drop_in_place(self.data_mut());
            let (layout, _) = Self::layout(self.capacity_data, self.capacity_rows);
            if layout.size() != 0 {
                std::alloc::dealloc(self.ends.as_ptr().cast::<u8>(), layout);
            }
        }
    }
}

/// Both consumers keep their storage whole and disarm it by zeroing `data_len`,
/// so the storage's own `Drop` stays the single place that frees items.
struct StorageStackConsumer<T, const N: usize, const R: usize> {
    stack: StorageStack<T, N, R>,
}

impl<T, const N: usize, const R: usize> StorageConsumer<T> for StorageStackConsumer<T, N, R> {
    /// How many rows the stack holds.
    fn rows(&self) -> usize {
        self.stack.rows as usize
    }

    /// Where row `row` ends, widened from the `u8` the stack indexes at.
    fn end(&self, row: usize) -> usize {
        self.stack.ends[row] as usize
    }

    /// The stack's initialised prefix.
    fn data(&self) -> &[T] {
        self.stack.data()
    }

    /// Zeroes the length, so the stack's own `Drop` frees nothing.
    unsafe fn release_data(&mut self) {
        self.stack.data_len = 0;
    }
}

/// The heap storage kept whole, so its own `Drop` still frees the allocation.
struct StorageHeapConsumer<T> {
    heap: StorageHeap<T>,
}

impl<T> StorageConsumer<T> for StorageHeapConsumer<T> {
    /// How many rows the heap holds.
    fn rows(&self) -> usize {
        self.heap.rows as usize
    }

    /// Where row `row` ends.
    fn end(&self, row: usize) -> usize {
        self.heap.ends()[row] as usize
    }

    /// The heap's initialised items.
    fn data(&self) -> &[T] {
        self.heap.data()
    }

    /// Zeroes the length, so the heap's `Drop` frees the allocation and no items.
    unsafe fn release_data(&mut self) {
        self.heap.data_len = 0;
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use super::*;
    use crate::util::str::SmallStr;

    #[test]
    fn test_jagged_vec() {
        type A = JaggedVec<SmallStr, 5, 2>;
        let mut a = A::new();
        a.push(["oi"]);
        a.push(vec!["oi"; 23]);

        assert_eq!(a.len(), 2);
        assert_eq!(a.get_row(0).unwrap().len(), 1);
        assert_eq!(a.get_row(1).unwrap().len(), 23);
    }

    static LIVE: AtomicIsize = AtomicIsize::new(0);

    /// Counts its own births and deaths, so a leak and a double drop both show.
    #[derive(Debug)]
    struct D(Box<u32>);

    impl D {
        fn new(v: u32) -> Self {
            LIVE.fetch_add(1, Ordering::SeqCst);
            D(Box::new(v))
        }
    }

    /// Serialises the tests that share [`LIVE`] and zeroes it, so a parallel
    /// run does not have them counting each other's items.
    fn counting() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        LIVE.store(0, Ordering::SeqCst);
        guard
    }

    impl Clone for D {
        fn clone(&self) -> Self {
            D::new(*self.0)
        }
    }

    impl Drop for D {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_heap_header_stays_at_thirty_two_bytes() {
        assert_eq!(size_of::<StorageHeap<u32>>(), 32);
        assert_eq!(size_of::<StorageHeap<[u8; 24]>>(), 32);
    }

    #[test]
    fn stack_leaves_the_tail_uninitialised() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 8, 4> = JaggedVec::new();
            v.push((0..2u32).map(D::new));
            assert_eq!(v.get_row(0).unwrap().len(), 2);
            assert!(v.get_row(1).is_none());
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn grow_from_stack_to_heap_keeps_every_row() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 4, 2> = JaggedVec::new();
            for r in 0..24u32 {
                v.push((0..3u32).map(|i| D::new(r * 100 + i)));
            }
            assert!(v.is_heap());
            assert_eq!(v.len(), 24);
            for r in 0..24usize {
                let row = v.get_row(r).unwrap();
                assert_eq!(row.len(), 3);
                for (i, d) in row.iter().enumerate() {
                    assert_eq!(*d.0, (r as u32) * 100 + i as u32);
                }
            }
            assert!(v.get_row(24).is_none());
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn is_row_open_tracks_the_uncommitted_items() {
        let mut v: JaggedVec<u32, 4, 2> = JaggedVec::new();
        assert!(!v.is_row_open());
        v.push_data(1);
        assert!(v.is_row_open());
        v.commit_row();
        assert!(!v.is_row_open());

        // An open row is carried across a grow by the consumer, so it is still
        // open on the other side.
        for i in 0..20u32 {
            v.push_data(i);
        }
        assert!(v.is_heap());
        assert!(v.is_row_open());
        v.commit_row();
        assert!(!v.is_row_open());
        assert_eq!(v.get_row(1).unwrap().len(), 20);
    }

    #[test]
    fn a_refused_push_leaves_no_trace() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 4, 4> = JaggedVec::new();
            assert_eq!(
                v.try_push((0..10u32).map(D::new)),
                Err(JaggedVecError::FullData)
            );
            assert!(v.get_row(0).is_none());
            v.try_push((0..2u32).map(D::new)).unwrap();
            assert_eq!(v.get_row(0).unwrap().len(), 2);
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_refused_push_leaves_no_trace_on_the_heap() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 1, 1> = JaggedVec::with_capacity(8, 4);
            assert!(v.is_heap());
            v.try_push((0..2u32).map(D::new)).unwrap();
            assert_eq!(
                v.try_push((0..20u32).map(D::new)),
                Err(JaggedVecError::FullData)
            );
            assert_eq!(v.len(), 1);
            v.try_push((0..2u32).map(D::new)).unwrap();
            assert_eq!(
                v.get_row(1).unwrap().len(),
                2,
                "the refused push left orphans for the next row to adopt"
            );
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stack_round_trips_through_its_own_consumer() {
        let _counting = counting();
        {
            let mut s: StorageStack<D, 8, 4> = StorageStack::new();
            s.push(&mut (0..2u32).map(D::new)).map_err(|e| e.0).unwrap();
            s.push(&mut (0..3u32).map(D::new)).map_err(|e| e.0).unwrap();
            let out: StorageStack<D, 8, 4> = StorageStack::from_consumer(s.consumer()).unwrap();
            assert_eq!(out.get_row(0).unwrap().len(), 2);
            assert_eq!(out.get_row(1).unwrap().len(), 3);
            assert!(out.get_row(2).is_none());
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_zero_sized_layout_never_reaches_the_allocator() {
        let mut v: JaggedVec<(), 4, 4> = JaggedVec::with_capacity(16, 0);
        assert!(v.is_heap());
        // And growing from a zero row capacity still makes progress.
        v.push([(), (), ()]);
        assert_eq!(v.get_row(0).unwrap().len(), 3);
    }

    #[test]
    fn zero_sized_items_keep_their_row_lengths() {
        let mut v: JaggedVec<(), 4, 2> = JaggedVec::new();
        for _ in 0..8 {
            v.push([(); 3]);
        }
        assert_eq!(v.len(), 8);
        assert_eq!(v.get_row(7).unwrap().len(), 3);
    }

    #[test]
    fn iter_agrees_with_get_row_and_fuses() {
        let mut v: JaggedVec<u32, 4, 2> = JaggedVec::new();
        for r in 0..5u32 {
            v.push(0..r);
        }
        let by_iter: Vec<&[u32]> = v.iter().collect();
        let by_index: Vec<&[u32]> = (0..v.len()).map(|r| v.get_row(r).unwrap()).collect();
        assert_eq!(by_iter, by_index);
        assert_eq!(v.iter().len(), 5);

        let mut it = v.iter();
        for _ in 0..5 {
            assert!(it.next().is_some());
        }
        assert!(it.next().is_none());
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_agrees_with_get_row_on_the_inline_storage() {
        let mut v: JaggedVec<u32, 16, 8> = JaggedVec::new();
        v.push([1u32, 2, 3]);
        v.push([4u32]);
        assert!(!v.is_heap());
        let by_iter: Vec<&[u32]> = v.iter().collect();
        assert_eq!(by_iter, vec![&[1u32, 2, 3][..], &[4u32][..]]);
        assert_eq!(v.as_flat(), &[1, 2, 3, 4]);
    }

    #[test]
    fn grow_only_widens_the_dimension_that_ran_out() {
        // Many short rows: the rows run out, the data capacity should not double
        // along with them.
        let mut v: JaggedVec<u32, 64, 2> = JaggedVec::new();
        for _ in 0..3 {
            v.push([1u32]);
        }
        let (capacity_data, capacity_rows) = v.capacities();
        assert_eq!(capacity_data, 64, "data capacity grew without being full");
        assert!(capacity_rows >= 3);
    }

    #[test]
    fn reserve_makes_a_run_of_pushes_fit_at_once() {
        let mut v: JaggedVec<u32, 4, 2> = JaggedVec::new();
        v.reserve(1000, 100);
        let capacities = v.capacities();
        for r in 0..100u32 {
            v.push([r; 10]);
        }
        assert_eq!(v.capacities(), capacities, "reserve did not size it once");
        assert_eq!(v.len(), 100);
        assert_eq!(v.get_row(99).unwrap(), &[99u32; 10]);
    }

    #[test]
    fn clone_matches_and_is_sized_to_what_it_holds() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 4, 2> = JaggedVec::new();
            for r in 0..10u32 {
                v.push((0..3u32).map(|i| D::new(r * 100 + i)));
            }
            let c = v.clone();
            assert_eq!(c.len(), v.len());
            for r in 0..10usize {
                let (a, b) = (v.get_row(r).unwrap(), c.get_row(r).unwrap());
                assert_eq!(a.len(), b.len());
                for (x, y) in a.iter().zip(b) {
                    assert_eq!(*x.0, *y.0);
                }
            }
            // The clone carries no doubling: it is exactly the rows it holds.
            assert_eq!(c.capacities(), (c.lens().0, c.lens().1));
            assert_eq!(LIVE.load(Ordering::SeqCst), 2 * 10 * 3);
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eq_ignores_which_storage_the_rows_are_in() {
        let mut inline: JaggedVec<u32, 16, 8> = JaggedVec::new();
        let mut spilled: JaggedVec<u32, 16, 8> = JaggedVec::with_capacity(64, 32);
        for v in [&mut inline, &mut spilled] {
            v.push([1u32, 2]);
            v.push([3u32]);
        }
        assert!(!inline.is_heap());
        assert!(spilled.is_heap());
        assert_eq!(inline, spilled);

        spilled.push([4u32]);
        assert_ne!(inline, spilled);
    }

    #[test]
    fn shrink_to_fit_gives_back_the_doubling() {
        let _counting = counting();
        {
            let mut v: JaggedVec<D, 4, 2> = JaggedVec::new();
            for r in 0..10u32 {
                v.push((0..3u32).map(|_| D::new(r)));
            }
            let grown = v.capacities();
            v.shrink_to_fit();
            assert!(v.capacities().0 < grown.0 || v.capacities().1 < grown.1);
            assert_eq!(v.capacities(), (30, 10));
            assert_eq!(v.len(), 10);
            assert_eq!(v.get_row(9).unwrap().len(), 3);
            // Already exact: a second call must not churn the allocation.
            v.shrink_to_fit();
            assert_eq!(v.capacities(), (30, 10));
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shrink_to_fit_drops_back_to_the_inline_storage() {
        let mut v: JaggedVec<u32, 16, 8> = JaggedVec::with_capacity(1024, 512);
        assert!(v.is_heap());
        v.push([1u32, 2, 3]);
        v.shrink_to_fit();
        assert!(
            !v.is_heap(),
            "it fits inline now and should have moved back"
        );
        assert_eq!(v.get_row(0).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn heap_storage_is_send_when_its_items_are() {
        fn assert_send<T: Send>() {}
        assert_send::<JaggedVec<String, 4, 2>>();
    }
}

#[cfg(test)]
mod unwind {
    use super::*;

    #[test]
    fn a_panicking_iterator_leaves_no_half_row_behind() {
        let mut v: JaggedVec<u32, 16, 8> = JaggedVec::new();
        v.push([1u32, 2]);

        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            v.push((0..).map(|i: u32| if i < 3 { 100 + i } else { panic!("boom") }));
        }));
        assert!(r.is_err());

        assert_eq!(v.get_row(0).unwrap(), &[1, 2]);
        assert_eq!(v.len(), 1);

        // The next row must not adopt what the panicking push left behind.
        v.push([9u32, 9]);
        assert_eq!(v.get_row(1).unwrap(), &[9, 9]);
    }
}
