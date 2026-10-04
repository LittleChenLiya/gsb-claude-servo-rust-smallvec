//! Transactional edit window for [`SmallVec`].
//!
//! This module provides [`Transaction`], an edit window that stages insertions
//! and removal ranges for a [`SmallVec`]. The underlying `SmallVec` is only
//! mutated when the transaction is [`commit`](Transaction::commit)ed; dropping
//! the transaction (either explicitly via [`rollback`](Transaction::rollback)
//! or during unwinding) restores the original element order and drops only the
//! inserted values.
//!
//! # Borrow semantics
//!
//! A `Transaction` holds an exclusive (`&mut`) borrow of the [`SmallVec`] for
//! its entire lifetime. While the transaction is alive, the original container
//! cannot be borrowed, moved, or otherwise accessed from outside - the borrow
//! checker enforces this. The edit window is therefore the *only* way to
//! observe or modify the container during the transaction.
//!
//! On [`commit`](Transaction::commit) the borrow is consumed and the container
//! is returned to the caller with the staged changes applied. On rollback
//! (drop) the borrow ends and the container is restored to its original state.
//!
//! # Commit lifetime
//!
//! [`commit`](Transaction::commit) consumes the transaction by value. It
//! returns a [`SmallVec`] containing the elements that were removed by staged
//! removal ranges, in their original order. The edited container is modified
//! in place (through the held `&mut`) and is owned by the caller once the
//! transaction is consumed.
//!
//! Removed elements are *moved* out of the container (no [`Clone`] bound is
//! required) and handed to the caller. Inserted elements that were staged but
//! subsequently removed are dropped during commit; inserted elements that
//! remain in the final sequence are moved into the container.
//!
//! # Rollback guarantees
//!
//! On rollback the original elements are still owned by the restored container;
//! none of them are dropped or moved. Only the values that were inserted
//! during the transaction are dropped. This holds both for explicit rollback
//! and for unwinding after a panic.
//!
//! # Inline storage
//!
//! After a commit or rollback the container's length, contents, and inline
//! storage state match the documented guarantees of [`SmallVec`]: when the
//! final length is at most the inline capacity `N`, the container is stored
//! inline (not spilled), regardless of whether it was spilled before the
//! transaction. Capacity growth during editing is permitted.

use crate::{
    allocator::{
        Allocator,
        Global
    },
    newrange::NewRange,
    SmallVec
};
use core::ops::RangeBounds;

/// A staged item in the edit window.
///
/// Either an original element (referenced by its index in the container when
/// the transaction began) or a pending insertion (referenced by its index in
/// the transaction's insertion buffer).
enum Item {
    /// An original element, identified by its index at transaction start.
    Orig(usize),
    /// A pending insertion, identified by its index in the insertion buffer.
    New(usize)
}

/// A transactional edit window for a [`SmallVec`].
///
/// `Transaction` provides a staging area for insertions and removal ranges.
/// The container is not mutated until [`commit`](Transaction::commit) is
/// called; dropping the transaction restores the original element order.
///
/// See the [module-level documentation](self) for borrow and commit semantics.
///
/// # Examples
///
/// ```
/// use smallvec::{SmallVec, transaction::Transaction};
///
/// let mut vec = SmallVec::<u32, 4>::from_buf([1, 2, 3]);
/// let mut tx = Transaction::begin(&mut vec);
/// tx.remove(1..2);
/// tx.insert(1, 9);
/// let removed = tx.commit();
/// assert_eq!(&*vec, &[1, 9, 3]);
/// assert_eq!(&*removed, &[2]);
/// ```
pub struct Transaction<'a, T, const N: usize, A: Allocator + Clone = Global> {
    /// The container being edited.
    ///
    /// Held exclusively for the lifetime of the transaction. The container is
    /// not mutated until commit; on rollback it is untouched.
    vec: &'a mut SmallVec<T, N, A>,
    /// The logical working sequence.
    ///
    /// Each entry is either an original element or a pending insertion. This
    /// is the sequence that will be materialized into the container on commit.
    working: alloc::vec::Vec<Item>,
    /// Pending insertion values.
    ///
    /// `Some` for values that have not yet been moved into the container;
    /// `None` after they have been moved during commit. Dropped on rollback.
    inserts: alloc::vec::Vec<Option<T>>,
    /// Original indices that have been staged for removal.
    ///
    /// Sorted and extracted in order on commit.
    removed: alloc::vec::Vec<usize>
}

impl<'a, T, const N: usize, A: Allocator + Clone> Transaction<'a, T, N, A> {
    /// Begins a transactional edit window over `vec`.
    ///
    /// The transaction records the current length and element order. The
    /// container is not mutated until [`commit`](Self::commit) is called.
    ///
    /// # Examples
    ///
    /// ```
    /// use smallvec::{SmallVec, transaction::Transaction};
    ///
    /// let mut vec = SmallVec::<u32, 4>::from_buf([1, 2, 3]);
    /// let tx = Transaction::begin(&mut vec);
    /// // `vec` cannot be borrowed here - `tx` holds an exclusive borrow.
    /// ```
    pub fn begin(vec: &'a mut SmallVec<T, N, A>) -> Self {
        let original_len = vec.len();
        let working = (0..original_len).map(Item::Orig).collect();
        Transaction {
            vec,
            working,
            inserts: alloc::vec::Vec::new(),
            removed: alloc::vec::Vec::new()
        }
    }

