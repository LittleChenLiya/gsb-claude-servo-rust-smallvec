//! A transactional editing window on top of [`SmallVec`].
//!
//! [`Transaction`] takes an exclusive, reborrowed hold of a [`SmallVec`] for
//! the duration of an edit. While the transaction is alive the outside world
//! cannot touch the original vector: its contents are never modified, so
//! elements do not need to be [`Clone`] (they are moved at most once). Staged
//! insertions live in separate heap storage owned by the transaction, and
//! staged removals are recorded as ranges of *original* indices only.
//!
//! The original vector is touched in one of two ways:
//!
//! * [`Transaction::commit`] / [`Transaction::try_commit`] merge all staged
//!   edits into the vector and hand ownership of every removed element back to
//!   the caller as a `SmallVec<T, N>`. Elements that were merely relocated keep
//!   exactly one owner; they are moved, never copied.
//! * [`Transaction::abort`] (or simply dropping the transaction, including an
//!   unwind during an edit) leaves the original vector untouched and drops only
//!   the values inserted during the transaction.
//!
//! # Editing model
//!
//! All coordinates refer to the **original** vector captured when the
//! transaction started:
//!
//! * [`Transaction::stage_remove`] records a half-open range `start..end` of
//!   original indices. Removal ranges must be in bounds and must not overlap
//!   (adjacent ranges are fine).
//! * [`Transaction::insert`] stores a value that will appear before original
//!   index `pos` after a commit (`0` = front, `original_len()` = back). Values
//!   inserted at the same position keep their insertion order.
//!
//! Every range and every insertion position is validated eagerly, while the
//! transaction still only owns side storage. No element of the original vector
//! is moved (and no allocation for the result is attempted) before all
//! validation has succeeded.
//!
//! # Borrowing lifetime
//!
//! A transaction is created through [`SmallVec::transaction`], which takes
//! `&mut self` for the lifetime of the transaction. The mutable borrow of the
//! vector cannot be released while the transaction is alive, and it ends
//! exactly when the transaction is consumed by [`commit`](Transaction::commit)
//! or [`abort`](Transaction::abort), or when it is dropped. Inserted values may
//! borrow from data that outlives the transaction.
//!
//! # Commit lifetime and ownership
//!
//! A successful commit:
//!
//! 1. allocates room for the new contents and for the removed elements (with
//!    [`SmallVec::try_reserve_exact`], so capacity may grow, but when the final
//!    length fits within the inline capacity `N` the vector ends up inline);
//! 2. moves every surviving original element and every staged insertion into
//!    the result vector exactly once;
//! 3. moves every removed original element into the returned
//!    `SmallVec<T, N>`, which the caller now owns.
//!
//! After the commit the transaction is consumed and the borrow of the original
//! vector is released. If [`Transaction::try_commit`] cannot allocate, the
//! transaction is returned by the error and the original vector is unchanged:
//! the caller may keep editing, commit later, or abort.
//!
//! # Example
//!
//! ```
//! use smallvec::SmallVec;
//!
//! // `Token` is intentionally not `Clone`.
//! struct Token(char);
//!
//! let mut v: SmallVec<Token, 4> = SmallVec::new();
//! v.push(Token('a'));
//! v.push(Token('b'));
//! v.push(Token('c'));
//!
//! // Abandon an edit: only the inserted value is dropped.
//! {
//!     let mut tx = v.transaction();
//!     tx.stage_remove(1..2); // remove `b`
//!     tx.insert(1, Token('x'));
//!     tx.abort();
//! }
//! assert_eq!(v.iter().map(|t| t.0).collect::<Vec<_>>(), ['a', 'b', 'c']);
//!
//! // Commit an edit: `b` escapes through the removal result.
//! let mut tx = v.transaction();
//! tx.stage_remove(1..2);
//! tx.insert(1, Token('x'));
//! let removed = tx.commit();
//! assert_eq!(v.iter().map(|t| t.0).collect::<Vec<_>>(), ['a', 'x', 'c']);
//! assert_eq!(removed.iter().map(|t| t.0).collect::<Vec<_>>(), ['b']);
//! ```

use {
    crate::{SmallVec, SmallVecError, allocator::Vec},
    core::ops::{Bound, RangeBounds},
};

/// A staged insertion: a value emitted before original index `pos`.
struct Pending<T> {
    pos: usize,
    value: T,
}

/// A transactional editing window borrowing a [`SmallVec`] exclusively.
///
/// See the [module documentation](self) for the editing, borrowing and
/// ownership rules.
pub struct Transaction<'a, T, const N: usize> {
    vec: &'a mut SmallVec<T, N>,
    /// Original-index ranges staged for removal; sorted and pairwise disjoint.
    removals: Vec<(usize, usize)>,
    /// Staged insertions; sorted stably by `pos` immediately before a commit.
    inserts: Vec<Pending<T>>,
}

