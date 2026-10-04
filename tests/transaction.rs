//! Tests for the transactional edit window.
//!
//! These tests verify the acceptance criteria:
//! - Non-`Clone` objects with IDs a, b, c: remove b, insert x, then abandon
//!   restores a, b, c and drops only x.
//! - Commit yields a, x, c and a removal result owning b.
//! - Drop counts are verified in both inline and overflow states.
//! - Invalid ranges are rejected before moving elements.
//! - Panic exits roll back the transaction.
//!
//! # Running locally
//!
//! ```sh
//! # Run only the transaction tests (small, deterministic):
//! cargo test --test transaction
//!
//! # With the `std` feature explicitly enabled:
//! cargo test --features std --test transaction
//!
//! # Run the doctests for the transaction module:
//! cargo test --doc transaction
//!
//! # Run the core test suite to confirm no regressions:
//! cargo test --test main
//! ```

use {
    core::sync::atomic::{
        AtomicUsize,
        Ordering
    },
    smallvec::{
        transaction::Transaction,
        SmallVec
    },
    std::panic
};

/// A non-`Clone` object with an ID and a drop counter.
///
/// Each instance increments the shared counter when dropped, allowing tests to
/// verify exactly which elements are dropped during commit or rollback.
struct Counted<'a> {
    id: u32,
    counter: &'a AtomicUsize
}

impl<'a> Counted<'a> {
    fn new(id: u32, counter: &'a AtomicUsize) -> Self {
        Counted {
            id,
            counter
        }
    }
}

impl<'a> Drop for Counted<'a> {
    fn drop(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

// Note: `Counted` intentionally does NOT derive `Clone`.

/// Helper to build the acceptance fixture: non-`Clone` objects a, b, c and a
/// pending insertion x, all sharing the same drop counter.
struct Fixture<'a> {
    a: Counted<'a>,
    b: Counted<'a>,
    c: Counted<'a>,
    x: Counted<'a>
}

impl<'a> Fixture<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        Fixture {
            a: Counted::new(1, counter),
            b: Counted::new(2, counter),
            c: Counted::new(3, counter),
            x: Counted::new(9, counter)
        }
    }
}

/// Creates a spilled (overflow) `SmallVec` with N = 2 and three elements.
///
/// `from_buf` has a const assertion `S <= N`, so it cannot construct a
/// spilled container directly. This helper builds one by pushing elements,
/// which triggers the heap growth.
fn spilled_vec<'a>(
    a: Counted<'a>,
    b: Counted<'a>,
    c: Counted<'a>
) -> SmallVec<Counted<'a>, 2> {
    let mut vec = SmallVec::new();
    vec.push(a);
    vec.push(b);
    vec.push(c);
    vec
}

// ---------------------------------------------------------------------------
// Acceptance criteria: rollback restores a, b, c and drops only x.
// ---------------------------------------------------------------------------

#[test]
fn acceptance_rollback_inline() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);
    assert!(!vec.spilled());

    {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        tx.insert(1, f.x);
        // Abandon: drop the transaction.
    }

    // Original order restored.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // Inline state preserved.
    assert!(!vec.spilled());
    // Only x was dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn acceptance_rollback_overflow() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    // N = 2, three elements => spilled (overflow).
    let mut vec = spilled_vec(f.a, f.b, f.c);
    assert!(vec.spilled());

    {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        tx.insert(1, f.x);
        // Abandon: drop the transaction.
    }

    // Original order restored.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // Overflow state preserved (len > N).
    assert!(vec.spilled());
    // Only x was dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Acceptance criteria: commit yields a, x, c and a removal result owning b.
// ---------------------------------------------------------------------------

#[test]
fn acceptance_commit_inline() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);
    assert!(!vec.spilled());

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        tx.insert(1, f.x);
        tx.commit()
    };

    // Final sequence: a, x, c.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 9);
    assert_eq!(vec[2].id, 3);
    // Inline state preserved.
    assert!(!vec.spilled());
    // Removal result owns b.
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].id, 2);
    // Nothing dropped during commit.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn acceptance_commit_overflow() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    // N = 2, three elements => spilled (overflow).
    let mut vec = spilled_vec(f.a, f.b, f.c);
    assert!(vec.spilled());

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        tx.insert(1, f.x);
        tx.commit()
    };

    // Final sequence: a, x, c.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 9);
    assert_eq!(vec[2].id, 3);
    // Overflow state preserved (len > N).
    assert!(vec.spilled());
    // Removal result owns b.
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].id, 2);
    // Nothing dropped during commit.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Inline state guarantee: spilled container shrinks to inline when final len
// is at most N.
// ---------------------------------------------------------------------------

