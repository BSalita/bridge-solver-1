//! A compact, pooled growable vector of [`Pattern`].
//!
//! This exists for one reason: the pattern tree is where this solver's memory
//! goes, and `Vec<Pattern>` is an expensive way to hold it. On the reference's
//! own `deals/freak/deal.1` the notrump strain builds **15.1 million** pattern
//! nodes, and measured against the C++ reference on an identical search tree we
//! peaked at 1,696 MB where it peaked at 1,107 MB. The difference is entirely
//! storage:
//!
//! * `Vec` is three words. This is two: a pointer, and a `u32` length and
//!   capacity. That takes `Pattern` from 64 bytes to 56, which over 15.1M
//!   nodes is 115 MB.
//! * `Vec` asks the allocator for every children list and rounds capacity by
//!   doubling, so each block carries a malloc header and up to 2x slack. Blocks
//!   here come from per-power-of-two free lists refilled in slabs, so a block is
//!   exactly its capacity and costs no header.
//!
//! It is a port of the reference's `Vector<T>` and `VectorPool<T>`
//! (`610e9da`), and like that one it recycles blocks rather than returning them
//! to the allocator -- see [`drain_pool`] for the consequence and the escape
//! hatch.
//!
//! Nothing here changes what the search does. Capacity growth is by doubling in
//! both, and element order is untouched, which matters more than it sounds:
//! `Pattern::lookup` returns the first child that matches, so child order is
//! semantics. The lock-step fixtures in `fixtures/divergence` are what hold
//! that.

use crate::pattern::Pattern;
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::RefCell;
use std::mem::{align_of, size_of};

/// Free lists are indexed by `log2(capacity)`. A children list cannot come
/// close to 2^24 patterns, so a class beyond this is a bug rather than a case
/// to handle.
const NUM_CLASSES: usize = 24;

/// Bytes per slab. Kept small deliberately: a bigger slab amortises the
/// allocator call further but leaves a size class holding a cold backlog well
/// beyond its working set, which is the opposite of the point on the very
/// trees that motivated this.
const SLAB_BYTES: usize = 8192;

thread_local! {
    /// One LIFO free list per size class, per thread. Per-thread rather than
    /// shared because solves are independent and a lock here would sit on the
    /// hottest path in the program.
    static POOL: RefCell<Vec<Vec<*mut Pattern>>> =
        RefCell::new((0..NUM_CLASSES).map(|_| Vec::new()).collect());

    /// Every slab allocated, per size class: the pointers the allocator
    /// actually handed out, and so the only ones it may be given back. Blocks
    /// are carved out of these, so a block pointer is in general *inside* an
    /// allocation, and freeing one is undefined behaviour.
    static SLABS: RefCell<Vec<Vec<*mut u8>>> =
        RefCell::new((0..NUM_CLASSES).map(|_| Vec::new()).collect());
}

/// Return every pooled block to the allocator.
///
/// The pool never shrinks on its own, so a thread that has once solved a
/// freakish deal holds that peak for its lifetime -- around a gigabyte in the
/// worst case measured. That is what the C++ reference does too, and it is the
/// right default for a process that solves deal after deal, but it is the wrong
/// default for a long-lived thread that solved one hard deal and moved on.
/// Call this at such a boundary.
///
/// # Safety of the timing, not of the call
///
/// The call itself is safe: memory goes back to the allocator a whole slab at
/// a time, and only once every block carved from that slab has been returned
/// to the pool. A slab with a block still held by a live [`PatternVec`] is
/// kept, along with its free blocks, until a later drain. It is sound to call
/// at any time.
///
/// It used to free every pooled block on its own, as if each were a separate
/// allocation; most are slices of an 8 KB slab, so that handed the allocator
/// pointers it never issued. macOS aborted on the first drain after a solve,
/// and other allocators would have corrupted their heaps instead.
pub fn drain_pool() {
    // A retained table cache holds live blocks, and live blocks are exactly
    // the ones a drain cannot touch. Dropping this thread's shared
    // `TableSolver` first returns them, so what follows frees the peak rather
    // than everything around it.
    crate::par::release_shared();
    POOL.with(|pool| {
        SLABS.with(|slabs| {
            let mut pool = pool.borrow_mut();
            let mut slabs = slabs.borrow_mut();
            for class in 0..NUM_CLASSES {
                drain_class(class, &mut pool[class], &mut slabs[class]);
            }
        })
    });
}

