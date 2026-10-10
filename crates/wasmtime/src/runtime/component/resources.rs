mod any;
mod host;
mod host_dynamic;
mod host_static;
mod host_tables;
mod ty;

use crate::hash_map::HashMap;

pub use any::*;
pub use host_dynamic::*;
pub use host_static::*;
pub use host_tables::*;
pub use ty::*;

use crate::prelude::*;

/// Trait for types that can report the host heap memory allocated for their children.
///
/// This trait must be implemented by any type `T` stored in a [`ResourceTable`].
/// Implementations should report only dynamically allocated bytes owned by
/// descendants of `self` (such as `Vec` capacity, `String` capacity, or boxed
/// children), **not** the inline bytes of `self` (`size_of_val(self)`).
///
/// For types that do not own separate heap allocations (scalars, fixed structs,
/// OS descriptors, `Arc` handles), implementing [`FixedHostHeapUsage`] is
/// sufficient and automatically provides an implementation returning `0`.
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
///     fn heap_size_of_children(&self) -> usize {
///         self.name.heap_size_of_children() + self.buffer.heap_size_of_children()
///     }
/// }
/// ```
///
/// ### Deriving
///
/// In most cases, these traits should be derived using `#[derive(HostHeapUsage)]`
/// or `#[derive(FixedHostHeapUsage)]`. Deriving `FixedHostHeapUsage` provides a
/// compile-time guarantee that all constituent fields implement `FixedHostHeapUsage`,
/// preventing any new field from silently adding unmetered heap allocations.
///
/// [`ResourceTable`]: super::ResourceTable
pub trait HostHeapUsage {
    /// Returns the number of bytes allocated on the heap owned by descendants
    /// of this value, excluding `core::mem::size_of_val(self)`.
    fn heap_size_of_children(&self) -> usize;

    /// Returns the total memory footprint of this value (inline size + heap children).
    /// Used by `ResourceTable` to account for both the table slot and heap data.
    #[inline]
    fn total_host_size(&self) -> usize {
        core::mem::size_of_val(self) + self.heap_size_of_children()
    }
}

/// Marker trait for types whose heap footprint is **constant** (that is,
/// mutation through `&mut self` can never change the value returned by
/// [`HostHeapUsage::heap_size_of_children`]).
///
/// This is the case for any type that owns no separately heap-allocated data:
/// plain structs and enums composed of scalars, OS handles, and zero-sized types.
/// Note that while an `Arc` or `Rc` pointer is fixed-size inline, types wrapping
/// shared pointers should only implement `FixedHostHeapUsage` if the underlying
/// pointee cannot accumulate unbounded unmetered host heap allocations.
///
/// # Safety contract
///
/// Implementing this trait is a promise that `heap_size_of_children()` returns the
/// same value before and after any `&mut self` mutation. Violating this
/// contract will cause the [`ResourceTable`]'s usage counter to silently drift.
///
/// Implementing `FixedHostHeapUsage` automatically provides a [`HostHeapUsage`]
/// implementation via a blanket impl that returns `0`.
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
/// entirely of such types, OS handles, and zero-sized types.
///
/// Do **not** implement this trait when the type owns heap allocations whose
/// size grows with runtime data (for example, types with `String`, `Vec<T>`,
/// `Box<T>`, or `HashMap` fields). For those types, implement [`HostHeapUsage`]
/// directly and use [`ResourceTable::update_resource`] for mutation.
///
/// [`ResourceTable`]: super::ResourceTable
pub trait FixedHostHeapUsage: Sized {}

/// Blanket [`HostHeapUsage`] implementation for all [`FixedHostHeapUsage`] types.
///
/// Returns `0`, as all bytes of the value live inline in `self`.
impl<T: FixedHostHeapUsage> HostHeapUsage for T {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        0
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
impl FixedHostHeapUsage for core::time::Duration {}
impl FixedHostHeapUsage for core::net::IpAddr {}
impl FixedHostHeapUsage for core::net::Ipv4Addr {}
impl FixedHostHeapUsage for core::net::Ipv6Addr {}
impl FixedHostHeapUsage for core::net::SocketAddr {}
impl FixedHostHeapUsage for core::net::SocketAddrV4 {}
impl FixedHostHeapUsage for core::net::SocketAddrV6 {}

/// [`wasmtime::Error`] owns an opaque boxed error value. Internal error allocations
/// are inconsequential and out of scope for resource table limits.
impl HostHeapUsage for crate::Error {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        0
    }
}

/// `anyhow::Error` owns an opaque boxed error value. Internal error allocations
/// are inconsequential and out of scope for resource table limits.
#[cfg(feature = "anyhow")]
impl HostHeapUsage for crate::anyhow::Error {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        0
    }
}

impl<T: HostHeapUsage> HostHeapUsage for Option<T> {
    fn heap_size_of_children(&self) -> usize {
        match self {
            Some(t) => t.heap_size_of_children(),
            None => 0,
        }
    }
}

impl<T: HostHeapUsage, E: HostHeapUsage> HostHeapUsage for Result<T, E> {
    fn heap_size_of_children(&self) -> usize {
        match self {
            Ok(t) => t.heap_size_of_children(),
            Err(e) => e.heap_size_of_children(),
        }
    }
}

impl HostHeapUsage for String {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        self.capacity()
    }
}

// Traversal of collection elements makes heap usage queries linear in
// collection size. For large collections with frequent mutations, tracked
// wrappers (`TrackedVec`, `TrackedHashSet`, and `TrackedHashMap`) maintain
// a running byte count in constant time.
impl<T: HostHeapUsage> HostHeapUsage for Vec<T> {
    fn heap_size_of_children(&self) -> usize {
        self.capacity() * core::mem::size_of::<T>()
            + self
                .iter()
                .map(|t| t.heap_size_of_children())
                .sum::<usize>()
    }
}

// Linear traversal overhead; see Vec note above.
impl<T: HostHeapUsage> HostHeapUsage for std::collections::HashSet<T> {
    fn heap_size_of_children(&self) -> usize {
        let capacity = self.capacity();
        let mut total = if capacity > 0 {
            capacity * (core::mem::size_of::<T>() + 1)
        } else {
            0
        };

        for item in self.iter() {
            total += item.heap_size_of_children();
        }

        total
    }
}

// Linear traversal overhead; see Vec note above.
impl<K, V> HostHeapUsage for HashMap<K, V>
where
    K: HostHeapUsage,
    V: HostHeapUsage,
{
    fn heap_size_of_children(&self) -> usize {
        let capacity = self.capacity();
        let mut total = if capacity > 0 {
            capacity * (core::mem::size_of::<K>() + core::mem::size_of::<V>() + 1)
        } else {
            0
        };

        for (key, value) in self.iter() {
            total += key.heap_size_of_children();
            total += value.heap_size_of_children();
        }

        total
    }
}

// wasmtime core types stored in ResourceTable are handles backed by a store-
// internal index; their heap is owned by the store, not the handle itself.
impl FixedHostHeapUsage for crate::Func {}
impl FixedHostHeapUsage for crate::Global {}
impl FixedHostHeapUsage for crate::Memory {}
impl FixedHostHeapUsage for crate::Table {}
impl FixedHostHeapUsage for crate::Tag {}
impl FixedHostHeapUsage for crate::Instance {}
impl FixedHostHeapUsage for crate::Module {}
#[cfg(feature = "debug")]
impl FixedHostHeapUsage for crate::FrameHandle {}
#[cfg(feature = "threads")]
impl FixedHostHeapUsage for crate::SharedMemory {}
