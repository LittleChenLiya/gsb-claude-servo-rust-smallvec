//! Storage-migration demo for `ObservedSmallVec`.
//!
//! Run with:
//!
//! ```text
//! cargo run --example migration_demo --features migration-observer
//! ```

use {
    smallvec::observer::{
        MigrationEvent,
        ObservedSmallVec
    },
    std::panic::{
        AssertUnwindSafe,
        catch_unwind
    }
};

/// Returns an observer closure that prints each event with a friendly
/// container name plus the instance's own identity.
fn reporter(name: &'static str) -> impl FnMut(MigrationEvent) {
    move |event: MigrationEvent| {
        println!(
            "  [{name} #{id}] {kind:?} via {trigger:?}: capacity {before} -> {after} (len {len})",
            id = event.container.0,
            kind = event.kind,
            trigger = event.trigger,
            before = event.capacity_before,
            after = event.capacity_after,
            len = event.len
        );
    }
}

fn main() {
    println!("== inline capacity 2: the third push spills exactly once ==");
    let mut v: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    v.set_observer(reporter("v"));
    v.push(1);
    v.push(2);
    v.push(3); // inline -> heap
    v.push(4);
    v.push(5); // heap -> heap realloc

    println!("== pop back down and shrink: return to inline storage ==");
    v.pop();
    v.pop();
    v.pop();
    v.shrink_to_fit(); // heap -> inline
    println!("  v is spilled: {}, contents: {:?}", v.spilled(), &*v);

    println!("== a pre-allocated short vector is not misjudged as inline ==");
    let mut pre: ObservedSmallVec<u32, 2> =
        ObservedSmallVec::with_capacity_observed(4, reporter("pre"));
    println!("  pre is spilled at birth: {}", pre.spilled());
    pre.push(1);
    pre.push(2);
    println!("  (filling the reserved capacity produced no false spill)");

    println!("== clone gets its own identity ==");
    let mut source: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    source.set_observer(reporter("source"));
    source.extend([7, 8, 9]);
    let mut clone = source.clone_observed(); // birth spill stamped with clone id
    clone.set_observer(reporter("clone"));
    clone.push(10); // reported by the clone, under the clone's id
    println!("  source id: {}, clone id: {}", source.id().0, clone.id().0);

    println!("== drain keeps the drained container's identity ==");
    let drained: Vec<u32> = source.drain(1..).collect();
    println!(
        "  drained {drained:?}; source now {:?}, still spilled: {} (no migration events above)",
        &*source,
        source.spilled()
    );

    println!("== re-entrant callbacks are explicitly rejected ==");
    let mut guarded: ObservedSmallVec<u32, 2> = ObservedSmallVec::new();
    let address = &mut guarded as *mut ObservedSmallVec<u32, 2> as usize;
    guarded.set_observer(move |_event| {
        // Try to re-enter the container from inside its own callback.
        let guarded = unsafe { &mut *(address as *mut ObservedSmallVec<u32, 2>) };
        guarded.push(99);
    });
    guarded.extend([1, 2]);
    let result = catch_unwind(AssertUnwindSafe(|| guarded.push(3)));
    match result {
        Err(panic) => println!(
            "  re-entry rejected with: {}",
            panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("<unknown>")
        ),
        Ok(()) => println!("  ERROR: re-entry was not rejected!")
    }
    guarded.clear_observer();
    guarded.push(4);
    println!("  container still usable afterwards: {:?}", &*guarded);
}