/// Free the slabs of one size class whose blocks are all in `free`, and drop
/// those blocks from it.
fn drain_class(class: usize, free: &mut Vec<*mut Pattern>, slabs: &mut Vec<*mut u8>) {
    if free.is_empty() || slabs.is_empty() {
        return;
    }
    let (block_bytes, blocks) = slab_shape(class);
    let available: std::collections::HashSet<usize> =
        free.iter().map(|&block| block as usize).collect();
    let mut released = std::collections::HashSet::new();
    slabs.retain(|&slab| {
        let start = slab as usize;
        let whole = (0..blocks).all(|i| available.contains(&(start + i * block_bytes)));
        if whole {
            // SAFETY: `slab` came from `alloc` in `pool_alloc` with exactly
            // this layout, and every block carved from it is on the free
            // list, so nothing points into it any more. It leaves `slabs`
            // here and its blocks leave the free list below, so it is freed
            // once.
            unsafe { dealloc(slab, slab_layout(class)) };
            released.extend((0..blocks).map(|i| start + i * block_bytes));
        }
        !whole
    });
    if !released.is_empty() {
        free.retain(|&block| !released.contains(&(block as usize)));
    }
}

/// Bytes per block of `class`, and blocks per slab.
fn slab_shape(class: usize) -> (usize, usize) {
    let block_bytes = (1usize << class) * size_of::<Pattern>();
    (block_bytes, (SLAB_BYTES / block_bytes).max(1))
}

/// The layout a slab of `class` is allocated with.
fn slab_layout(class: usize) -> Layout {
    let (block_bytes, blocks) = slab_shape(class);
    match Layout::from_size_align(block_bytes * blocks, align_of::<Pattern>()) {
        Ok(layout) => layout,
        // Unreachable: a power of two times a type's size, times a small
        // count, at that type's own alignment. Aborting beats a panic in a
        // library.
        Err(_) => std::process::abort(),
    }
}

/// Take a block holding `1 << class` patterns.
fn pool_alloc(class: usize) -> *mut Pattern {
    POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if let Some(block) = pool[class].pop() {
            return block;
        }

        // Refill: one allocator call for as many blocks as fit in a slab, so
        // the call cost is amortised across all of them.
        let (block_bytes, blocks) = slab_shape(class);
        let layout = slab_layout(class);
        // SAFETY: the layout is non-zero, since `block_bytes` is at least
        // `size_of::<Pattern>()` and `blocks` at least one.
        let slab = unsafe { alloc(layout) };
        if slab.is_null() {
            handle_alloc_error(layout);
        }
        // Remembered so `drain_pool` can give back exactly what was allocated.
        SLABS.with(|slabs| slabs.borrow_mut()[class].push(slab));
        for i in 1..blocks {
            // SAFETY: `i < blocks`, so this stays inside the slab.
            pool[class].push(unsafe { slab.add(i * block_bytes) } as *mut Pattern);
        }
        slab as *mut Pattern
    })
}

/// Give a block back. It is not freed, only made available again.
fn pool_free(block: *mut Pattern, class: usize) {
    // `try_with` rather than `with`, because this runs from `Drop` and one of
    // the trees reaching it is the thread's shared `TableSolver`, whose
    // thread-local is registered before the pool's and so torn down after it.
    // On that last pass the pool is already gone and the block is dropped on
    // the floor -- which is what happens to every pooled block at thread exit
    // anyway, since the free lists hold raw pointers and free nothing when
    // they drop. `with` would panic here, and a panic in a destructor during
    // cleanup aborts the process.
    let _ = POOL.try_with(|pool| pool.borrow_mut()[class].push(block));
}

