//! Explicit storage-migration observation for [`SmallVec`].
//!
//! [`ObservedSmallVec`] is an opt-in wrapper around [`SmallVec`] that reports
//! the *actual* storage migrations the container performs:
//!
//! - **inline -> heap** ("spill"), e.g. pushing a third item into a vector with
//!   inline capacity 2,
//! - **heap -> heap** reallocations (growth, and shrinking while spilled),
//! - **heap -> inline** transitions (e.g. `shrink_to_fit` after popping).
//!
//! Migrations are detected from the container's own location tag
//! ([`SmallVec::spilled`]) and its real [`capacity`][SmallVec::capacity]
//! sampled immediately before and after each operation -- never inferred
//! from the length. Every [`MigrationEvent`] carries the [`ContainerId`] of
//! the instance that performed the migration, the [`Trigger`] operation, and
//! the capacities before and after. Element values are never observed:
//! [`MigrationEvent`] does not even mention the element type.
//!
//! ## Container identity
//!
//! Each [`ObservedSmallVec`] gets a fresh [`ContainerId`], including the ones
//! produced by [`clone_observed`][ObservedSmallVec::clone_observed] and
//! [`split_off`][ObservedSmallVec::split_off]. Birth migrations of a clone
//! are reported through the *source's* observer but stamped with the
//! *clone's* identity, and [`drain`][ObservedSmallVec::drain] always records
//! against the instance it drains, so events can never be confused across
//! instances.
//!
//! ## Cost when observation is disabled
//!
//! - [`SmallVec`] itself is untouched: code that does not use this wrapper
//!   keeps the original API and pays nothing, and disabling the
//!   `migration-observer` cargo feature removes this module entirely.
//! - An [`ObservedSmallVec`] without an installed observer performs one
//!   predictable branch per operation and records nothing.
//!
//! ## Re-entrancy
//!
//! Callbacks run *after* the triggering operation has fully completed, but
//! still inside the wrapper's mutable call stack. Re-entering the observed
//! container from within its own migration callback is explicitly rejected:
//! every mutating entry point panics with a clear message when called while
//! a callback is in flight, and the container stays usable afterwards.
//!
//! ## Targeted commands
//!
//! ```text
//! cargo test --test migration --features migration-observer
//! cargo run --example migration_demo --features migration-observer
//! cargo check            # observation disabled: original API and cost
//! ```

use {
    crate::{
        Allocator,
        Drain,
        Global,
        SmallVec,
        Vec
    },
    alloc::boxed::Box,
    core::{
        cell::Cell,
        marker::PhantomData,
        ops::{
            Deref,
            RangeBounds
        },
        ptr::NonNull,
        sync::atomic::{
            AtomicUsize,
            Ordering
        }
    }
};

/// Unique identity of an observed container instance.
///
/// A fresh identity is assigned to every [`ObservedSmallVec`], including the
/// ones produced by [`ObservedSmallVec::clone_observed`] and
/// [`ObservedSmallVec::split_off`], so events from different instances can
/// never be confused with one another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ContainerId(pub usize);

impl ContainerId {
    fn next() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        ContainerId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The kind of storage migration that actually happened.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MigrationKind {
    /// Inline (stack) storage was replaced by a heap allocation: a "spill".
    InlineToHeap,
    /// One heap allocation was replaced by another (growth, or shrinking
    /// while remaining spilled).
    HeapRealloc,
    /// The heap allocation was released and the elements moved back into
    /// the inline storage.
    HeapToInline
}

/// The operation that triggered a storage migration.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Trigger {
    WithCapacity,
    FromVec,
    FromSlice,
    Clone,
    SplitOff,
    Push,
    Insert,
    Extend,
    Resize,
    Reserve,
    ReserveExact,
    Grow,
    ShrinkToFit,
    ShrinkTo,
    Drain
}

/// A single, actually-performed storage migration.
///
/// Reports the container identity, the kind of migration, the triggering
/// operation, and the capacities before and after. Element values are never
/// recorded -- this type deliberately has no element type parameter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MigrationEvent {
    /// Identity of the container instance that migrated.
    pub container: ContainerId,
    /// Which kind of migration happened.
    pub kind: MigrationKind,
    /// The operation that triggered the migration.
    pub trigger: Trigger,
    /// Length of the container right after the triggering operation.
    pub len: usize,
    /// Capacity before the triggering operation.
    pub capacity_before: usize,
    /// Capacity after the triggering operation.
    pub capacity_after: usize
}

