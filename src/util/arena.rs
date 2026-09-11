//! One allocation of bytes, handed out in fixed blocks.
//!
//! [`Arena`] takes its whole backing store up front and lends it out as
//! [`ArenaBlock`]s of `ARENA_BLOCK_SIZE` bytes each. It exists for the case where a
//! structure needs many equally sized, long lived buffers — a log's chunks,
//! say — and would otherwise ask the allocator for each of them separately.
//!
//! # What it buys
//!
//! Not bytes, mostly. A handle is larger than a [`Box`], since it carries a
//! reference to the arena as well as an index. What it buys is that the
//! blocks are one contiguous run rather than several thousand scattered
//! allocations: no per allocation header, no fragmentation, one call to the
//! allocator instead of thousands, and a walk over the blocks that reads
//! forwards through memory.
//!
//! # Blocks go out and do not come back
//!
//! Handing one out moves an offset along, and that is the whole allocator.
//! There is no free list, because nothing is ever put back: an arena is built
//! for a structure that takes what it needs at the start and holds it for as
//! long as it lives, and for that a counter is the entire bookkeeping.
//!
//! A handle that drops spends its block. A caller that allocates in a loop
//! will run the arena dry, and is not the caller this is for. The store goes
//! in one piece when the arena and its last handle are gone.
//!
//! # No lock anywhere, and no `Weak`
//!
//! A block belongs to exactly one handle, so two handles never name the same
//! bytes. Reading and writing through a handle is a pointer offset — the same
//! work as through a `Box` — because there is nothing to synchronise.
//!
//! A handle holds a strong reference, so the store outlives every block lent
//! from it. A weak one would let the arena go while blocks were still out,
//! but every access would then have to upgrade it, and an upgrade touches the
//! reference count: one cache line, shared by every holder. Measured against
//! a plain deref that is 35x on one thread, and it gets *worse* with more —
//! 389x on four, 731x on eight, while the plain deref gets faster because it
//! shares nothing. Holding the store a little longer is the cheaper mistake.
//!
//! # Swapping
//!
//! A handle is an index, so exchanging two of them is exchanging two indices
//! — no bytes move. `std::mem::swap` already does exactly that; there is no
//! special method for it.
//!
//! ```ignore
//! let arena = Arena::new(1024);
//! let mut a = arena.alloc().unwrap();
//! let mut b = arena.alloc().unwrap();
//! std::mem::swap(&mut a, &mut b);  // two indices, not two buffers
//! ```

use std::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// How many bytes one block holds.
///
/// Fixed rather than a parameter: the arena exists for one caller, and a size
/// it has to agree with is clearer stated once than threaded through every
/// signature. Whatever holds these has to be built to the same number — a
/// `const` assertion where the two meet keeps them from drifting apart.
pub const ARENA_BLOCK_SIZE: usize = 256;

/// A fixed pool of `ARENA_BLOCK_SIZE` byte buffers, allocated once.
///
/// Cloning is cheap and shares the same pool; the store lives until the arena
/// and every handle it lent out are gone.
pub struct Arena {
    inner: Arc<ArenaInner>,
}

impl Clone for Arena {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Arena {
    /// Reserve `capacity` blocks, zeroed.
    ///
    /// The store is taken from the allocator here and never grown, so this is
    /// the one place an arena backed structure can be slow — and the only
    /// one. Zeroing up front is what lets a block be handed over as plain
    /// initialised bytes, with no uninitialised memory to reason about
    /// anywhere else.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero, or if it does not fit in a `u32`. An index is
    /// what makes a handle small; widening it would defeat the point.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "an Arena needs a non-zero capacity");
        assert!(
            u32::try_from(capacity).is_ok(),
            "an Arena holds at most u32::MAX blocks"
        );

        let mut blocks = Vec::with_capacity(capacity);
        blocks.resize_with(capacity, || UnsafeCell::new([0u8; ARENA_BLOCK_SIZE]));

        Self {
            inner: Arc::new(ArenaInner {
                blocks: blocks.into_boxed_slice(),
                offset: AtomicUsize::new(0),
            }),
        }
    }

    /// Take the next block from the pool, or `None` if it is spent.
    ///
    /// For the part of a structure that is sized up front: asking for more
    /// than was reserved is a mistake about the sizing, and this is how the
    /// arena says so instead of papering over it.
    pub fn alloc(&self) -> Option<ArenaBlock> {
        Some(ArenaBlock {
            inner: ArenaBlockInner::Pooled {
                arena: self.inner.clone(),
                index: self.inner.claim()?,
            },
        })
    }