/// `log2` of the capacity needed to hold `n`, rounded up, minimum one element.
fn class_for(n: usize) -> usize {
    let cap = n.next_power_of_two().max(1);
    cap.trailing_zeros() as usize
}

/// A growable vector of [`Pattern`] in two words.
pub struct PatternVec {
    /// Dangling when `cap == 0`; no allocation is made for an empty list, which
    /// is the common case for a leaf pattern.
    ptr: *mut Pattern,
    len: u32,
    cap: u32,
}

impl PatternVec {
    pub const fn new() -> Self {
        PatternVec {
            ptr: std::ptr::NonNull::dangling().as_ptr(),
            len: 0,
            cap: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_slice(&self) -> &[Pattern] {
        // SAFETY: `ptr` is valid for `len` initialised patterns whenever
        // `len > 0`, and `from_raw_parts` accepts a dangling pointer at zero
        // length provided it is aligned, which `NonNull::dangling` is.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [Pattern] {
        // SAFETY: as `as_slice`, and `&mut self` rules out aliasing.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len as usize) }
    }

    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Pattern> {
        self.as_slice().iter()
    }

    #[inline]
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Pattern> {
        self.as_mut_slice().iter_mut()
    }

    /// Grow to hold at least `wanted`, doubling as `Vec` does so that the tree
    /// shape and the number of reallocations match the reference's.
    fn reserve_exact_class(&mut self, wanted: usize) {
        if wanted <= self.cap as usize {
            return;
        }
        let new_class = class_for(wanted);
        let new_cap = 1usize << new_class;
        let new_ptr = pool_alloc(new_class);
        if self.len > 0 {
            // SAFETY: the two blocks are distinct and non-overlapping (the new
            // one has just come off a free list, so nothing else holds it), and
            // both are valid for `len` patterns.
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len as usize);
            }
        }
        if self.cap > 0 {
            // The elements were moved, not dropped: the old block holds no live
            // patterns now, so it goes straight back to the pool.
            pool_free(self.ptr, class_for(self.cap as usize));
        }
        self.ptr = new_ptr;
        self.cap = new_cap as u32;
    }

    pub fn push(&mut self, value: Pattern) {
        let len = self.len as usize;
        self.reserve_exact_class(len + 1);
        // SAFETY: `reserve_exact_class` guarantees `cap > len`, so this slot is
        // inside the block and currently uninitialised.
        unsafe { std::ptr::write(self.ptr.add(len), value) };
        self.len += 1;
    }

    /// Remove element `i`, moving the last element into its place.
    ///
    /// Matches `Vec::swap_remove`, and the reference's `Delete`, which is the
    /// same operation written out. The reordering it causes is deliberate and
    /// load-bearing; see `Pattern::update`.
    pub fn swap_remove(&mut self, i: usize) -> Pattern {
        let len = self.len as usize;
        assert!(i < len, "swap_remove index {i} out of range for {len}");
        // SAFETY: `i` and `len - 1` are both in range and initialised. The
        // value at `i` is moved out and the last element written over it, so
        // exactly one copy of each pattern survives.
        unsafe {
            let taken = std::ptr::read(self.ptr.add(i));
            if i != len - 1 {
                std::ptr::copy_nonoverlapping(self.ptr.add(len - 1), self.ptr.add(i), 1);
            }
            self.len -= 1;
            taken
        }
    }

    /// Move every element of `other` onto the end of `self`, leaving it empty.
    pub fn append(&mut self, other: &mut PatternVec) {
        if other.is_empty() {
            return;
        }
        let (len, other_len) = (self.len as usize, other.len as usize);
        self.reserve_exact_class(len + other_len);
        // SAFETY: the blocks are distinct (`other` is a separate vector), the
        // destination has room after the reserve, and `other.len` is set to
        // zero so the moved patterns are owned by `self` alone.
        unsafe {
            std::ptr::copy_nonoverlapping(other.ptr, self.ptr.add(len), other_len);
        }
        self.len += other_len as u32;
        other.len = 0;
    }