    /// Inserts `value` at `index` in the working sequence.
    ///
    /// The insertion is staged; the container is not mutated until commit.
    ///
    /// # Panics
    ///
    /// Panics if `index > len`, where `len` is the current length of the
    /// working sequence.
    pub fn insert(&mut self, index: usize, value: T) {
        let len = self.working.len();
        if index > len {
            #[cold]
            #[inline(never)]
            #[track_caller]
            fn assert_failed(index: usize, len: usize) -> ! {
                panic!("insertion index (is {index}) should be <= length (is {len})");
            }
            assert_failed(index, len);
        }
        let idx = self.inserts.len();
        self.inserts.push(Some(value));
        self.working.insert(index, Item::New(idx));
    }

    /// Stages a removal of the elements in `range`.
    ///
    /// The removal is recorded; the container is not mutated until commit.
    /// Removed elements are returned by [`commit`](Self::commit).
    ///
    /// # Panics
    ///
    /// Panics if `range` is out of bounds for the current working length. The
    /// range is validated before any element is moved.
    pub fn remove<R>(&mut self, range: R)
    where
        R: RangeBounds<usize>
    {
        let len = self.working.len();
        let core::ops::Range {
            start,
            end
        } = core::ops::Range::new(range, len);

        // Drain the range from the working sequence, recording original
        // indices for extraction on commit. Insertions removed here are
        // dropped (they are not in the final sequence).
        let removed_items: alloc::vec::Vec<Item> = self.working.drain(start..end).collect();
        for item in removed_items {
            if let Item::Orig(i) = item {
                self.removed.push(i);
            }
        }
    }

    /// Commits the transaction, applying all staged changes.
    ///
    /// Returns a [`SmallVec`] containing the elements removed by staged
    /// removal ranges, in their original order. The edited container is
    /// modified in place.
    ///
    /// On success the final length, contents, and inline storage state match
    /// the documented [`SmallVec`] guarantees.
    ///
    /// # Panics
    ///
    /// Panics if memory allocation fails while materializing the result. In
    /// that case no elements have been moved yet, so the container is
    /// restored to its original state (the transaction is rolled back).
    pub fn commit(mut self) -> SmallVec<T, N, A> {
        let final_len = self.working.len();
        let original_len = self.vec.len();

        // Sanity check: all staged removal indices refer to original elements.
        debug_assert!(self.removed.iter().all(|&i| i < original_len));

        // Materialize the final sequence in a fresh container with the same
        // allocator. Starting inline guarantees the final inline storage state
        // when `final_len <= N`.
        let alloc = self.vec.allocator.clone();
        let mut result = SmallVec::new_in(alloc.clone());

        // Reserve capacity before moving elements. If allocation fails here,
        // no moves have happened and the container is untouched.
        result.reserve(final_len);

        // SAFETY: we set the length to zero so the container does not drop the
        // original elements. The elements remain initialized in their slots
        // and are moved out below. The container's storage is not deallocated
        // until it is replaced at the end of this function.
        unsafe { self.vec.set_len(0) };

        // Move the working sequence into `result`.
        for item in &self.working {
            match item {
                Item::Orig(i) => {
                    // Move original `i` out of the container.
                    // SAFETY: `i < original_len <= capacity`, so the slot is
                    // in bounds and initialized.
                    let elem = unsafe { self.vec.as_ptr().add(*i).read() };
                    result.push(elem);
                }
                Item::New(j) => {
                    // Move the pending insertion into `result`.
                    let elem = self.inserts[*j]
                        .take()
                        .expect("insertion should not have been moved yet");
                    result.push(elem);
                }
            }
        }

        // Extract removed originals in original order.
        self.removed.sort();
        let mut removed_result = SmallVec::new_in(alloc);
        for &i in &self.removed {
            // SAFETY: `i < original_len`, and this slot has not been read yet
            // (removed originals are not in the working sequence).
            let elem = unsafe { self.vec.as_ptr().add(i).read() };
            removed_result.push(elem);
        }

        // Replace the edited container with the materialized result. The old
        // container has length zero and all slots moved-from, so dropping it
        // releases only its storage (if heap) without dropping elements.
        let _ = core::mem::replace(self.vec, result);

        removed_result
    }

    /// Rolls back the transaction, restoring the original element order.
    ///
    /// This is equivalent to dropping the transaction. The original elements
    /// are still owned by the restored container; only inserted values are
    /// dropped.
    pub fn rollback(self) {
        // Dropping `self` drops the insertion buffer, restoring the original
        // element order. The container is untouched.
    }
}