impl<'a, T, const N: usize> Transaction<'a, T, N> {
    /// Opens a transaction over `vec`.
    ///
    /// Normally reached through [`SmallVec::transaction`]. The borrow of `vec`
    /// lasts until the transaction is consumed or dropped.
    pub(crate) fn new(vec: &'a mut SmallVec<T, N>) -> Self {
        Transaction {
            vec,
            removals: Vec::new(),
            inserts: Vec::new(),
        }
    }

    /// Length of the original vector captured by the transaction.
    #[inline]
    pub fn original_len(&self) -> usize {
        self.vec.len()
    }

    /// Whether the original vector captured by the transaction is empty.
    #[inline]
    pub fn is_original_empty(&self) -> bool {
        self.vec.is_empty()
    }

    /// Number of elements currently staged for removal.
    #[inline]
    pub fn staged_removal_len(&self) -> usize {
        self.removals.iter().map(|&(start, end)| end - start).sum()
    }

    /// Number of values currently staged for insertion.
    #[inline]
    pub fn staged_insert_len(&self) -> usize {
        self.inserts.len()
    }

    /// Normalizes a range against the original length, rejecting out-of-bounds
    /// ranges before any element is moved.
    fn check_range<R: RangeBounds<usize>>(&self, range: R) -> (usize, usize) {
        let start = match range.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s.checked_add(1).expect("range start overflow"),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&e) => e.checked_add(1).expect("range end overflow"),
            Bound::Excluded(&e) => e,
            Bound::Unbounded => self.original_len(),
        };
        assert!(
            start <= end && end <= self.original_len(),
            "removal range {start}..{end} out of bounds for length {}",
            self.original_len()
        );
        (start, end)
    }

    /// Records `range` as a section of *original* indices removed on commit.
    ///
    /// Nothing is removed yet: the original vector keeps owning the elements
    /// until a successful [`commit`](Self::commit). The range must be in bounds
    /// and must not overlap a previously staged range; adjacent ranges are
    /// allowed.
    ///
    /// # Panics
    ///
    /// Panics if `range` is out of bounds or overlaps an already staged
    /// removal. Validation happens while only the transaction's side storage
    /// exists, so a rejected range never moves or drops any element of the
    /// original vector.
    pub fn stage_remove<R: RangeBounds<usize>>(&mut self, range: R) {
        let (start, end) = self.check_range(range);

        // Empty ranges need no bookkeeping.
        if start == end {
            return;
        }

        // Disjointness check against the recorded, sorted ranges. This is also
        // where an illegal range is refused, strictly before any move.
        match self.removals.binary_search_by_key(&start, |&(s, _)| s) {
            Ok(_) => panic!("overlapping removal ranges staged at index {start}"),
            Err(idx) => {
                if idx > 0 {
                    let prev_end = self.removals[idx - 1].1;
                    assert!(
                        start >= prev_end,
                        "removal range {start}..{end} overlaps a previously staged range"
                    );
                }
                if idx < self.removals.len() {
                    let next_start = self.removals[idx].0;
                    assert!(
                        end <= next_start,
                        "removal range {start}..{end} overlaps a previously staged range"
                    );
                }
                self.removals.insert(idx, (start, end));
            }
        }
    }

    /// Validates an insertion position expressed against the original vector.
    fn check_position(&self, pos: usize) {
        assert!(
            pos <= self.original_len(),
            "insertion position {pos} out of bounds for length {}",
            self.original_len()
        );
    }

    /// Stages `value` for insertion before original index `pos`.
    ///
    /// `pos == 0` inserts at the front and `pos == original_len()` appends.
    /// Several values inserted at the same position keep their insertion order
    /// after a commit. The inserted value is owned by the transaction; on abort
    /// (or an unwind) it, and only it, is dropped.
    ///
    /// # Panics
    ///
    /// Panics if `pos > original_len()`. As with
    /// [`stage_remove`](Self::stage_remove), the check runs before any element
    /// is moved.
    pub fn insert(&mut self, pos: usize, value: T) {
        self.check_position(pos);
        self.inserts.push(Pending { pos, value });
    }

    /// Abandons the transaction.
    ///
    /// The borrowed vector is left untouched, in its original length, content
    /// and storage state. Every value inserted during the transaction is
    /// dropped; original elements are not dropped.
    pub fn abort(self) {
        // Dropping `self` drops staged insertions (`removals` holds no
        // elements) and releases the borrow of `vec`.
    }

    /// Merges every staged edit into the borrowed vector and returns the
    /// removed elements.
    ///
    /// On success the vector holds the merged contents and the returned
    /// `SmallVec<T, N>` owns every removed original element, in ascending index
    /// order. Capacity may grow; when the resulting length is at most `N` the
    /// vector is guaranteed to be back in inline storage.
    ///
    /// This method panics on allocation failure; use
    /// [`try_commit`](Self::try_commit) to handle that case. After allocation
    /// succeeds the rest of the commit only performs moves and length updates,
    /// which cannot fail here.
    pub fn commit(self) -> SmallVec<T, N> {
        self.try_commit().unwrap_or_else(SmallVecError::handle)
    }

    /// Fallible form of [`commit`](Self::commit).
    ///
    /// On allocation failure the transaction remains usable and the original
    /// vector is unchanged; the caller may keep editing, commit later, or
    /// [`abort`](Self::abort).
    pub fn try_commit(mut self) -> Result<SmallVec<T, N>, SmallVecError> {
        let original_len = self.vec.len();
        let removed_len = self.staged_removal_len();
        let inserted_len = self.inserts.len();
        let final_len = original_len - removed_len + inserted_len;

        // Values inserted at one position must keep their insertion order.
        self.inserts.sort_by_key(|pending| pending.pos);

        // Every allocation happens first. `reserve_exact` keeps a vector
        // inline whenever the length fits in `N`; if either reservation fails,
        // returning the error drops `self` while the original vector is still
        // intact, so the side-stored insertions are the only things dropped.
        let mut result: SmallVec<T, N> = SmallVec::new();
        result.try_reserve_exact(final_len)?;

        let mut removed: SmallVec<T, N> = SmallVec::new();
        removed.try_reserve_exact(removed_len)?;

        // From here on no allocator calls and no user code run: only pointer
        // moves and length updates, so this region is infallible and the
        // ownership transfer is panic-safe.
        //
        // SAFETY:
        // * each original element is read from `src_ptr` exactly once, and the
        //   source is emptied at the end, so every moved value keeps a single
        //   owner;
        // * every destination lies in the spare capacity reserved above;
        // * ZST reads and writes are valid (non-null, aligned) no-ops.
        let src_ptr = self.vec.as_ptr();
        let result_base = result.as_mut_ptr();
        let removed_base = removed.as_mut_ptr();

        let mut removal_iter = self.removals.iter().copied().peekable();
        let mut insert_iter = self.inserts.iter().peekable();
        let mut out = 0usize;
        let mut out_removed = 0usize;
        let mut inserts_consumed = 0usize;

        for i in 0..original_len {
            // Insertions positioned before original index `i`.
            while let Some(pending) = insert_iter.peek() {
                if pending.pos > i {
                    break;
                }
                unsafe {
                    result_base
                        .add(out)
                        .copy_from_nonoverlapping(core::ptr::addr_of!(pending.value), 1);
                }
                out += 1;
                inserts_consumed += 1;
                insert_iter.next();
            }

            // Decide whether original element `i` is removed. A range boundary
            // exactly at `i` first advances past the finished range.
            let mut removing = false;
            if let Some(&(start, end)) = removal_iter.peek() {
                if i == end {
                    removal_iter.next();
                    if let Some(&(start, _)) = removal_iter.peek() {
                        removing = start == i;
                    }
                } else {
                    removing = i >= start;
                }
            }

            unsafe {
                if removing {
                    removed_base
                        .add(out_removed)
                        .copy_from_nonoverlapping(src_ptr.add(i), 1);
                    out_removed += 1;
                } else {
                    result_base
                        .add(out)
                        .copy_from_nonoverlapping(src_ptr.add(i), 1);
                    out += 1;
                }
            }
        }

        // Insertions positioned at `original_len` (the tail).
        for pending in insert_iter {
            unsafe {
                result_base
                    .add(out)
                    .copy_from_nonoverlapping(core::ptr::addr_of!(pending.value), 1);
            }
            out += 1;
            inserts_consumed += 1;
        }

        debug_assert_eq!(out, final_len);
        debug_assert_eq!(out_removed, removed_len);
        debug_assert_eq!(inserts_consumed, inserted_len);

        // The iterators are not used anymore, so their borrows end here (NLL),
        // before the mutable `set_len` calls below.

        // SAFETY: exactly `out` and `out_removed` slots were initialized in the
        // reserved spare capacity above.
        unsafe {
            result.set_len(out);
            removed.set_len(out_removed);
        }

        // The source elements now live in `result` or `removed`. An empty
        // SmallVec only owns (and on replacement, releases) its storage.
        // SAFETY: every one of the `original_len` elements was moved out once.
        unsafe {
            self.vec.set_len(0);
        }

        // The insertions side vector must not drop the values moved out of it.
        // All staged positions are valid, so every insertion was consumed.
        debug_assert_eq!(inserts_consumed, inserted_len);
        // SAFETY: all values were moved out exactly once.
        unsafe {
            self.inserts.set_len(0);
        }

        *self.vec = result;
        Ok(removed)
    }
}

impl<T, const N: usize> SmallVec<T, N> {
    /// Opens a [`Transaction`] that exclusively borrows this vector for the
    /// lifetime of the edit.
    ///
    /// The vector cannot be used through any other path while the returned
    /// transaction is alive; see the [module documentation](crate::transaction)
    /// for details.
    pub fn transaction(&mut self) -> Transaction<'_, T, N> {
        Transaction::new(self)
    }
}
