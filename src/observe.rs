//! Storage migration observation for [`SmallVec`].
//!
//! This module is only available when the `observe` cargo feature is enabled.
//! It provides an opt-in, zero-element-value record of the actual storage
//! migrations a [`SmallVec`] undergoes:
//!
//! * [`MigrationKind::Spill`] — inline storage overflowed to the heap.
//! * [`MigrationKind::Realloc`] — existing heap storage was reallocated
//!   (grown or shrunk).
//! * [`MigrationKind::ReturnToInline`] — heap storage was abandoned in favor
//!   of the inline buffer.
//!
//! Observers are attached to a *single* container with
//! [`SmallVec::set_observer`]. Every event carries the identity of the
//! container that migrated, so events from different instances (including
//! clones and drained containers) can never be confused.
//!
//! [`SmallVec`]: crate::SmallVec
//! [`SmallVec::set_observer`]: crate::SmallVec::set_observer

use crate::Box;

/// A stable identity for a [`SmallVec`] container.
///
/// Unlike the container's address, this identity survives moves (e.g. the
/// return value of [`clone`](Clone::clone)). It is assigned when the first
/// observer is installed and never changes for the rest of the container's
/// life.
///
/// [`SmallVec`]: crate::SmallVec
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContainerId(pub u64);

impl ContainerId {
    /// Returns the raw numeric identity.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// The kind of storage migration a [`SmallVec`] underwent.
///
/// [`SmallVec`]: crate::SmallVec
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationKind {
    /// The inline buffer overflowed and the contents were moved to a fresh
    /// heap allocation (inline → heap).
    Spill,
    /// An existing heap allocation was reallocated (heap → heap), either
    /// grown or shrunk.
    Realloc,
    /// The heap allocation was abandoned and the contents were moved back
    /// into the inline buffer (heap → inline).
    ReturnToInline
}

/// The operation that triggered a storage migration.
///
/// This identifies the container-level operation in progress when the
/// migration happened, rather than guessing from the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationCause {
    /// A [`push`](crate::SmallVec::push) /
    /// [`push_mut`](crate::SmallVec::push_mut) call.
    Push,
    /// An [`insert`](crate::SmallVec::insert) /
    /// [`insert_mut`](crate::SmallVec::insert_mut) call.
    Insert,
    /// An [`append`](crate::SmallVec::append) call.
    Append,
    /// An [`extend`](crate::SmallVec::extend) call (including
    /// [`extend_from_slice`](crate::SmallVec::extend_from_slice) and
    /// [`resize`](crate::SmallVec::resize)).
    Extend,
    /// A [`reserve`](crate::SmallVec::reserve) /
    /// [`reserve_exact`](crate::SmallVec::reserve_exact) call.
    Reserve,
    /// A [`grow`](crate::SmallVec::grow) call.
    Grow,
    /// A [`shrink_to_fit`](crate::SmallVec::shrink_to_fit) /
    /// [`shrink_to`](crate::SmallVec::shrink_to) call.
    Shrink,
    /// A [`clone`](Clone::clone) call on the source container.
    Clone,
    /// A drain operation (via [`Splice`](crate::Splice)'s internal tail
    /// moving).
    Drain,
    /// A [`splice`](crate::SmallVec::splice) call.
    Splice
}

/// An observed storage migration event.
///
/// The event contains *only* metadata about the migration: its
/// [`kind`](MigrationKind), the [`cause`](MigrationCause), the capacity
/// before and after the migration, and the identity of the container that
/// migrated. It never carries any element values.
#[derive(Debug, Clone)]
pub struct MigrationEvent {
    /// The kind of migration that occurred.
    pub kind: MigrationKind,
    /// The container-level operation that triggered the migration.
    pub cause: MigrationCause,
    /// The capacity (in elements) before the migration.
    pub old_capacity: usize,
    /// The capacity (in elements) after the migration.
    pub new_capacity: usize,
    /// Identity of the container that migrated.
    ///
    /// This is stable for the container's whole lifetime, including across
    /// moves (e.g. the return value of [`clone`](Clone::clone)).
    pub container: ContainerId
}

/// A callback that receives [`MigrationEvent`]s for one container.
///
/// A blanket implementation is provided for every
/// `FnMut(&MigrationEvent) + 'static` closure.
pub trait MigrationObserver: 'static {
    /// Called after a storage migration completed successfully.
    ///
    /// The container is in a consistent state when this is called, but the
    /// callback must not re-enter the container (see
    /// [`SmallVec::set_observer`](crate::SmallVec::set_observer)).
    fn on_migration(&mut self, event: &MigrationEvent);

    /// Called when a container with this observer is cloned.
    ///
    /// Return `Some` to install a clone of this observer on the new
    /// container. The default implementation returns `None`, so closures
    /// (which are not generally cloneable) are simply not propagated.
    fn clone_observer(&self) -> Option<Box<dyn MigrationObserver>> {
        None
    }
}

impl<F> MigrationObserver for F
where F: FnMut(&MigrationEvent) + 'static
{
    #[inline]
    fn on_migration(&mut self, event: &MigrationEvent) {
        self(event)
    }
}

/// The observer state stored inside a [`SmallVec`](crate::SmallVec).
pub(crate) struct ObserverSlot {
    pub(crate) callback: Box<dyn MigrationObserver>
}