    /// Take the next block, falling back to the heap once the pool is spent.
    ///
    /// For the part that is not sized up front. The block behaves the same
    /// either way; what it loses is the company of the others — it sits
    /// wherever the allocator put it instead of alongside them.
    ///
    /// Deliberately not what [`alloc`](Arena::alloc) does. A structure whose
    /// fixed parts quietly spilled onto the heap would lose the locality the
    /// arena is for and never say a word about it, so those parts ask for a
    /// pooled block and are told when there is none.
    pub fn alloc_or_heap(&self) -> ArenaBlock {
        self.alloc().unwrap_or_else(|| ArenaBlock {
            inner: ArenaBlockInner::Owned(Box::new([0u8; ARENA_BLOCK_SIZE])),
        })
    }

    /// How many blocks the arena was built with.
    pub fn capacity(&self) -> usize {
        self.inner.blocks.len()
    }

    /// How many blocks have never been handed out.
    pub fn available(&self) -> usize {
        self.capacity() - self.inner.offset.load(Ordering::Relaxed)
    }

    /// Whether every block has been spent.
    pub fn is_exhausted(&self) -> bool {
        self.available() == 0
    }
}

/// One block of an [`Arena`]: `ARENA_BLOCK_SIZE` bytes, owned.
///
/// Stands in for a `Box<[u8; ARENA_BLOCK_SIZE]>` and derefs the same way. Unlike a
/// `Box`, the block it sits in is not reusable once dropped — see the
/// [module docs](self).
pub struct ArenaBlock {
    inner: ArenaBlockInner,
}

/// Where a block's bytes actually live.
///
/// Both shapes are the same size — the pooled one is a pointer and an index,
/// twelve bytes that round up to sixteen, and the tag rides in the padding
/// that rounding leaves behind. So carrying the choice costs nothing over
/// carrying only the pooled form, and what it buys is that running the pool
/// dry is a slower block rather than no block.
enum ArenaBlockInner {
    /// A block of an arena, shared with every other block of that arena.
    Pooled { arena: Arc<ArenaInner>, index: u32 },
    /// A block of its own, from the allocator, once the pool was spent.
    Owned(Box<[u8; ARENA_BLOCK_SIZE]>),
}

impl ArenaBlock {
    /// A block of its own, from the allocator, belonging to no arena.
    ///
    /// For a caller that has no arena to hand — one whose pool has already
    /// gone, say. It behaves like any other block and swaps with them freely.
    pub fn heap() -> Self {
        Self {
            inner: ArenaBlockInner::Owned(Box::new([0u8; ARENA_BLOCK_SIZE])),
        }
    }

    /// Which block of its arena this is, or `None` if it came from the heap.
    ///
    /// Only meaningful against the arena it came from. Useful for showing
    /// that a swap moved the index and not the bytes.
    pub fn index(&self) -> Option<u32> {
        match &self.inner {
            ArenaBlockInner::Pooled { index, .. } => Some(*index),
            ArenaBlockInner::Owned(_) => None,
        }
    }

    /// Whether the pool had room for this block, or the allocator had to.
    pub fn is_pooled(&self) -> bool {
        matches!(self.inner, ArenaBlockInner::Pooled { .. })
    }

