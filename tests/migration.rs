//! Acceptance tests for the explicit storage-migration observer
//! (`migration-observer` feature).
//!
//! Run with:
//!
//! ```text
//! cargo test --test migration --features migration-observer
//! ```

use {
    smallvec::observer::{
        ContainerId,
        MigrationEvent,
        MigrationKind,
        ObservedSmallVec,
        Trigger
    },
    std::{
        cell::RefCell,
        panic::{
            AssertUnwindSafe,
            catch_unwind
        },
        rc::Rc
    }
};

type Log = Rc<RefCell<Vec<MigrationEvent>>>;

/// Returns a shared event log plus an observer closure feeding it.
fn recorder() -> (Log, impl FnMut(MigrationEvent)) {
    let log = Log::default();
    let sink = {
        let log = Rc::clone(&log);
        move |event: MigrationEvent| log.borrow_mut().push(event)
    };
    (log, sink)
}

fn kinds(log: &Log) -> Vec<(MigrationKind, Trigger)> {
    log.borrow()
        .iter()
        .map(|event| (event.kind, event.trigger))
        .collect()
}

#[test]
fn third_push_into_inline_capacity_two_spills_exactly_once() {
    let (log, sink) = recorder();
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    v.set_observer(sink);

    v.push(1);
    v.push(2);
    assert!(log.borrow().is_empty(), "no migration while inline");
    assert!(!v.spilled());

    v.push(3);
    {
        let events = log.borrow();
        assert_eq!(events.len(), 1, "exactly one spill, got {events:?}");
        let event = events[0];
        assert_eq!(event.container, v.id());
        assert_eq!(event.kind, MigrationKind::InlineToHeap);
        assert_eq!(event.trigger, Trigger::Push);
        assert_eq!(event.capacity_before, 2);
        assert_eq!(event.capacity_after, 4);
        assert_eq!(event.len, 3);
    }
    assert!(v.spilled());

    // Room to spare now: no further migration.
    v.push(4);
    assert_eq!(log.borrow().len(), 1);
}

#[test]
fn heap_reallocation_is_recorded_with_capacities() {
    let (log, sink) = recorder();
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    v.set_observer(sink);
    v.extend([1, 2, 3, 4]);
    assert_eq!(
        kinds(&log),
        [(MigrationKind::InlineToHeap, Trigger::Extend)]
    );
    assert_eq!(v.capacity(), 4);

    // Exceeding the heap capacity reallocates in place on the heap.
    v.push(5);
    assert_eq!(
        kinds(&log),
        [
            (MigrationKind::InlineToHeap, Trigger::Extend),
            (MigrationKind::HeapRealloc, Trigger::Push)
        ]
    );
    let event = log.borrow()[1];
    assert_eq!(event.capacity_before, 4);
    assert_eq!(event.capacity_after, 8);
    assert!(v.spilled());

    // Growing an already-spilled vector is a heap reallocation, not a spill.
    v.reserve_exact(16);
    let event = log.borrow()[2];
    assert_eq!(event.kind, MigrationKind::HeapRealloc);
    assert_eq!(event.trigger, Trigger::ReserveExact);
    assert_eq!(event.capacity_before, 8);
    assert_eq!(event.capacity_after, 16 + 5);

    // Shrinking while staying on the heap is a heap reallocation too.
    v.shrink_to_fit();
    let event = log.borrow()[3];
    assert_eq!(event.kind, MigrationKind::HeapRealloc);
    assert_eq!(event.trigger, Trigger::ShrinkToFit);
    assert_eq!(event.capacity_after, 5);
    assert!(v.spilled());
    assert_eq!(log.borrow().len(), 4);
}

