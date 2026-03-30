mod any;
mod host;
mod host_dynamic;
mod host_static;
mod host_tables;
mod ty;

pub use any::*;
pub use host_dynamic::*;
pub use host_static::*;
pub use host_tables::*;
pub use ty::*;

use crate::prelude::*;
use crate::anyhow;

/// Trait for types that can report their host heap memory usage.
///
/// This trait must be implemented by any type `T` that can be stored in a
/// [`ResourceTable`]. The `host_heap_usage` method should return an estimate
/// of the total memory (in bytes) allocated for this resource, including the
/// inline size of the value itself (`std::mem::size_of_val(self)`) plus any
/// additional heap allocations it owns (e.g. `Vec` capacity, `String`
/// capacity, nested boxed values, etc.).
///
/// The minimum correct implementation for any type is
/// `std::mem::size_of_val(self)`, which accounts for the inline footprint that
/// the resource table entry occupies. Returning `0` is never correct.
///
/// # Examples
///
/// ```
/// use wasmtime::component::HostHeapUsage;
///
/// struct MyResource {
///     name: String,
///     buffer: Vec<u8>,
/// }
///
/// impl HostHeapUsage for MyResource {
///     fn host_heap_usage(&self) -> usize {
///         std::mem::size_of_val(self)
///             + self.name.capacity()
///             + self.buffer.capacity()
///     }
/// }
/// ```
///
/// [`ResourceTable`]: super::ResourceTable
pub trait HostHeapUsage {
    /// Returns the number of bytes of memory used by this value, including its
    /// inline size and any heap allocations it owns.
    fn host_heap_usage(&self) -> usize;
}

/// Marker trait for types whose host heap footprint is **constant** — that is,
/// mutation through `&mut self` can never change the value returned by
/// [`HostHeapUsage::host_heap_usage`].
///
/// This is the case for any type that owns no separately heap-allocated data:
/// plain structs and enums composed of scalars, `Arc`/`Rc` wrappers (the
/// pointee is shared, not solely owned), OS handles, and zero-sized types.
///
/// # Safety contract
///
/// Implementing this trait is a promise that `host_heap_usage()` returns the
/// same value before and after any `&mut self` mutation. Violating this
/// contract will cause the [`ResourceTable`]'s usage counter to silently drift.
///
/// Implementing `FixedHostHeapUsage` automatically provides a [`HostHeapUsage`]
/// implementation via a blanket impl that returns `core::mem::size_of_val(self)`.
/// You do not need to implement `HostHeapUsage` separately.
///
/// # Payoff
///
/// Types that implement `FixedHostHeapUsage` may use
/// [`ResourceTable::get_mut`], which returns a plain `&mut T` with no
/// overhead. Types that only implement [`HostHeapUsage`] must use
/// [`ResourceTable::update_resource`], which accepts a closure and updates
/// the table's usage counter around the mutation.
///
/// # When to implement this trait
///
/// Implement `FixedHostHeapUsage` when every byte owned by the value lives
/// *inline* within the value itself: primitives, plain enums, structs composed
/// entirely of such types, `Arc`/`Rc` wrappers (where the pointee is shared and
/// not solely owned by this value), OS handles, and zero-sized types.
///
/// Do **not** implement this trait when the type owns heap allocations whose
/// size grows with runtime data — for example types with `String`, `Vec<T>`,
/// `Box<T>`, or `HashMap` fields. For those types, implement [`HostHeapUsage`]
/// directly and use [`ResourceTable::update_resource`] for mutation.
///
/// [`ResourceTable`]: super::ResourceTable
pub trait FixedHostHeapUsage: Sized {}

/// Blanket [`HostHeapUsage`] implementation for all [`FixedHostHeapUsage`] types.
///
/// Returns `core::mem::size_of_val(self)`, which is the complete and correct
/// footprint for any type whose heap usage never changes through mutation.
impl<T: FixedHostHeapUsage> HostHeapUsage for T {
    fn host_heap_usage(&self) -> usize {
        core::mem::size_of_val(self)
    }
}

// Primitive and scalar types are all fixed-size.
impl FixedHostHeapUsage for () {}
impl FixedHostHeapUsage for bool {}
impl FixedHostHeapUsage for u8 {}
impl FixedHostHeapUsage for u16 {}
impl FixedHostHeapUsage for u32 {}
impl FixedHostHeapUsage for u64 {}
impl FixedHostHeapUsage for u128 {}
impl FixedHostHeapUsage for usize {}
impl FixedHostHeapUsage for i8 {}
impl FixedHostHeapUsage for i16 {}
impl FixedHostHeapUsage for i32 {}
impl FixedHostHeapUsage for i64 {}
impl FixedHostHeapUsage for i128 {}
impl FixedHostHeapUsage for isize {}
impl FixedHostHeapUsage for f32 {}
impl FixedHostHeapUsage for f64 {}
impl FixedHostHeapUsage for char {}

/// `anyhow::Error` owns a heap-allocated boxed error value of unknown size.
/// We report the inline pointer size only; the actual allocation is opaque.
///
/// TODO: if anyhow ever exposes a way to query the inner allocation size,
/// use it here.
impl HostHeapUsage for anyhow::Error {
    fn host_heap_usage(&self) -> usize {
        core::mem::size_of_val(self)
    }
}

impl<T: HostHeapUsage> HostHeapUsage for Option<T> {
    fn host_heap_usage(&self) -> usize {
        // size_of_val(self) already includes the inline footprint of T within
        // the Option layout, so we only add the *heap* portion of the inner
        // value's usage (i.e. total minus its inline size).
        core::mem::size_of_val(self)
            + match self {
                Some(t) => t.host_heap_usage().saturating_sub(core::mem::size_of::<T>()),
                None => 0,
            }
    }
}

impl<T: HostHeapUsage, E: HostHeapUsage> HostHeapUsage for Result<T, E> {
    fn host_heap_usage(&self) -> usize {
        // Same reasoning as Option<T>: size_of_val(self) already covers the
        // inline footprint of either variant.
        core::mem::size_of_val(self)
            + match self {
                Ok(t) => t.host_heap_usage().saturating_sub(core::mem::size_of::<T>()),
                Err(e) => e.host_heap_usage().saturating_sub(core::mem::size_of::<E>()),
            }
    }
}

impl HostHeapUsage for String {
    fn host_heap_usage(&self) -> usize {
        core::mem::size_of_val(self) + self.capacity()
    }
}

impl<T: HostHeapUsage> HostHeapUsage for Vec<T> {
    fn host_heap_usage(&self) -> usize {
        // The Vec's inline struct (pointer, length, capacity) plus the heap
        // buffer (capacity * element size) plus any heap owned by each live
        // element beyond its inline footprint.
        core::mem::size_of_val(self)
            + self.capacity() * core::mem::size_of::<T>()
            + self
                .iter()
                .map(|t| t.host_heap_usage().saturating_sub(core::mem::size_of::<T>()))
                .sum::<usize>()
    }
}