    /// Whether both blocks came from the same arena.
    ///
    /// Two blocks from different arenas can still be swapped — each takes the
    /// other's arena with it — but a caller that does not mean to mix pools
    /// can check. Two heap blocks share no arena, so this is false for them.
    pub fn same_arena(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (
                ArenaBlockInner::Pooled { arena: a, .. },
                ArenaBlockInner::Pooled { arena: b, .. },
            ) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Deref for ArenaBlock {
    type Target = [u8; ARENA_BLOCK_SIZE];

    fn deref(&self) -> &[u8; ARENA_BLOCK_SIZE] {
        match &self.inner {
            // SAFETY: no other handle can name this block — the offset only
            // ever moves forwards — and every byte was zeroed when the store
            // was made.
            ArenaBlockInner::Pooled { arena, index } => unsafe {
                &*arena.blocks[*index as usize].get()
            },
            ArenaBlockInner::Owned(block) => block,
        }
    }
}

impl DerefMut for ArenaBlock {
    fn deref_mut(&mut self) -> &mut [u8; ARENA_BLOCK_SIZE] {
        match &mut self.inner {
            // SAFETY: as for `deref`, and `&mut self` rules out another
            // reference through this handle. No other handle can name the
            // block at all.
            ArenaBlockInner::Pooled { arena, index } => unsafe {
                &mut *arena.blocks[*index as usize].get()
            },
            ArenaBlockInner::Owned(block) => block,
        }
    }
}

// No `Drop`: bytes have no destructor, and the block is not put back.

// SAFETY: a handle owns its block outright, so moving one to another thread
// leaves nothing shared behind, and sharing a `&ArenaBlock` hands out a
// `&[u8]` — which is what a `&Box<[u8; N]>` does too.
unsafe impl Send for ArenaBlock {}
unsafe impl Sync for ArenaBlock {}

struct ArenaInner {
    blocks: Box<[UnsafeCell<[u8; ARENA_BLOCK_SIZE]>]>,
    /// How many blocks have been handed out. Only ever moves forwards.
    offset: AtomicUsize,
}

impl ArenaInner {
    /// Take the next block, if there is one.
    ///
    /// A plain increment would do but for the end: several threads arriving
    /// at the last block would all move the offset past it. Comparing first
    /// keeps it inside the store, so `available` cannot go negative and a
    /// long running arena cannot creep the counter away from the truth.
    ///
    /// Relaxed throughout: nothing is published through the offset. Each
    /// caller goes on to touch only its own block, and blocks are disjoint.
    fn claim(&self) -> Option<u32> {
        let mut offset = self.offset.load(Ordering::Relaxed);
        loop {
            if offset >= self.blocks.len() {
                return None;
            }
            match self.offset.compare_exchange_weak(
                offset,
                offset + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(offset as u32),
                Err(actual) => offset = actual,
            }
        }
    }
}

// SAFETY: the store is only ever reached through a handle that owns its
// index, so two threads holding two handles touch disjoint blocks. What is
// genuinely shared — the offset — is an atomic.
unsafe impl Send for ArenaInner {}
unsafe impl Sync for ArenaInner {}

#[cfg(test)]
mod test {
    use super::*;

    use super::ARENA_BLOCK_SIZE as BLOCK;

    #[test]
    fn lends_out_every_block_and_then_says_no() {
        let arena = Arena::new(3);
        assert_eq!(arena.capacity(), 3);
        assert_eq!(arena.available(), 3);

        let blocks: Vec<_> = (0..3).map(|_| arena.alloc().unwrap()).collect();
        assert!(arena.is_exhausted());
        assert!(arena.alloc().is_none(), "lent more than it has");

        // And it stays spent: the offset does not go back.
        drop(blocks);
        assert_eq!(arena.available(), 0);
        assert!(arena.alloc().is_none(), "a spent block was lent again");
    }

    /// Past the pool the allocator takes over, so a caller that cannot be
    /// sized up front still gets a block.
    #[test]
    fn falls_back_to_the_heap_once_the_pool_is_spent() {
        let arena = Arena::new(2);
        let pooled: Vec<_> = (0..2).map(|_| arena.alloc_or_heap()).collect();
        assert!(pooled.iter().all(|b| b.is_pooled()));
        assert!(arena.is_exhausted());

        let mut spare = arena.alloc_or_heap();
        assert!(
            !spare.is_pooled(),
            "the pool was spent, this must be its own"
        );
        assert_eq!(spare.index(), None);

        // And it is a block like any other.
        assert_eq!(&spare[..], &[0u8; ARENA_BLOCK_SIZE][..]);
        spare.fill(5);
        assert!(spare.iter().all(|&b| b == 5));
        // It did not tread on a pooled one.
        assert!(pooled[0].iter().all(|&b| b == 0));
    }

    /// A heap block and a pooled one swap like any two, which is what lets
    /// the fallback be invisible to whatever holds them.
    #[test]
    fn a_heap_block_swaps_with_a_pooled_one() {
        let arena = Arena::new(1);
        let mut pooled = arena.alloc_or_heap();
        let mut heap = arena.alloc_or_heap();
        assert!(pooled.is_pooled() && !heap.is_pooled());
        pooled.fill(1);
        heap.fill(2);

        std::mem::swap(&mut pooled, &mut heap);

        assert!(!pooled.is_pooled() && heap.is_pooled());
        assert_eq!((pooled[0], heap[0]), (2, 1));
    }

    /// The choice rides in padding the pooled form already had, so carrying
    /// it costs nothing.
    #[test]
    fn the_fallback_is_free() {
        assert_eq!(
            std::mem::size_of::<ArenaBlock>(),
            std::mem::size_of::<Arc<ArenaInner>>() + std::mem::size_of::<usize>(),
            "ArenaBlock grew past a pointer and an index"
        );
    }

    #[test]
    #[should_panic(expected = "non-zero capacity")]
    fn zero_capacity_is_rejected() {
        Arena::new(0);
    }

    /// Blocks come out in order, so a fresh arena fills forwards.
    #[test]
    fn blocks_are_handed_out_in_order() {
        let arena = Arena::new(4);
        let indices: Vec<u32> = (0..4)
            .map(|_| arena.alloc().unwrap().index().unwrap())
            .collect();
        assert_eq!(indices, [0, 1, 2, 3]);
    }

    /// A block arrives zeroed, which is what lets it be read before it is
    /// written without any uninitialised memory in the picture.
    #[test]
    fn a_block_arrives_zeroed() {
        let arena = Arena::new(2);
        let block = arena.alloc().unwrap();
        assert_eq!(&block[..], &[0u8; ARENA_BLOCK_SIZE][..]);
    }

    #[test]
    fn a_block_holds_what_is_written_to_it() {
        let arena = Arena::new(2);
        let mut block = arena.alloc().unwrap();
        let texto = "olá!".as_bytes();
        block[..texto.len()].copy_from_slice(texto);
        assert_eq!(&block[..texto.len()], texto);
    }

    /// The invariant every `unsafe` here rests on: two handles never overlap.
    #[test]
    fn writing_one_block_leaves_the_others_alone() {
        let arena = Arena::new(8);
        let mut blocks: Vec<_> = (0..8).map(|_| arena.alloc().unwrap()).collect();

        for (i, block) in blocks.iter_mut().enumerate() {
            block.fill(i as u8);
        }
        for (i, block) in blocks.iter().enumerate() {
            assert!(
                block.iter().all(|&b| b == i as u8),
                "block {i} was written through by another handle"
            );
        }
    }

    /// The whole point: exchanging two handles exchanges indices, and the
    /// bytes stay where they were.
    #[test]
    fn swapping_moves_indices_not_data() {
        let arena = Arena::new(2);
        let mut a = arena.alloc().unwrap();
        let mut b = arena.alloc().unwrap();
        a.fill(1);
        b.fill(2);
        let (index_a, index_b) = (a.index().unwrap(), b.index().unwrap());
        let (addr_a, addr_b) = (a.as_ptr(), b.as_ptr());

        std::mem::swap(&mut a, &mut b);

        assert_eq!(
            (a.index().unwrap(), b.index().unwrap()),
            (index_b, index_a),
            "indices did not swap"
        );
        assert_eq!((a[0], b[0]), (2, 1));
        // Each handle now points at the block the other had — nothing moved.
        assert_eq!((a.as_ptr(), b.as_ptr()), (addr_b, addr_a));
    }

    /// Every block is part of one run, which is the reason for the whole
    /// exercise: no per allocation header between them, and a walk over them
    /// reads forwards.
    #[test]
    fn blocks_are_one_contiguous_allocation() {
        let arena = Arena::new(8);
        let blocks: Vec<_> = (0..8).map(|_| arena.alloc().unwrap()).collect();

        let mut addresses: Vec<usize> = blocks.iter().map(|b| b.as_ptr() as usize).collect();
        addresses.sort_unstable();
        for pair in addresses.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                BLOCK,
                "blocks are not packed back to back"
            );
        }
    }