#[test]
fn shrink_back_to_inline_is_recorded_accurately() {
    let (log, sink) = recorder();
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    v.set_observer(sink);
    v.extend([10, 20, 30]);
    assert!(v.spilled());
    assert_eq!(
        kinds(&log),
        [(MigrationKind::InlineToHeap, Trigger::Extend)]
    );

    // Still too long for the inline buffer: shrink_to_fit only tightens the
    // heap allocation (4 -> 3), a heap reallocation.
    v.shrink_to_fit();
    {
        let events = log.borrow();
        assert_eq!(events.len(), 2);
        let event = events[1];
        assert_eq!(event.kind, MigrationKind::HeapRealloc);
        assert_eq!(event.trigger, Trigger::ShrinkToFit);
        assert_eq!(event.capacity_before, 4);
        assert_eq!(event.capacity_after, 3);
    }
    assert!(v.spilled());

    // Now it fits: shrink_to_fit moves it back inline.
    v.pop();
    v.shrink_to_fit();
    {
        let events = log.borrow();
        assert_eq!(events.len(), 3);
        let event = events[2];
        assert_eq!(event.container, v.id());
        assert_eq!(event.kind, MigrationKind::HeapToInline);
        assert_eq!(event.trigger, Trigger::ShrinkToFit);
        assert_eq!(event.capacity_before, 3);
        assert_eq!(event.capacity_after, 2);
        assert_eq!(event.len, 2);
    }
    assert!(!v.spilled());
    assert_eq!(&*v, &[10, 20]);

    // shrink_to can also drive the transition.
    v.extend([1, 2, 3]);
    assert!(v.spilled());
    v.truncate(1);
    v.shrink_to(0);
    let event = log.borrow()[4];
    assert_eq!(event.kind, MigrationKind::HeapToInline);
    assert_eq!(event.trigger, Trigger::ShrinkTo);
    assert_eq!(event.capacity_after, 2);
    assert!(!v.spilled());
}

#[test]
fn preallocated_short_vector_is_not_misjudged_as_inline() {
    // A vector pre-allocated beyond its inline capacity is born on the heap
    // even though it is short (empty, in fact). A length-based heuristic
    // would call it inline; the observer must not.
    let (log, sink) = recorder();
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::with_capacity_observed(4, sink);
    assert!(v.spilled());
    {
        let events = log.borrow();
        assert_eq!(events.len(), 1);
        let event = events[0];
        assert_eq!(event.container, v.id());
        assert_eq!(event.kind, MigrationKind::InlineToHeap);
        assert_eq!(event.trigger, Trigger::WithCapacity);
        assert_eq!(event.capacity_before, 2);
        assert_eq!(event.capacity_after, 4);
        assert_eq!(event.len, 0);
    }

    // Filling the pre-allocated capacity causes no spurious "spill".
    v.push(1);
    v.push(2);
    v.push(3);
    assert_eq!(log.borrow().len(), 1, "no false spill for heap-born vector");

    // A pre-allocation that fits inline is genuinely inline: no event.
    let (log2, sink2) = recorder();
    let mut w: ObservedSmallVec<u32, 2> = ObservedSmallVec::with_capacity_observed(2, sink2);
    assert!(!w.spilled());
    assert!(log2.borrow().is_empty());
    w.push(1);
    w.push(2);
    assert!(log2.borrow().is_empty());

    // from_vec keeps a heap allocation even for short contents.
    let (log3, sink3) = recorder();
    let v3: ObservedSmallVec<u32, 2> = ObservedSmallVec::from_vec_observed(vec![7, 8], sink3);
    assert!(v3.spilled());
    let events = log3.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, MigrationKind::InlineToHeap);
    assert_eq!(events[0].trigger, Trigger::FromVec);
}

#[test]
fn clone_records_under_its_own_identity() {
    let (source_log, source_sink) = recorder();
    let mut source: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    source.set_observer(source_sink);
    source.extend([1, 2, 3, 4]);
    assert_eq!(source_log.borrow().len(), 1);
    let source_id = source.id();

    // The clone's birth spill is reported through the source's observer but
    // stamped with the clone's fresh identity.
    let mut clone = source.clone_observed();
    let clone_id = clone.id();
    assert_ne!(source_id, clone_id);
    {
        let events = source_log.borrow();
        assert_eq!(events.len(), 2);
        let event = events[1];
        assert_eq!(event.container, clone_id);
        assert_eq!(event.kind, MigrationKind::InlineToHeap);
        assert_eq!(event.trigger, Trigger::Clone);
        assert_eq!(event.capacity_before, 2);
        assert_eq!(event.capacity_after, 4);
        assert_eq!(event.len, 4);
    }

    // From now on each container reports to its own log only.
    let (clone_log, clone_sink) = recorder();
    clone.set_observer(clone_sink);
    clone.extend([4, 5]); // reallocate the clone: 4 -> 8
    source.push(9); // reallocate the source: 4 -> 8

    let clone_events = clone_log.borrow();
    assert_eq!(clone_events.len(), 1);
    assert_eq!(clone_events[0].container, clone_id);
    assert_eq!(clone_events[0].kind, MigrationKind::HeapRealloc);
    assert_eq!(clone_events[0].trigger, Trigger::Extend);

    let source_events = source_log.borrow();
    assert_eq!(source_events.len(), 3);
    assert_eq!(source_events[2].container, source_id);
    assert_eq!(source_events[2].kind, MigrationKind::HeapRealloc);
    assert_eq!(source_events[2].trigger, Trigger::Push);
    assert!(
        source_events
            .iter()
            .all(|e| e.container == source_id || e.container == clone_id)
    );
}

