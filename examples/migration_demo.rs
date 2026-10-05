//! Demo: observe `SmallVec` storage migrations.
//!
//! This is a runnable companion to the `observe` cargo feature. It installs a
//! recording observer on a `SmallVec`, drives it through an inline -> heap
//! spill, a heap reallocation, and a heap -> inline return, and prints every
//! event.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example migration_demo --features observe
//! ```

#[cfg(feature = "observe")]
mod demo {
    use smallvec::{
        MigrationKind,
        MigrationObserver,
        SmallVec
    };
    use std::{
        cell::RefCell,
        rc::Rc
    };

    /// A simple observer that records every event into a shared log.
    #[derive(Clone)]
    struct Recorder {
        log: Rc<RefCell<Vec<String>>>
    }

    impl MigrationObserver for Recorder {
        fn on_migration(&mut self, event: &smallvec::MigrationEvent) {
            self.log.borrow_mut().push(format!(
                "{:>14} via {:<10} capacity {} -> {} (container {:?})",
                match event.kind {
                    MigrationKind::Spill => "spill",
                    MigrationKind::Realloc => "realloc",
                    MigrationKind::ReturnToInline => "return-inline",
                },
                format!("{:?}", event.cause).to_lowercase(),
                event.old_capacity,
                event.new_capacity,
                event.container
            ));
        }

        fn clone_observer(&self) -> Option<Box<dyn MigrationObserver>> {
            Some(Box::new(self.clone()))
        }
    }

    pub fn run() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let recorder = Recorder { log: log.clone() };

        // Inline capacity is 2.
        let mut v: SmallVec<i32, 2> = SmallVec::new();
        v.set_observer(recorder);

        println!("== push 3 items: the 3rd overflows to the heap ==");
        v.push(1);
        v.push(2);
        v.push(3);

        println!("== keep pushing: the heap allocation grows ==");
        v.push(4);
        v.push(5);
        v.push(6);

        println!("== truncate and shrink: contents move back inline ==");
        v.truncate(1);
        v.shrink_to_fit();

        println!("\nRecorded migrations:");
        for line in log.borrow().iter() {
            println!("  {line}");
        }

        println!("\nFinal contents: {:?}", v.as_slice());
    }
}

fn main() {
    #[cfg(feature = "observe")]
    demo::run();

    #[cfg(not(feature = "observe"))]
    eprintln!(
        "This example requires the `observe` feature.\n\
         Run: cargo run --example migration_demo --features observe"
    );
}