/// A [`SmallVec`] wrapper that reports its actual storage migrations to an
/// installed observer. See the [module documentation](self) for details.
pub struct ObservedSmallVec<'a, T, const N: usize, A: Allocator = Global> {
    inner: SmallVec<T, N, A>,
    id: ContainerId,
    observer: Option<Box<dyn FnMut(MigrationEvent) + 'a>>,
    in_callback: Cell<bool>
}

impl<'a, T, const N: usize> ObservedSmallVec<'a, T, N, Global> {
    /// Creates an empty, inline observed vector.
    #[inline]
    pub fn new() -> Self {
        Self::adopt(SmallVec::new(), Trigger::WithCapacity, None)
    }

    /// Creates an observed vector with at least the requested capacity.
    ///
    /// If `capacity` exceeds the inline capacity the vector is born spilled;
    /// install the observer up front with [`Self::with_capacity_observed`]
    /// to catch that birth migration.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::adopt(
            SmallVec::with_capacity(capacity),
            Trigger::WithCapacity,
            None
        )
    }

    /// [`with_capacity`][Self::with_capacity], with the observer installed
    /// before the allocation happens.
    #[inline]
    pub fn with_capacity_observed(
        capacity: usize,
        observer: impl FnMut(MigrationEvent) + 'a
    ) -> Self {
        Self::adopt(
            SmallVec::with_capacity(capacity),
            Trigger::WithCapacity,
            Some(Box::new(observer))
        )
    }

    /// Wraps `SmallVec::from_vec`. A heap-allocated source vector stays on
    /// the heap and is *not* misjudged as inline, even when it is short.
    #[inline]
    pub fn from_vec(vec: Vec<T>) -> Self {
        Self::adopt(SmallVec::from_vec(vec), Trigger::FromVec, None)
    }

    /// [`from_vec`][Self::from_vec], with the observer installed up front.
    #[inline]
    pub fn from_vec_observed(vec: Vec<T>, observer: impl FnMut(MigrationEvent) + 'a) -> Self {
        Self::adopt(
            SmallVec::from_vec(vec),
            Trigger::FromVec,
            Some(Box::new(observer))
        )
    }
}

impl<'a, T: Copy, const N: usize> ObservedSmallVec<'a, T, N, Global> {
    /// Wraps `SmallVec::from_slice_copy`.
    #[inline]
    pub fn from_slice_copy(slice: &[T]) -> Self {
        Self::adopt(SmallVec::from_slice_copy(slice), Trigger::FromSlice, None)
    }

    /// [`from_slice_copy`][Self::from_slice_copy], with the observer
    /// installed up front.
    #[inline]
    pub fn from_slice_copy_observed(
        slice: &[T],
        observer: impl FnMut(MigrationEvent) + 'a
    ) -> Self {
        Self::adopt(
            SmallVec::from_slice_copy(slice),
            Trigger::FromSlice,
            Some(Box::new(observer))
        )
    }
}

impl<'a, T, const N: usize, A: Allocator> ObservedSmallVec<'a, T, N, A> {
    /// Wraps `SmallVec::new_in`.
    #[inline]
    pub fn new_in(allocator: A) -> Self {
        Self::adopt(SmallVec::new_in(allocator), Trigger::WithCapacity, None)
    }

    /// Wraps `SmallVec::with_capacity_in`.
    #[inline]
    pub fn with_capacity_in(capacity: usize, allocator: A) -> Self {
        Self::adopt(
            SmallVec::with_capacity_in(capacity, allocator),
            Trigger::WithCapacity,
            None
        )
    }

    /// Adopts an already-built vector. If it is spilled (e.g. pre-allocated
    /// beyond the inline capacity), a birth [`MigrationKind::InlineToHeap`]
    /// event attributed to `trigger` is reported to `observer`, so a
    /// heap-born vector is never misjudged as inline.
    fn adopt(
        inner: SmallVec<T, N, A>,
        trigger: Trigger,
        observer: Option<Box<dyn FnMut(MigrationEvent) + 'a>>
    ) -> Self {
        let mut this = Self {
            inner,
            id: ContainerId::next(),
            observer,
            in_callback: Cell::new(false)
        };
        if this.inner.spilled() {
            let event = MigrationEvent {
                container: this.id,
                kind: MigrationKind::InlineToHeap,
                trigger,
                len: this.inner.len(),
                capacity_before: SmallVec::<T, N, A>::inline_size(),
                capacity_after: this.inner.capacity()
            };
            this.emit(event);
        }
        this
    }

    /// The identity of this container instance.
    #[inline]
    pub fn id(&self) -> ContainerId {
        self.id
    }

    /// Whether an observer is currently installed.
    #[inline]
    pub fn observation_enabled(&self) -> bool {
        self.observer.is_some()
    }

