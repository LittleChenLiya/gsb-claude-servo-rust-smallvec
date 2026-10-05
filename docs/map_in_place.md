# `SmallVec::map_in_place` — ownership and verification

`map_in_place` is a consumptive, same-element-type conversion entry. The
mapper takes ownership of each `T` and returns either a new `T` (success) or
a recovered `T` together with an error. No [`Clone`] bound is required.

[`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html

## Signature

```rust
pub fn map_in_place<F, E>(
    self,
    f: F
) -> Result<(), MapInPlaceError<T, N, A, E>>
where
    F: FnMut(T) -> Result<T, (T, E)>;
```

- `self` is consumed. The vector's original buffer is reused for the whole
  operation; no reallocation occurs, so a spilled vector keeps the same heap
  pointer.
- On success, `Ok(())` is returned and the vector is dropped normally (every
  element is a new value).
- On error, `Err(MapInPlaceError)` is returned, carrying:
  - `vec` — the partially converted vector,
  - `index` — the index of the failed element,
  - `error` — the mapper's error.

## Ownership semantics

### Success

Every element is read out of the buffer (taking ownership), passed to the
mapper, and replaced by the new value the mapper returns. The old value is
dropped by the mapper's scope when the mapper returns; the new value is
dropped when the vector drops.

### Error (mapper returns `Err((recovered, error))`)

This is **not** a whole-batch rollback. The returned vector is fully
initialized and valid:

- `0..index` are the new values produced by the mapper,
- slot `index` is filled with the **recovered** value the mapper returned,
- `index + 1..` are the original, untouched values.

The recovered value is whatever the mapper provides; it is **not** assumed to
be identical to the original element. The caller must not rely on it being
the original value.

The vector's original buffer is reused (no reallocation). A spilled vector
therefore has the same heap pointer before and after.

### Panic (mapper panics)

A drop guard is installed before the loop. While the mapper runs on element
`i`:

- `0..i` are converted new values,
- slot `i` is uninitialized (the element has been read out),
- `i + 1..` are untouched original values.

If the mapper panics, the guard drops `0..i` and `i + 1..` exactly once. The
element at index `i` is **not** dropped by the guard: it is owned by the
mapper and dropped (or leaked) by the mapper's unwinding. This guarantees:

- no double drop,
- no uninitialized slot,
- the completed prefix and unhandled suffix are each dropped exactly once.

The vector's own `Drop` impl sees length zero (set by the guard) and only
deallocates the buffer; it does not re-drop the elements.

## Cargo targeted commands

Run only the `map_in_place` tests:

```sh
cargo test --test main map_in_place
```

Run the doc test for `map_in_place`:

```sh
cargo test --doc map_in_place
```

Verify the unsafe code with Miri (requires a nightly toolchain with the
`miri` component):

```sh
cargo +nightly miri test --test main map_in_place
```

The drop-counting tests use a `DropCounter` fixture that increments a shared
`Cell<usize>` on drop. The expected counts are `2 * prefix_len + 1 +
suffix_len`: each converted element's old value is dropped in the mapper, the
panic element is dropped by the mapper's unwinding, and the guard drops the
new prefix and the original suffix. A count that is too high indicates a
double drop; a count that is too low (or undefined behavior under Miri)
indicates an uninitialized slot.
