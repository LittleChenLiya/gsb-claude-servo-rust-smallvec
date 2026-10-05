//! Storage migration observer tests.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features observe --test observe
//! ```

#![cfg(feature = "observe")]

use {
    smallvec::{
        MigrationCause,
        MigrationEvent,
        MigrationKind,
        MigrationObserver,
        SmallVec
    },
    std::{
        boxed::Box,
        cell::RefCell,
        panic,
        rc::Rc,
        string::String,
        vec,
        vec::Vec
    }
};

/// A recording observer. Clones propagate the observer (and thus the event
/// log) to the new container.
#[derive(Clone)]
struct Recorder {
    events: Rc<RefCell<Vec<MigrationEvent>>>
}

impl MigrationObserver for Recorder {
    fn on_migration(&mut self, event: &MigrationEvent) {
        self.events.borrow_mut().push(event.clone());
    }

    fn clone_observer(&self) -> Option<Box<dyn MigrationObserver>> {
        Some(Box::new(self.clone()))
    }
}

fn recorder() -> (Recorder, Rc<RefCell<Vec<MigrationEvent>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    (
        Recorder {
            events: events.clone()
        },
        events
    )
}

#[test]
fn spill_once_on_third_push() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(observer);

    v.push(1);
    v.push(2);
    assert!(events.borrow().is_empty(), "no migration while inline");

    v.push(3);
    let events = events.borrow();
    assert_eq!(events.len(), 1, "exactly one spill");
    let event = &events[0];
    assert_eq!(event.kind, MigrationKind::Spill);
    assert_eq!(event.cause, MigrationCause::Push);
    assert_eq!(event.old_capacity, 2);
    assert_eq!(event.new_capacity, 4);
    assert_eq!(event.container, v.container_id().unwrap());
}

#[test]
fn realloc_reports_capacities() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(observer);

    v.push(1);
    v.push(2);
    v.push(3); // spill to 4
    v.push(4);
    v.push(5); // realloc 4 -> 8

    let events = events.borrow();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, MigrationKind::Spill);
    assert_eq!(events[1].kind, MigrationKind::Realloc);
    assert_eq!(events[1].cause, MigrationCause::Push);
    assert_eq!(events[1].old_capacity, 4);
    assert_eq!(events[1].new_capacity, 8);
    assert_eq!(events[1].container, v.container_id().unwrap());
}

#[test]
fn shrink_returns_to_inline() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(observer);

    v.push(1);
    v.push(2);
    v.push(3); // spill to 4
    v.truncate(1);
    v.shrink_to_fit();

    let events = events.borrow();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, MigrationKind::Spill);
    assert_eq!(events[1].kind, MigrationKind::ReturnToInline);
    assert_eq!(events[1].cause, MigrationCause::Shrink);
    assert_eq!(events[1].old_capacity, 4);
    assert_eq!(events[1].new_capacity, 2);
    assert_eq!(events[1].container, v.container_id().unwrap());
    assert!(!v.spilled(), "back on the inline buffer");
}

#[test]
fn shrink_to_reports_inline_return() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 4>::new();
    v.set_observer(observer);

    v.extend(0..6); // spill to 8
    v.truncate(2);
    v.shrink_to(2); // return to inline

    let events = events.borrow();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].kind, MigrationKind::ReturnToInline);
    assert_eq!(events[1].old_capacity, 8);
    assert_eq!(events[1].new_capacity, 4);
    assert!(!v.spilled());
}

#[test]
fn from_vec_short_not_misjudged_as_inline() {
    // A pre-allocated `Vec` is already on the heap: the observer must not
    // fabricate an inline -> heap spill for it, and the container must not be
    // misjudged as inline.
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::from_vec(vec![1, 2]);
    assert!(v.spilled(), "from_vec storage is heap storage");
    assert!(v.capacity() > 2, "heap capacity exceeds the inline cap");

    v.set_observer(observer);
    assert!(events.borrow().is_empty(), "no fabricated spill event");

    // Growing it behaves like the heap container it is.
    v.push(3);
    v.push(4);
    v.push(5); // realloc 4 -> 8
    let events = events.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, MigrationKind::Realloc);
    assert_eq!(events[0].old_capacity, 4);
    assert_eq!(events[0].new_capacity, 8);
}