#[test]
fn drain_records_against_the_drained_container_only() {
    let (a_log, a_sink) = recorder();
    let mut a: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    a.set_observer(a_sink);
    a.extend([1, 2, 3, 4, 5]);
    let a_id = a.id();
    let events_before = a_log.borrow().len();

    // Draining does not migrate storage, so it must not invent events.
    let drained: Vec<u32> = a.drain(1..4).collect();
    assert_eq!(drained, [2, 3, 4]);
    assert_eq!(&*a, &[1, 5]);
    assert!(a.spilled(), "drain must not silently move storage");
    assert_eq!(a_log.borrow().len(), events_before, "no false events");
    assert!(a_log.borrow().iter().all(|event| event.container == a_id));

    // Feeding another observed container from the drain attributes the
    // spill to the *target* container.
    let (b_log, b_sink) = recorder();
    let mut b: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    b.set_observer(b_sink);
    let b_id = b.id();
    b.extend(a.drain(..));
    assert_eq!(&*b, &[1, 5]);
    assert!(a.is_empty());
    assert_eq!(
        a_log.borrow().len(),
        events_before,
        "drain source unchanged"
    );
    assert_eq!(b_log.borrow().len(), 0, "two items fit b's inline capacity");
    b.push(6);
    let b_events = b_log.borrow();
    assert_eq!(b_events.len(), 1);
    assert_eq!(b_events[0].container, b_id);
    assert_eq!(b_events[0].kind, MigrationKind::InlineToHeap);
    assert_eq!(b_events[0].trigger, Trigger::Push);
}

#[test]
fn reentrant_callback_is_explicitly_rejected() {
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    // Hand the callback a way back into the container (as a raw address so
    // the closure can be 'static). The wrapper must refuse the re-entry.
    let address = &mut v as *mut ObservedSmallVec<u32, 2> as usize;
    v.set_observer(move |_event| {
        let v = unsafe { &mut *(address as *mut ObservedSmallVec<u32, 2>) };
        v.push(99);
    });
    v.push(1);
    v.push(2);

    // The third push spills, the callback fires and tries to re-enter.
    let result = catch_unwind(AssertUnwindSafe(|| v.push(3)));
    let panic = result.expect_err("re-entrant callback must be rejected");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("re-entrant"),
        "unexpected panic message: {message}"
    );

    // The flag is released even though the callback panicked: the container
    // remains fully usable and the completed spill is still observed.
    assert!(v.spilled());
    assert_eq!(v.len(), 3);
    v.clear_observer();
    v.push(4);
    assert_eq!(&*v, &[1, 2, 3, 4]);
}

#[test]
fn disabled_observation_keeps_behavior_and_records_nothing() {
    // No observer installed: operations behave exactly like SmallVec's.
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    assert!(!v.observation_enabled());
    v.extend([1, 2, 3]);
    assert!(v.spilled());
    v.shrink_to_fit();
    v.truncate(2);
    v.shrink_to_fit();
    assert!(!v.spilled());
    assert_eq!(&*v, &[1, 2]);

    // Read-only access goes through Deref to the inner SmallVec.
    assert_eq!(v.len(), 2);
    assert_eq!(v.capacity(), 2);

    // The plain SmallVec API is untouched by the feature.
    let mut plain = smallvec::SmallVec::<u32, 2>::new();
    plain.extend([1, 2, 3]);
    assert!(plain.spilled());
}

#[test]
fn events_carry_no_element_values_and_ids_are_unique() {
    // Compile-time proof that an event is plain data with no element type.
    fn assert_copy<T: Copy>() {}
    assert_copy::<MigrationEvent>();
    assert_copy::<ContainerId>();

    let a: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    let b: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    assert_ne!(a.id(), b.id());
}
