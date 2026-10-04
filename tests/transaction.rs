//! Acceptance tests for the transactional editing window.
//!
//! Covers non-`Clone` elements, abort/commit ownership, drop accounting in
//! both inline and spilled storage, illegal ranges, unwinding exits and
//! inline-storage guarantees.

#![cfg(feature = "std")]

use {
    smallvec::SmallVec,
    std::{
        cell::Cell,
        panic::{AssertUnwindSafe, catch_unwind},
        rc::Rc,
    },
};

/// A non-`Clone`, non-`Copy` element that counts drops and records its id.
struct Item {
    id: char,
    drops: Rc<Cell<u32>>,
}

impl Item {
    fn new(id: char, drops: &Rc<Cell<u32>>) -> Self {
        Item {
            id,
            drops: Rc::clone(drops),
        }
    }
}

impl Drop for Item {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

fn ids<const N: usize>(v: &SmallVec<Item, N>) -> Vec<char> {
    v.iter().map(|i| i.id).collect()
}

fn make<const N: usize>(drops: &Rc<Cell<u32>>) -> SmallVec<Item, N> {
    let mut v = SmallVec::new();
    v.push(Item::new('a', drops));
    v.push(Item::new('b', drops));
    v.push(Item::new('c', drops));
    v
}

#[test]
fn abort_restores_abc_and_drops_only_x_inline_and_spilled() {
    // N = 4: three items stay inline.
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 4> = make(&drops);
    assert!(!v.spilled());
    {
        let mut tx = v.transaction();
        tx.stage_remove(1..2);
        tx.insert(1, Item::new('x', &drops));
        tx.abort();
    }
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert!(!v.spilled());
    assert_eq!(v.len(), 3);
    assert_eq!(drops.get(), 1, "abort must drop only x (inline)");

    // N = 2: three items are spilled on the heap.
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 2> = make(&drops);
    assert!(v.spilled());
    {
        let mut tx = v.transaction();
        tx.stage_remove(1..2);
        tx.insert(1, Item::new('x', &drops));
        tx.abort();
    }
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert!(v.spilled());
    assert_eq!(v.len(), 3);
    assert_eq!(drops.get(), 1, "abort must drop only x (spilled)");
}

#[test]
fn commit_yields_axc_and_owns_b_inline_and_spilled() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 4> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(1..2);
    tx.insert(1, Item::new('x', &drops));
    let removed: SmallVec<Item, 4> = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'x', 'c']);
    assert_eq!(ids(&removed), vec!['b']);
    assert!(!v.spilled());
    assert_eq!(drops.get(), 0);
    drop(removed);
    assert_eq!(
        drops.get(),
        1,
        "dropping the removal result drops only b (inline)"
    );

    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 2> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(1..2);
    tx.insert(1, Item::new('x', &drops));
    let removed: SmallVec<Item, 2> = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'x', 'c']);
    assert_eq!(ids(&removed), vec!['b']);
    assert!(v.spilled(), "final len 3 > N = 2, must stay spilled");
    assert_eq!(drops.get(), 0);
    drop(removed);
    assert_eq!(
        drops.get(),
        1,
        "dropping the removal result drops only b (spilled)"
    );
}

#[test]
fn drop_accounting_on_abort_inline_and_spilled() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 8> = make(&drops);
    {
        let mut tx = v.transaction();
        tx.stage_remove(1..2);
        tx.insert(0, Item::new('x', &drops));
        tx.insert(3, Item::new('y', &drops));
        tx.abort();
    }
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert_eq!(drops.get(), 2, "abort drops the two inserts only (inline)");

    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 1> = make(&drops);
    {
        let mut tx = v.transaction();
        tx.stage_remove(1..2);
        tx.insert(0, Item::new('x', &drops));
        tx.insert(3, Item::new('y', &drops));
        tx.abort();
    }
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert_eq!(drops.get(), 2, "abort drops the two inserts only (spilled)");
}