#[test]
fn commit_shrinks_to_inline_when_fits() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    // N = 2, three elements => spilled.
    let mut vec = spilled_vec(f.a, f.b, f.c);
    assert!(vec.spilled());

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        // Remove b and c, leaving a (len 1 <= N).
        tx.remove(1..3);
        tx.commit()
    };

    // Final sequence: a.
    assert_eq!(vec.len(), 1);
    assert_eq!(vec[0].id, 1);
    // Shrunk to inline.
    assert!(!vec.spilled());
    // Removal result owns b and c.
    assert_eq!(removed.len(), 2);
    assert_eq!(removed[0].id, 2);
    assert_eq!(removed[1].id, 3);
    // Nothing dropped during commit.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Invalid ranges are rejected before moving elements.
// ---------------------------------------------------------------------------

#[test]
fn invalid_range_rejected() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut tx = Transaction::begin(&mut vec);
        // Out-of-bounds range: working length is 3.
        tx.remove(5..6);
    }));

    assert!(result.is_err());
    // No elements moved: original order and length preserved.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // Nothing dropped (no insertions were made).
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_insert_index_rejected() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut tx = Transaction::begin(&mut vec);
        // Out-of-bounds index: working length is 3.
        tx.insert(4, f.x);
    }));

    assert!(result.is_err());
    // Original order preserved.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // x was moved into the transaction but the transaction was dropped,
    // so x is dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Panic exits roll back the transaction.
// ---------------------------------------------------------------------------

#[test]
fn panic_during_transaction_rolls_back() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        tx.insert(1, f.x);
        panic!("intentional panic to test rollback");
    }));

    assert!(result.is_err());
    // Original order restored.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // Only x was dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn panic_during_operation_rolls_back() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(1..2);
        // This panics because the range is invalid.
        tx.remove(10..11);
    }));

    assert!(result.is_err());
    // Container untouched.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    // Nothing dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Borrow semantics: the container cannot be borrowed while the transaction
// is alive. This is enforced by the type system; the following test documents
// the intended usage.
// ---------------------------------------------------------------------------

#[test]
fn borrow_semantics_documented() {
    let mut vec = SmallVec::<u32, 4>::from_buf([1, 2, 3]);
    let mut tx = Transaction::begin(&mut vec);
    tx.remove(1..2);
    tx.insert(1, 9);
    // The following would fail to compile because `tx` holds `&mut vec`:
    //     let _ = &vec;
    //     let _ = vec.len();
    let removed = tx.commit();
    // After commit, `vec` is accessible again.
    assert_eq!(&*vec, &[1, 9, 3]);
    assert_eq!(&*removed, &[2]);
}

// ---------------------------------------------------------------------------
// Commit lifetime: removed elements are owned by the returned SmallVec.
// ---------------------------------------------------------------------------

#[test]
fn commit_lifetime_returns_owned_removed() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(0..1); // remove a
        tx.remove(1..2); // remove c (original index 2)
        tx.commit()
    };

    // Final sequence: b.
    assert_eq!(vec.len(), 1);
    assert_eq!(vec[0].id, 2);
    // Removed owns a and c.
    assert_eq!(removed.len(), 2);
    assert_eq!(removed[0].id, 1);
    assert_eq!(removed[1].id, 3);
    // Nothing dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Edge cases.
// ---------------------------------------------------------------------------

#[test]
fn empty_transaction() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let removed = {
        let tx = Transaction::begin(&mut vec);
        tx.commit()
    };

    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    assert!(removed.is_empty());
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn remove_all() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(0..3);
        tx.commit()
    };

    assert_eq!(vec.len(), 0);
    assert!(!vec.spilled());
    assert_eq!(removed.len(), 3);
    assert_eq!(removed[0].id, 1);
    assert_eq!(removed[1].id, 2);
    assert_eq!(removed[2].id, 3);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn insert_at_front() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.b, f.c]);

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.insert(0, f.a);
        tx.commit()
    };

    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    assert!(removed.is_empty());
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn multiple_removes() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.remove(0..1); // remove a
        tx.remove(1..2); // remove c (original index 2)
        tx.commit()
    };

    assert_eq!(vec.len(), 1);
    assert_eq!(vec[0].id, 2); // b
    assert_eq!(removed.len(), 2);
    assert_eq!(removed[0].id, 1); // a
    assert_eq!(removed[1].id, 3); // c
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[test]
fn removed_insertion_is_dropped() {
    let counter = AtomicUsize::new(0);
    let f = Fixture::new(&counter);

    let mut vec = SmallVec::<_, 4>::from_buf([f.a, f.b, f.c]);

    let removed = {
        let mut tx = Transaction::begin(&mut vec);
        tx.insert(1, f.x); // insert x at position 1
        tx.remove(1..2); // remove x (now at position 1)
        tx.commit()
    };

    // x was inserted then removed, so it is dropped.
    assert_eq!(vec.len(), 3);
    assert_eq!(vec[0].id, 1);
    assert_eq!(vec[1].id, 2);
    assert_eq!(vec[2].id, 3);
    assert!(removed.is_empty());
    // x dropped.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