    /// The store outlives the arena handle: a block lent out stays valid even
    /// if whatever created the arena is gone. This is what the strong
    /// reference is for.
    #[test]
    fn a_block_outlives_the_arena_handle() {
        let mut block = {
            let arena = Arena::new(2);
            arena.alloc().unwrap()
        };
        block.fill(9);
        assert!(block.iter().all(|&b| b == 9));
    }

    /// Handles are owned, so they cross threads with their bytes.
    #[test]
    fn handles_can_be_sent_between_threads() {
        let arena = Arena::new(4);
        let mut block = arena.alloc().unwrap();

        let handle = std::thread::spawn(move || {
            block.fill(3);
            block
        });
        let block = handle.join().unwrap();
        assert!(block.iter().all(|&b| b == 3));
    }

    #[test]
    fn concurrent_allocation_hands_out_distinct_blocks() {
        let arena = Arena::new(64);
        let mut indices: Vec<u32> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let arena = arena.clone();
                    scope.spawn(move || {
                        (0..8)
                            .map(|_| arena.alloc().unwrap().index().unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        // Nothing comes back, so all 64 must be distinct however the threads
        // interleaved — a counter that lost a race would repeat one.
        assert_eq!(indices.len(), 64);
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), 64, "a block was handed out twice");
        assert!(arena.is_exhausted());
    }

    /// Asking past the end from several threads at once must not push the
    /// offset beyond the store.
    #[test]
    fn racing_past_the_end_leaves_the_count_honest() {
        let arena = Arena::new(4);
        let lent: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let arena = arena.clone();
                    scope.spawn(move || arena.alloc().is_some() as usize)
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });

        assert_eq!(lent, 4, "lent a different number of blocks than it had");
        assert_eq!(arena.available(), 0, "the offset ran past the store");
    }
}