#[test]
fn drop_accounting_on_commit_inline_and_spilled() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 8> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(1..3); // b, c
    tx.insert(1, Item::new('x', &drops));
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'x']);
    assert_eq!(ids(&removed), vec!['b', 'c']);
    assert_eq!(drops.get(), 0);
    drop(removed);
    assert_eq!(drops.get(), 2);
    drop(v);
    assert_eq!(
        drops.get(),
        4,
        "every element dropped exactly once (inline)"
    );

    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 1> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(1..3);
    tx.insert(1, Item::new('x', &drops));
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'x']);
    assert_eq!(ids(&removed), vec!['b', 'c']);
    assert!(v.spilled());
    assert_eq!(drops.get(), 0);
    drop(removed);
    assert_eq!(drops.get(), 2);
    drop(v);
    assert_eq!(
        drops.get(),
        4,
        "every element dropped exactly once (spilled)"
    );
}

#[test]
fn commit_back_inline_when_final_length_fits() {
    // A spilled vector committed to a length <= N must return inline; no
    // element is dropped during that migration.
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 4> = SmallVec::new();
    for id in ['a', 'b', 'c', 'd', 'e'] {
        v.push(Item::new(id, &drops));
    }
    assert!(v.spilled());

    let mut tx = v.transaction();
    tx.stage_remove(3..5); // d, e
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert_eq!(ids(&removed), vec!['d', 'e']);
    assert!(
        !v.spilled(),
        "final length fits inline capacity; must be inline"
    );
    assert_eq!(drops.get(), 0);
}
#[test]
fn illegal_removal_range_is_refused_before_any_move() {
    let drops = Rc::new(Cell::new(0));

    // The vector lives outside the unwinding closure, so originals survive the
    // panic and their drops are not counted: rejection happens before a move.
    let mut v: SmallVec<Item, 4> = make(&drops);
    let out_of_bounds = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v.transaction();
        tx.stage_remove(1..10);
    }));
    assert!(out_of_bounds.is_err());
    assert_eq!(drops.get(), 0, "originals untouched after rejected range");
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);

    let mut v: SmallVec<Item, 4> = make(&drops);
    let start_after_end = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v.transaction();
        #[allow(clippy::reversed_empty_ranges)]
        tx.stage_remove(2..1);
    }));
    assert!(start_after_end.is_err());
    assert_eq!(drops.get(), 0);
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);

    // Overlapping ranges panic; adjacent ranges commit fine.
    let mut v: SmallVec<Item, 4> = make(&drops);
    let overlap = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v.transaction();
        tx.stage_remove(0..1);
        tx.stage_remove(0..2);
    }));
    assert!(overlap.is_err());
    assert_eq!(drops.get(), 0);
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);

    let mut v: SmallVec<Item, 4> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(0..1);
    tx.stage_remove(1..2); // adjacent: legal
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['c']);
    assert_eq!(ids(&removed), vec!['a', 'b']);
}

#[test]
fn illegal_insertion_position_is_refused() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 4> = make(&drops);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v.transaction();
        // The value is constructed, then the position check rejects it; its
        // drop is the single insert-side drop, originals are untouched.
        tx.insert(4, Item::new('x', &drops));
    }));
    assert!(result.is_err());
    assert_eq!(drops.get(), 1, "the rejected x alone is dropped");
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
}

#[test]
fn unwind_during_editing_restores_original_order() {
    // Spilled source: only the staged insert is dropped; the vector keeps
    // owning a, b, c in order.
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 2> = make(&drops);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v.transaction();
        tx.stage_remove(1..2);
        tx.insert(0, Item::new('x', &drops));
        panic!("simulated failure");
    }));
    assert!(result.is_err());
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert!(v.spilled());
    assert_eq!(
        drops.get(),
        1,
        "only the insert is dropped on unwind (spilled)"
    );

    // Inline source.
    let drops2 = Rc::new(Cell::new(0));
    let mut v2: SmallVec<Item, 4> = make(&drops2);
    let result2 = catch_unwind(AssertUnwindSafe(|| {
        let mut tx = v2.transaction();
        tx.stage_remove(0..1);
        tx.insert(2, Item::new('y', &drops2));
        panic!("simulated failure 2");
    }));
    assert!(result2.is_err());
    assert_eq!(ids(&v2), vec!['a', 'b', 'c']);
    assert!(!v2.spilled());
    assert_eq!(drops2.get(), 1);
}

