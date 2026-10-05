# rust-smallvec

> [!WARNING]
> smallvec v2 is on pre-release
>
> this means that there might be unexpected changes between versions. see the changelog
> for detailed breakdowns
>
> changes include, but are not limited to:
> - breaking API changes
> - deprecation of items
> - adding, removing or modifying features
> 
> beware that this is a pre-release and we can't ensure that there are no vulnerabilities
> or corner cases. if this feels like a substantial risk to you, please downgrade to the
> latest v1 version
> 
> we are looking for people to test this v2 version, so feel free to play around with
> smallvec and use it for your purposes if this warning is not a concern, and please file
> an issue on the repo if you find some unexpected behavior or have a request for a feature

[Release notes](https://github.com/servo/rust-smallvec/releases)

Small vectors in various sizes. These store a certain number of elements
inline, and fall back to the heap for larger allocations.  This can be a
useful optimization for improving cache locality and reducing allocator
traffic for workloads that fit within the inline buffer.

## Example

```rust
use smallvec::SmallVec;

// This SmallVec can hold up to 4 items on the stack:
let mut v: SmallVec<i32, 4> = SmallVec::from([1, 2, 3, 4]);

// It will automatically move its contents to the heap if
// contains more than four items:
v.push(5);

// SmallVec points to a slice, so you can use normal slice
// indexing and other methods to access its contents:
v[0] = v[1] + v[2];
v.sort();
```

## Feature List

- `allocator-api` (nightly): uses the `allocator-api` backend for the allocator API
- `allocator-api2`: uses the `allocator-api2` backend for the allocator API
- `arbitrary`: implements `Arbitrary` for any `SmallVec` storing elements that implement `Arbitrary`
- `borsh`: implements `BorshSerialize`, `BorshDeserialize` and `BorshSchema`
- `bytes`: implements `BufMut` for SmallVec
- `defmt`: implements `defmt::Format` for SmallVec
- `encase`: implements encasing as a runtime-sized array
- `internals`: exports through the public API `LocatedLength` and `RawSmallVec`
- `malloc_size_of`: implements `MallocSizeOf` and `MallocShallowSizeOf`
- `observe`: adds a storage migration observer (`MigrationEvent`, `MigrationKind`, `MigrationCause`); see below
- `rayon`: implements parallel iteration
- `serde`: implements serde's serialization and deserialization
- `specialization` (nightly): enables specialization, improving performance on some cases
- `std`: implements the `std::io::Write` type for `SmallVec<u8, N>`

> [!NOTE]
> - SmallVec without any features enabled does not make use of the standard library.
> - The `rayon` feature implicitly requires `std`.
> - The `allocator-api2` feature requires `allocator-api`

## Storage migration observer

The optional `observe` feature adds an opt-in observer that records the
*actual* storage migrations a `SmallVec` undergoes: inline → heap spills,
heap reallocations, and heap → inline returns. Every event reports the
capacity before and after the migration, the container-level operation that
triggered it, and a stable identity of the container that migrated. Events
never carry element values.

```rust
use smallvec::{MigrationKind, SmallVec};

let mut v: SmallVec<i32, 2> = SmallVec::new();
v.set_observer(|event| {
    // event.kind, event.cause, event.old_capacity, event.new_capacity, ...
});

v.push(1);
v.push(2);
v.push(3); // one spill: capacity 2 -> 4
```

When the `observe` feature is disabled, the observer API and its cost are
entirely absent.

### Targeted cargo commands

```sh
# Run the observer tests:
cargo test --features observe --test observe

# Run the migration demo:
cargo run --example migration_demo --features observe

# Check the observer-enabled build:
cargo check --features observe
```