    /// Installs or replaces the migration observer.
    #[inline]
    pub fn set_observer(&mut self, observer: impl FnMut(MigrationEvent) + 'a) {
        self.assert_not_in_callback();
        self.observer = Some(Box::new(observer));
    }

    /// Removes the current observer, restoring the zero-bookkeeping
    /// behavior.
    #[inline]
    pub fn clear_observer(&mut self) {
        self.assert_not_in_callback();
        self.observer = None;
    }

    /// Unwraps the inner [`SmallVec`], ending observation.
    #[inline]
    pub fn into_inner(self) -> SmallVec<T, N, A> {
        self.inner
    }

    #[inline]
    #[track_caller]
    fn assert_not_in_callback(&self) {
        assert!(
            !self.in_callback.get(),
            "re-entrant access to an `ObservedSmallVec` from within its own \
             migration callback is forbidden"
        );
    }

    /// Samples the actual storage location and capacity, but only when an
    /// observer is installed -- with observation disabled this is a single
    /// branch and nothing is read.
    #[inline]
    fn snapshot(&self) -> Option<(bool, usize)> {
        if self.observer.is_some() {
            Some((self.inner.spilled(), self.inner.capacity()))
        } else {
            None
        }
    }

    /// Compares the pre-operation snapshot with the actual post-operation
    /// state (read from the container's own location tag, never guessed from
    /// the length) and reports the migration, if any.
    fn record(&mut self, trigger: Trigger, before: Option<(bool, usize)>) {
        let Some((was_heap, capacity_before)) = before else {
            return;
        };
        let is_heap = self.inner.spilled();
        let capacity_after = self.inner.capacity();
        let kind = match (was_heap, is_heap) {
            (false, true) => Some(MigrationKind::InlineToHeap),
            (true, false) => Some(MigrationKind::HeapToInline),
            (true, true) if capacity_before != capacity_after => Some(MigrationKind::HeapRealloc),
            _ => None
        };
        if let Some(kind) = kind {
            let event = MigrationEvent {
                container: self.id,
                kind,
                trigger,
                len: self.inner.len(),
                capacity_before,
                capacity_after
            };
            self.emit(event);
        }
    }

    fn emit(&mut self, event: MigrationEvent) {
        let Some(observer) = self.observer.as_mut() else {
            return;
        };

        /// Releases the re-entrancy flag even if the callback panics, so the
        /// container is not left permanently rejecting calls.
        struct ResetFlag<'b>(&'b Cell<bool>);
        impl Drop for ResetFlag<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }

        assert!(
            !self.in_callback.replace(true),
            "nested `ObservedSmallVec` migration callback"
        );
        let _reset = ResetFlag(&self.in_callback);
        observer(event);
    }

    #[inline]
    pub fn push(&mut self, value: T) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.push(value);
        self.record(Trigger::Push, before);
    }

    #[inline]
    pub fn insert(&mut self, index: usize, value: T) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.insert(index, value);
        self.record(Trigger::Insert, before);
    }

    #[inline]
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.extend(iter);
        self.record(Trigger::Extend, before);
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.reserve(additional);
        self.record(Trigger::Reserve, before);
    }

    #[inline]
    pub fn reserve_exact(&mut self, additional: usize) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.reserve_exact(additional);
        self.record(Trigger::ReserveExact, before);
    }

    #[inline]
    pub fn grow(&mut self, new_capacity: usize) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.grow(new_capacity);
        self.record(Trigger::Grow, before);
    }

    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.shrink_to_fit();
        self.record(Trigger::ShrinkToFit, before);
    }

    #[inline]
    pub fn shrink_to(&mut self, min_capacity: usize) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.shrink_to(min_capacity);
        self.record(Trigger::ShrinkTo, before);
    }

    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.assert_not_in_callback();
        self.inner.pop()
    }

    #[inline]
    pub fn truncate(&mut self, length: usize) {
        self.assert_not_in_callback();
        self.inner.truncate(length);
    }

    #[inline]
    pub fn clear(&mut self) {
        self.assert_not_in_callback();
        self.inner.clear();
    }

    /// Removes a range of elements, yielding them by value.
    ///
    /// The returned iterator records any storage migration against *this*
    /// container's identity when it is dropped.
    #[inline]
    pub fn drain<R>(&mut self, range: R) -> ObservedDrain<'_, 'a, T, N, A>
    where R: RangeBounds<usize> {
        self.assert_not_in_callback();
        let before = self.snapshot();
        // Raw pointer to the owner, taken before `inner.drain` borrows
        // `self.inner`; tied to the `&mut self` borrow by the phantom member
        // of `ObservedDrain`, so it stays valid and exclusive for the whole
        // lifetime of the drain.
        let owner = NonNull::from(&mut *self);
        let drain = self.inner.drain(range);
        ObservedDrain {
            drain: Some(drain),
            owner,
            before,
            _marker: PhantomData
        }
    }
}