#[test]
fn clone_records_along_new_identity() {
    let (observer, events) = recorder();
    let mut a = SmallVec::<i32, 2>::new();
    a.set_observer(observer);

    a.push(1);
    a.push(2);
    a.push(3); // spill on `a`
    let b = a.clone(); // spill on `b`, identity must be `b`

    let events = events.borrow();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, MigrationKind::Spill);
    assert_eq!(events[0].cause, MigrationCause::Push);
    assert_eq!(events[0].container, a.container_id().unwrap());

    assert_eq!(events[1].kind, MigrationKind::Spill);
    assert_eq!(events[1].cause, MigrationCause::Clone);
    assert_eq!(events[1].container, b.container_id().unwrap());
    assert_ne!(events[0].container, events[1].container);
}

#[test]
fn splice_records_along_drained_identity() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(observer);

    v.push(1);
    v.push(2);
    v.push(3); // spill to 4
    v.splice(0..1, [4, 5, 6, 7]); // tail move reallocates `v`

    let events = events.borrow();
    let splice_events: Vec<_> = events
        .iter()
        .filter(|e| e.cause == MigrationCause::Splice)
        .collect();
    assert_eq!(splice_events.len(), 1);
    assert_eq!(splice_events[0].kind, MigrationKind::Realloc);
    assert_eq!(splice_events[0].old_capacity, 4);
    assert_eq!(splice_events[0].new_capacity, 8);
    assert_eq!(splice_events[0].container, v.container_id().unwrap());
}

#[test]
fn closure_observer_is_not_propagated_to_clone() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let events_for_closure = events.clone();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(move |event: &MigrationEvent| {
        events_for_closure.borrow_mut().push(event.clone());
    });

    v.push(1);
    v.push(2);
    v.push(3);
    assert_eq!(events.borrow().len(), 1);

    // Closures are not cloneable, so the clone gets no observer and its
    // migration is not recorded (and certainly not confused with `v`).
    let _b = v.clone();
    assert_eq!(events.borrow().len(), 1);
}

#[test]
fn observer_can_be_replaced_taken_and_cleared() {
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    assert!(!v.has_observer());
    v.set_observer(observer);
    assert!(v.has_observer());

    v.push(1);
    v.push(2);
    v.push(3);
    assert_eq!(events.borrow().len(), 1);

    let taken = v.take_observer();
    assert!(taken.is_some());
    assert!(!v.has_observer());

    v.push(4);
    v.push(5); // realloc, no observer -> no event
    assert_eq!(events.borrow().len(), 1);

    v.set_observer(recorder().0);
    v.clear_observer();
    assert!(!v.has_observer());
}

#[test]
fn reentrant_observer_call_is_rejected() {
    let mut v = SmallVec::<i32, 2>::new();
    let v_ptr = &mut v as *mut SmallVec<i32, 2>;
    v.set_observer(move |_event: &MigrationEvent| {
        // Deliberately re-enter the same container from within the callback.
        // This must panic rather than alias the container's internals.
        unsafe {
            (*v_ptr).grow(100);
        }
    });

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        // The third push overflows the inline buffer and triggers the spill
        // observer, which then re-enters the container.
        v.push(1);
        v.push(2);
        v.push(3);
    }));
    assert!(result.is_err(), "reentrant observer call must panic");

    let payload = result.unwrap_err();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
        .unwrap_or("");
    assert!(
        message.contains("reentrant"),
        "unexpected panic message: {message:?}"
    );
}

#[test]
fn observer_does_not_record_element_values() {
    // The event type has no field for elements; assert the shape explicitly
    // so this invariant cannot regress silently.
    let (observer, events) = recorder();
    let mut v = SmallVec::<i32, 2>::new();
    v.set_observer(observer);
    v.push(1);
    v.push(2);
    v.push(3);

    let event = &events.borrow()[0];
    let MigrationEvent {
        kind: _,
        cause: _,
        old_capacity: _,
        new_capacity: _,
        container: _
    } = event;
}