    /// Drop every element, keeping the block.
    pub fn clear(&mut self) {
        let len = self.len as usize;
        self.len = 0;
        for i in 0..len {
            // SAFETY: each index below the old length holds an initialised
            // pattern, and `len` is already zero so a panic in a nested drop
            // cannot cause a double free.
            unsafe { std::ptr::drop_in_place(self.ptr.add(i)) };
        }
    }
}

impl Default for PatternVec {
    fn default() -> Self {
        PatternVec::new()
    }
}

impl Drop for PatternVec {
    fn drop(&mut self) {
        self.clear();
        if self.cap > 0 {
            pool_free(self.ptr, class_for(self.cap as usize));
            self.cap = 0;
        }
    }
}

impl Clone for PatternVec {
    fn clone(&self) -> Self {
        let mut copy = PatternVec::new();
        if self.len > 0 {
            copy.reserve_exact_class(self.len as usize);
            for (i, pattern) in self.iter().enumerate() {
                // SAFETY: the reserve above guarantees room for `len`, and each
                // slot is written exactly once.
                unsafe { std::ptr::write(copy.ptr.add(i), pattern.clone()) };
            }
            copy.len = self.len;
        }
        copy
    }
}

impl std::ops::Index<usize> for PatternVec {
    type Output = Pattern;

    #[inline]
    fn index(&self, i: usize) -> &Pattern {
        &self.as_slice()[i]
    }
}

impl std::ops::IndexMut<usize> for PatternVec {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Pattern {
        &mut self.as_mut_slice()[i]
    }
}

impl<'a> IntoIterator for &'a PatternVec {
    type Item = &'a Pattern;
    type IntoIter = std::slice::Iter<'a, Pattern>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// SAFETY: `PatternVec` owns its block exclusively and `Pattern` is `Send`, so
// moving one between threads moves the whole tree with it. It is deliberately
// not `Sync`: the pool it draws from is per-thread.
unsafe impl Send for PatternVec {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks pooled for `class`, and slabs held for it, on this thread.
    fn counts(class: usize) -> (usize, usize) {
        let pooled = POOL.with(|p| p.borrow()[class].len());
        let slabs = SLABS.with(|s| s.borrow()[class].len());
        (pooled, slabs)
    }

    fn filled(n: usize) -> PatternVec {
        let mut v = PatternVec::new();
        for _ in 0..n {
            v.push(Pattern::default());
        }
        v
    }

    #[test]
    fn draining_after_a_solve_frees_whole_slabs() {
        // This aborted on macOS when drain_pool freed blocks one by one:
        // most blocks are slices of a slab, not allocations of their own.
        let deal = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";
        let hands = crate::Hands::from_pbn(deal).expect("deal");
        let first =
            crate::par::TableSolver::new().solve_strain_hands(hands, bridge_types::Strain::Spades);
        drain_pool();
        let again =
            crate::par::TableSolver::new().solve_strain_hands(hands, bridge_types::Strain::Spades);
        drain_pool();
        assert_eq!(first, again);
    }

    #[test]
    fn a_live_block_keeps_its_slab() {
        // Class 0 holds one pattern a block, so a slab has many blocks.
        drain_pool();
        let (_, slabs_before) = counts(0);
        let kept = filled(1);
        let freed: Vec<PatternVec> = (0..5).map(|_| filled(1)).collect();
        drop(freed);
        assert!(counts(0).1 > slabs_before, "a slab was allocated");

        // `kept` still holds a block of the newest slab, so that slab stays.
        drain_pool();
        let (pooled, slabs) = counts(0);
        assert!(slabs > slabs_before);
        assert!(pooled > 0, "its free blocks stay pooled with it");
        assert_eq!(kept.len(), 1);

        drop(kept);
        drain_pool();
        assert_eq!(counts(0).1, slabs_before, "now the slab is free, it goes");
    }
}