impl<'a, T: Clone, const N: usize, A: Allocator> ObservedSmallVec<'a, T, N, A> {
    #[inline]
    pub fn extend_from_slice(&mut self, other: &[T]) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.extend_from_slice(other);
        self.record(Trigger::Extend, before);
    }

    #[inline]
    pub fn resize(&mut self, length: usize, value: T) {
        self.assert_not_in_callback();
        let before = self.snapshot();
        self.inner.resize(length, value);
        self.record(Trigger::Resize, before);
    }
}

impl<'a, T: Clone, const N: usize, A: Allocator + Clone> ObservedSmallVec<'a, T, N, A> {
    /// Clones the contents into a new observed container with its **own**
    /// identity. If the clone spills while being built, that birth migration
    /// is reported through *this* container's observer but stamped with the
    /// *clone's* [`ContainerId`] -- never with the source's. The clone starts
    /// out without an observer; install one with
    /// [`set_observer`][Self::set_observer].
    pub fn clone_observed(&mut self) -> ObservedSmallVec<'static, T, N, A> {
        self.assert_not_in_callback();
        let inner = self.inner.clone();
        let clone = ObservedSmallVec {
            inner,
            id: ContainerId::next(),
            observer: None,
            in_callback: Cell::new(false)
        };
        if clone.inner.spilled() {
            let event = MigrationEvent {
                container: clone.id,
                kind: MigrationKind::InlineToHeap,
                trigger: Trigger::Clone,
                len: clone.inner.len(),
                capacity_before: SmallVec::<T, N, A>::inline_size(),
                capacity_after: clone.inner.capacity()
            };
            self.emit(event);
        }
        clone
    }

    /// Splits the collection into two at the given index. The returned
    /// container gets its own identity; its birth migration (if it spills)
    /// is reported through this container's observer under the new identity.
    pub fn split_off(&mut self, at: usize) -> ObservedSmallVec<'static, T, N, A> {
        self.assert_not_in_callback();
        let inner = self.inner.split_off(at);
        let other = ObservedSmallVec {
            inner,
            id: ContainerId::next(),
            observer: None,
            in_callback: Cell::new(false)
        };
        if other.inner.spilled() {
            let event = MigrationEvent {
                container: other.id,
                kind: MigrationKind::InlineToHeap,
                trigger: Trigger::SplitOff,
                len: other.inner.len(),
                capacity_before: SmallVec::<T, N, A>::inline_size(),
                capacity_after: other.inner.capacity()
            };
            self.emit(event);
        }
        other
    }
}

impl<T, const N: usize> Default for ObservedSmallVec<'_, T, N, Global> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Read-only access to the inner vector. There is deliberately no
/// `DerefMut`: mutating the inner `SmallVec` directly would bypass
/// observation.
impl<T, const N: usize, A: Allocator> Deref for ObservedSmallVec<'_, T, N, A> {
    type Target = SmallVec<T, N, A>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Draining iterator returned by [`ObservedSmallVec::drain`]. When dropped,
/// it reports any storage migration against the identity of the container
/// it was created from.
pub struct ObservedDrain<'a, 'obs, T, const N: usize, A: Allocator = Global> {
    drain: Option<Drain<'a, T, N, A>>,
    owner: NonNull<ObservedSmallVec<'obs, T, N, A>>,
    before: Option<(bool, usize)>,
    // Ties the drain to the exclusive borrow of the owner, keeping `owner`
    // valid and unaliased for the drain's whole lifetime.
    _marker: PhantomData<&'a mut ObservedSmallVec<'obs, T, N, A>>
}

impl<T, const N: usize, A: Allocator> Iterator for ObservedDrain<'_, '_, T, N, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        self.drain.as_mut().and_then(Drain::next)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.drain.as_ref().map_or((0, Some(0)), Drain::size_hint)
    }
}

impl<T, const N: usize, A: Allocator> Drop for ObservedDrain<'_, '_, T, N, A> {
    fn drop(&mut self) {
        // Finish the underlying drain first: its own `Drop` moves the
        // remaining tail elements back into place.
        drop(self.drain.take());
        // SAFETY: `owner` points to the `ObservedSmallVec` this drain was
        // created from. The phantom `&'a mut` member ties this drain's
        // lifetime to the exclusive borrow of that owner, so the owner is
        // alive and cannot be accessed by anyone else here.
        let owner = unsafe { self.owner.as_mut() };
        owner.record(Trigger::Drain, self.before);
    }
}