#[test]
fn capacity_may_grow_and_length_contents_are_exact() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 2> = SmallVec::new();
    v.push(Item::new('a', &drops));
    assert!(!v.spilled());

    let mut tx = v.transaction();
    // All positions are original coordinates: insert after the single
    // original element; same-position inserts keep insertion order.
    tx.insert(1, Item::new('x', &drops));
    tx.insert(1, Item::new('y', &drops));
    tx.insert(1, Item::new('z', &drops));
    let removed = tx.commit();

    assert_eq!(ids(&v), vec!['a', 'x', 'y', 'z']);
    assert_eq!(v.len(), 4);
    assert!(v.spilled());
    assert!(v.capacity() >= 4);
    assert!(removed.is_empty());
    assert_eq!(drops.get(), 0);
}

#[test]
fn multiple_inserts_at_same_position_keep_order() {
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 8> = make(&drops);
    let mut tx = v.transaction();
    tx.insert(1, Item::new('x', &drops));
    tx.insert(1, Item::new('y', &drops));
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'x', 'y', 'b', 'c']);
    assert!(removed.is_empty());
}

#[test]
fn empty_and_full_range_ops() {
    let drops = Rc::new(Cell::new(0));

    // Empty staged range is a no-op.
    let mut v: SmallVec<Item, 4> = make(&drops);
    let mut tx = v.transaction();
    #[allow(clippy::reversed_empty_ranges)]
    tx.stage_remove(1..1);
    let removed = tx.commit();
    assert_eq!(ids(&v), vec!['a', 'b', 'c']);
    assert!(removed.is_empty());

    // Full-range removal commits to an empty inline vector.
    let mut v: SmallVec<Item, 4> = make(&drops);
    let mut tx = v.transaction();
    tx.stage_remove(..);
    let removed = tx.commit();
    assert!(v.is_empty());
    assert!(!v.spilled());
    assert_eq!(ids(&removed), vec!['a', 'b', 'c']);
}

#[test]
fn zero_inline_capacity_and_exclusive_borrow_docs() {
    // N = 0: even a single element is spilled; commit still works.
    let drops = Rc::new(Cell::new(0));
    let mut v: SmallVec<Item, 0> = SmallVec::new();
    v.push(Item::new('a', &drops));
    v.push(Item::new('b', &drops));
    assert!(v.spilled());

    let mut tx = v.transaction();
    assert_eq!(tx.original_len(), 2);
    assert_eq!(tx.staged_removal_len(), 0);
    assert_eq!(tx.staged_insert_len(), 0);
    tx.stage_remove(0..1);
    tx.insert(1, Item::new('x', &drops));
    assert_eq!(tx.staged_removal_len(), 1);
    assert_eq!(tx.staged_insert_len(), 1);
    // While `tx` is alive, `v` cannot be borrowed elsewhere (enforced by the
    // borrow checker; no runtime call needed).
    let removed = tx.commit();
    // x is inserted before original index 1 (b), so it precedes b.
    assert_eq!(ids(&v), vec!['x', 'b']);
    assert_eq!(ids(&removed), vec!['a']);
    assert!(v.spilled());
    assert_eq!(drops.get(), 0);

    // Removing everything from a spilled vector commits to an empty vector.
    let mut v: SmallVec<Item, 0> = SmallVec::new();
    v.push(Item::new('a', &drops));
    let mut tx = v.transaction();
    tx.stage_remove(..);
    let removed = tx.commit();
    assert!(v.is_empty());
    assert_eq!(ids(&removed), vec!['a']);
}
