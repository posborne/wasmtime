use super::Resource;
use super::resources::{FixedHostHeapUsage, HostHeapUsage};
use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use core::any::Any;
use core::fmt;
use core::mem;
use core::ops::{Deref, DerefMut};

#[derive(Debug)]
/// Errors returned by operations on `ResourceTable`
pub enum ResourceTableError {
    /// ResourceTable has no free keys
    Full,
    /// Resource not present in table
    NotPresent,
    /// Resource present in table, but with a different type
    WrongType,
    /// Resource cannot be deleted because child resources exist in the table. Consult wit docs for
    /// the particular resource to see which methods may return child resources.
    HasChildren,
    /// The host heap memory limit for this table has been exceeded.
    HostMemoryLimitExceeded,
}

impl fmt::Display for ResourceTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "resource table has no free keys"),
            Self::NotPresent => write!(f, "resource not present"),
            Self::WrongType => write!(f, "resource is of another type"),
            Self::HasChildren => write!(f, "resource has children"),
            Self::HostMemoryLimitExceeded => {
                write!(f, "host heap memory limit exceeded for resource table")
            }
        }
    }
}

impl core::error::Error for ResourceTableError {}

/// A mutable borrow of a variable-size resource from a [`ResourceTable`].
///
/// Returned by [`ResourceTable::borrow_mut`]. Derefs to `&mut T` for
/// convenient access. When the mutation is complete, call [`finish`](Self::finish)
/// to re-sample [`HostHeapUsage::host_heap_usage`] and update the table's
/// running total. `finish()` returns `Err` if the new size exceeds the
/// configured maximum — the mutation is not rolled back, but the caller
/// can react immediately.
///
/// If `finish()` is not called before the guard is dropped, the accounting
/// update is performed in `drop` as a safety net (but any limit-exceeded
/// error is silently ignored since `drop` cannot return errors).
pub struct ResourceBorrow<'a, T: HostHeapUsage> {
    /// Raw pointer to the owning table for the drop fallback.
    /// Valid for `'a`.
    table: *mut ResourceTable,
    /// The usage at the time the borrow was created.
    usage_before: usize,
    /// The mutable reference into the table entry.
    value: &'a mut T,
    /// Set to `true` once `finish()` has been called, so drop knows
    /// not to double-update.
    finished: bool,
}

impl<'a, T: HostHeapUsage> ResourceBorrow<'a, T> {
    /// Complete the mutable borrow, re-sampling [`HostHeapUsage::host_heap_usage`]
    /// and updating the table's running total.
    ///
    /// Returns [`ResourceTableError::HostMemoryLimitExceeded`] if the new usage
    /// exceeds the configured limit (the counter is still updated to reflect
    /// reality).
    pub fn finish(mut self) -> Result<(), ResourceTableError> {
        self.finished = true;
        let new_usage = self.value.host_heap_usage();
        // SAFETY: `table` is valid for `'a` and we hold exclusive access
        // through `value`.
        let table = unsafe { &mut *self.table };
        table.update_usage(self.usage_before, new_usage)
    }
}

impl<T: HostHeapUsage> Deref for ResourceBorrow<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: HostHeapUsage> DerefMut for ResourceBorrow<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T: HostHeapUsage> Drop for ResourceBorrow<'_, T> {
    fn drop(&mut self) {
        if !self.finished {
            let new_usage = self.value.host_heap_usage();
            // SAFETY: `table` is valid for the lifetime of the borrow.
            let table = unsafe { &mut *self.table };
            let _ = table.update_usage(self.usage_before, new_usage);
        }
    }
}

/// The `ResourceTable` type maps a `Resource<T>` to its `T`.
pub struct ResourceTable {
    entries: Vec<Entry>,
    free_head: Option<usize>,
    max_capacity: usize,
    /// The current total host heap usage (in bytes) of all resources in the table.
    current_host_heap_usage: usize,
    /// An optional maximum limit on total host heap usage (in bytes). If `Some`, any `push` or
    /// `update_resource` that would exceed this limit returns
    /// [`ResourceTableError::HostMemoryLimitExceeded`].
    max_host_heap_usage: Option<usize>,
}

#[derive(Debug)]
enum Entry {
    Free { next: Option<usize> },
    Occupied { entry: TableEntry },
}

impl Entry {
    pub fn occupied(&self) -> Option<&TableEntry> {
        match self {
            Self::Occupied { entry } => Some(entry),
            Self::Free { .. } => None,
        }
    }

    pub fn occupied_mut(&mut self) -> Option<&mut TableEntry> {
        match self {
            Self::Occupied { entry } => Some(entry),
            Self::Free { .. } => None,
        }
    }
}

struct Tombstone;

// Change this to `true` to assist with handle debugging in development if
// necessary.
const DELETE_WITH_TOMBSTONE: bool = false;

/// Default setting for `ResourceTable::max_capacity`, chosen to be high
/// enough that it doesn't need changing all that often but low enough that
/// exhausting it isn't a massive problem for the host.
const DEFAULT_MAX_CAPACITY: usize = 1_000_000;

/// This structure tracks parent and child relationships for a given table entry.
///
/// Parents and children are referred to by table index. We maintain the
/// following invariants:
/// * the parent must exist when adding a child.
/// * whenever a child is created, its index is added to children.
/// * whenever a child is deleted, its index is removed from children.
/// * an entry with children may not be deleted.
#[derive(Debug)]
struct TableEntry {
    /// The entry in the table, as a boxed dynamically-typed object
    entry: Box<dyn Any + Send>,
    /// The index of the parent of this entry, if it has one.
    parent: Option<u32>,
    /// The indices of any children of this entry.
    children: BTreeSet<u32>,
}

impl TableEntry {
    fn new(entry: Box<dyn Any + Send>, parent: Option<u32>) -> Self {
        Self {
            entry,
            parent,
            children: BTreeSet::new(),
        }
    }
    fn add_child(&mut self, child: u32) {
        debug_assert!(!self.children.contains(&child));
        self.children.insert(child);
    }
    fn remove_child(&mut self, child: u32) {
        let was_removed = self.children.remove(&child);
        debug_assert!(was_removed);
    }
}

impl ResourceTable {
    /// Create an empty table
    pub fn new() -> Self {
        ResourceTable::with_capacity(0)
    }

    /// Returns whether or not this table is empty.
    ///
    /// Note that this is an `O(n)` operation, where `n` is the number of
    /// entries in the backing `Vec`.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|entry| match entry {
            Entry::Free { .. } => true,
            Entry::Occupied { entry } => entry.entry.downcast_ref::<Tombstone>().is_some(),
        })
    }

    /// Returns the maximum capacity of this table, in elements, before adding
    /// any more will be refused.
    pub fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    /// Configures the maximum number of entries that may be present within this
    /// table.
    ///
    /// Note that this does not retroactively shrink the table nor evict
    /// existing entries should the maximum be smaller than the current size of
    /// the entry table.
    pub fn set_max_capacity(&mut self, max: usize) {
        self.max_capacity = max;
    }

    /// Create an empty table with at least the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        ResourceTable {
            entries: Vec::with_capacity(capacity),
            free_head: None,
            max_capacity: DEFAULT_MAX_CAPACITY,
            current_host_heap_usage: 0,
            max_host_heap_usage: None,
        }
    }

    /// Set the maximum allowed total host heap usage (in bytes) for all resources in this table.
    ///
    /// Once set, any call to [`push`](ResourceTable::push) or
    /// [`push_child`](ResourceTable::push_child) or
    /// [`update_resource`](ResourceTable::update_resource) that would cause the total to exceed
    /// this limit will return [`ResourceTableError::HostMemoryLimitExceeded`].
    ///
    /// Pass `None` to remove any previously set limit.
    pub fn set_max_host_heap_usage(&mut self, max: Option<usize>) {
        self.max_host_heap_usage = max;
    }

    /// Returns the current total host heap usage (in bytes) of all resources currently held in
    /// this table.
    pub fn current_host_heap_usage(&self) -> usize {
        self.current_host_heap_usage
    }

    /// Manually record that `size` additional bytes of host heap are being managed outside the
    /// table but should count towards this table's limit.
    ///
    /// Returns [`ResourceTableError::HostMemoryLimitExceeded`] if adding `size` would exceed the
    /// configured maximum.
    pub fn retain_host_heap_usage(&mut self, size: usize) -> Result<(), ResourceTableError> {
        self.check_and_add_usage(size)
    }

    /// Undo a previous call to [`retain_host_heap_usage`](ResourceTable::retain_host_heap_usage)
    /// (or any manual addition), releasing `size` bytes from the tracked total.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `size` exceeds the current tracked usage (which would indicate a
    /// bug in the caller's accounting).
    pub fn release_host_heap_usage(&mut self, size: usize) {
        debug_assert!(
            self.current_host_heap_usage >= size,
            "release_host_heap_usage: size {} exceeds current usage {}",
            size,
            self.current_host_heap_usage
        );
        self.current_host_heap_usage = self.current_host_heap_usage.saturating_sub(size);
    }

    /// Check whether adding `extra` bytes to the current usage would exceed the limit, and if not,
    /// add it.
    fn check_and_add_usage(&mut self, extra: usize) -> Result<(), ResourceTableError> {
        if let Some(max) = self.max_host_heap_usage {
            let new_usage = self.current_host_heap_usage.saturating_add(extra);
            if new_usage > max {
                return Err(ResourceTableError::HostMemoryLimitExceeded);
            }
        }
        self.current_host_heap_usage = self.current_host_heap_usage.saturating_add(extra);
        Ok(())
    }

    /// Adjust the table's running total to account for a resource whose cached
    /// size has changed from `old_usage` to `new_usage`.
    ///
    /// When `new_usage > old_usage` the growth is checked against the
    /// configured maximum before being applied; if the limit would be exceeded
    /// the counter is still updated (so subsequent calls see accurate state) and
    /// [`ResourceTableError::HostMemoryLimitExceeded`] is returned.  Shrinks
    /// are always applied unconditionally.
    fn update_usage(
        &mut self,
        old_usage: usize,
        new_usage: usize,
    ) -> Result<(), ResourceTableError> {
        if new_usage >= old_usage {
            let delta = new_usage - old_usage;
            if let Some(max) = self.max_host_heap_usage {
                let projected = self.current_host_heap_usage.saturating_add(delta);
                if projected > max {
                    self.current_host_heap_usage = projected;
                    return Err(ResourceTableError::HostMemoryLimitExceeded);
                }
            }
            self.current_host_heap_usage = self.current_host_heap_usage.saturating_add(delta);
        } else {
            let delta = old_usage - new_usage;
            self.current_host_heap_usage = self.current_host_heap_usage.saturating_sub(delta);
        }
        Ok(())
    }

    /// Inserts a new value `T` into this table, returning a corresponding
    /// `Resource<T>` which can be used to refer to it after it was inserted.
    pub fn push<T>(&mut self, entry: T) -> Result<Resource<T>, ResourceTableError>
    where
        T: Send + 'static + HostHeapUsage,
    {
        let usage = entry.host_heap_usage();
        self.check_and_add_usage(usage)?;
        match self.push_(TableEntry::new(Box::new(entry), None)) {
            Ok(idx) => Ok(Resource::new_own(idx)),
            Err(e) => {
                // Roll back the usage we speculatively added above.
                self.current_host_heap_usage = self.current_host_heap_usage.saturating_sub(usage);
                Err(e)
            }
        }
    }

    /// Pop an index off of the free list, if it's not empty.
    fn pop_free_list(&mut self) -> Option<usize> {
        if let Some(ix) = self.free_head {
            // Advance free_head to the next entry if one is available.
            match &self.entries[ix] {
                Entry::Free { next } => self.free_head = *next,
                Entry::Occupied { .. } => unreachable!(),
            }
            Some(ix)
        } else {
            None
        }
    }

    /// Free an entry in the table, returning its [`TableEntry`]. Add the index to the free list.
    fn free_entry(&mut self, ix: usize, debug: bool) -> TableEntry {
        if debug {
            // Instead of making this entry available for reuse, we leave a
            // tombstone in debug mode.  This helps detect use-after-delete and
            // double-delete bugs.
            match mem::replace(
                &mut self.entries[ix],
                Entry::Occupied {
                    entry: TableEntry {
                        entry: Box::new(Tombstone),
                        parent: None,
                        children: BTreeSet::new(),
                    },
                },
            ) {
                Entry::Occupied { entry } => entry,
                Entry::Free { .. } => unreachable!(),
            }
        } else {
            let entry = match core::mem::replace(
                &mut self.entries[ix],
                Entry::Free {
                    next: self.free_head,
                },
            ) {
                Entry::Occupied { entry } => entry,
                Entry::Free { .. } => unreachable!(),
            };

            self.free_head = Some(ix);

            entry
        }
    }

    /// Push a new entry into the table, returning its handle. This will prefer to use free entries
    /// if they exist, falling back on pushing new entries onto the end of the table.
    fn push_(&mut self, e: TableEntry) -> Result<u32, ResourceTableError> {
        if let Some(free) = self.pop_free_list() {
            self.entries[free] = Entry::Occupied { entry: e };
            Ok(free.try_into().unwrap())
        } else {
            if self.entries.len() >= self.max_capacity {
                return Err(ResourceTableError::Full);
            }
            let ix = self
                .entries
                .len()
                .try_into()
                .map_err(|_| ResourceTableError::Full)?;
            self.entries.push(Entry::Occupied { entry: e });
            Ok(ix)
        }
    }

    fn occupied(&self, key: u32) -> Result<&TableEntry, ResourceTableError> {
        self.entries
            .get(key as usize)
            .and_then(Entry::occupied)
            .ok_or(ResourceTableError::NotPresent)
    }

    fn occupied_mut(&mut self, key: u32) -> Result<&mut TableEntry, ResourceTableError> {
        self.entries
            .get_mut(key as usize)
            .and_then(Entry::occupied_mut)
            .ok_or(ResourceTableError::NotPresent)
    }

    /// Insert a resource at the next available index, and track that it has a
    /// parent resource.
    ///
    /// The parent must exist to create a child. All children resources must
    /// be destroyed before a parent can be destroyed - otherwise
    /// [`ResourceTable::delete`] will fail with
    /// [`ResourceTableError::HasChildren`].
    ///
    /// Parent-child relationships are tracked inside the table to ensure that
    /// a parent resource is not deleted while it has live children. This
    /// allows child resources to hold "references" to a parent by table
    /// index, to avoid needing e.g. an `Arc<Mutex<parent>>` and the associated
    /// locking overhead and design issues, such as child existence extending
    /// lifetime of parent referent even after parent resource is destroyed,
    /// possibility for deadlocks.
    pub fn push_child<T, U>(
        &mut self,
        entry: T,
        parent: &Resource<U>,
    ) -> Result<Resource<T>, ResourceTableError>
    where
        T: Send + 'static + HostHeapUsage,
        U: 'static,
    {
        let usage = entry.host_heap_usage();
        self.check_and_add_usage(usage)?;
        let parent = parent.rep();
        self.occupied(parent)?;
        match self.push_(TableEntry::new(Box::new(entry), Some(parent))) {
            Ok(child) => {
                self.occupied_mut(parent)?.add_child(child);
                Ok(Resource::new_own(child))
            }
            Err(e) => {
                // Roll back the usage we speculatively added above.
                self.current_host_heap_usage = self.current_host_heap_usage.saturating_sub(usage);
                Err(e)
            }
        }
    }

    /// Add an already-resident child to a resource.
    pub fn add_child<T: 'static, U: 'static>(
        &mut self,
        child: Resource<T>,
        parent: Resource<U>,
    ) -> Result<(), ResourceTableError> {
        let entry = self.occupied_mut(child.rep())?;
        assert!(entry.parent.is_none());
        entry.parent = Some(parent.rep());
        self.occupied_mut(parent.rep())?.add_child(child.rep());
        Ok(())
    }

    /// Remove a child to from a resource (but leave it in the table).
    pub fn remove_child<T: 'static, U: 'static>(
        &mut self,
        child: Resource<T>,
        parent: Resource<U>,
    ) -> Result<(), ResourceTableError> {
        let entry = self.occupied_mut(child.rep())?;
        assert_eq!(entry.parent, Some(parent.rep()));
        entry.parent = None;
        self.occupied_mut(parent.rep())?.remove_child(child.rep());
        Ok(())
    }

    /// Get an immutable reference to a resource of a given type at a given
    /// index.
    ///
    /// Multiple shared references can be borrowed at any given time.
    pub fn get<T: Any + Sized>(&self, key: &Resource<T>) -> Result<&T, ResourceTableError> {
        self.get_(key.rep())?
            .downcast_ref()
            .ok_or(ResourceTableError::WrongType)
    }

    fn get_(&self, key: u32) -> Result<&dyn Any, ResourceTableError> {
        let r = self.occupied(key)?;
        Ok(&*r.entry)
    }

    /// Get a mutable reference to a resource of a given type at a given index.
    ///
    /// This method is only available for types that implement
    /// [`FixedHostHeapUsage`], which guarantees that mutation through `&mut T`
    /// cannot change the value returned by
    /// [`HostHeapUsage::host_heap_usage`]. Because the size is fixed, no
    /// accounting update is needed on mutation and a plain `&mut T` can be
    /// returned safely.
    ///
    /// For types whose heap footprint can vary with mutation (types that only
    /// implement [`HostHeapUsage`] but not [`FixedHostHeapUsage`]), use
    /// [`borrow_mut`](ResourceTable::borrow_mut) or
    /// [`update_resource`](ResourceTable::update_resource) instead.
    pub fn get_mut<T: Any + Sized + FixedHostHeapUsage>(
        &mut self,
        key: &Resource<T>,
    ) -> Result<&mut T, ResourceTableError> {
        self.get_any_mut(key.rep())?
            .downcast_mut()
            .ok_or(ResourceTableError::WrongType)
    }

    /// Get a tracked mutable borrow of a variable-size resource.
    ///
    /// Returns a [`ResourceBorrow<T>`] guard that derefs to `&mut T`. When the
    /// mutation is complete, call [`ResourceBorrow::finish`] to update the
    /// table's heap usage accounting. `finish()` returns `Err` if the new size
    /// exceeds the configured limit.
    ///
    /// If the guard is dropped without calling `finish()`, the accounting is
    /// still updated (as a safety net), but any limit-exceeded error is
    /// silently ignored.
    ///
    /// For types that implement [`FixedHostHeapUsage`] (where mutation cannot
    /// change the heap footprint), prefer the simpler
    /// [`get_mut`](ResourceTable::get_mut) which returns `&mut T` directly.
    pub fn borrow_mut<T: Any + Sized + HostHeapUsage>(
        &mut self,
        key: &Resource<T>,
    ) -> Result<ResourceBorrow<'_, T>, ResourceTableError> {
        let rep = key.rep();
        let entry = self.occupied_mut(rep)?;
        let value = entry
            .entry
            .downcast_mut::<T>()
            .ok_or(ResourceTableError::WrongType)?;
        let usage_before = value.host_heap_usage();
        // SAFETY: we extend the lifetime of `value` from the entry borrow to
        // `'_` (the lifetime of `&mut self`). This is valid because the guard
        // holds exclusive access to the table for its lifetime — no other
        // access to the table or this entry is possible while the guard lives.
        let value: &mut T = unsafe { &mut *(value as *mut T) };
        Ok(ResourceBorrow {
            table: self as *mut ResourceTable,
            usage_before,
            value,
            finished: false,
        })
    }

    /// Returns the raw `Any` at the `key` index provided.
    ///
    /// # Warning
    ///
    /// This method bypasses heap usage tracking entirely. Prefer
    /// [`get_mut`](ResourceTable::get_mut) for [`FixedHostHeapUsage`] types,
    /// [`borrow_mut`](ResourceTable::borrow_mut) for tracked variable-size
    /// borrows, or [`update_resource`](ResourceTable::update_resource) for
    /// closure-based mutation.
    ///
    /// Only use this when working with type-erased entries where the concrete
    /// type is not available at the call site.
    pub fn get_any_mut(&mut self, key: u32) -> Result<&mut dyn Any, ResourceTableError> {
        let r = self.occupied_mut(key)?;
        Ok(&mut *r.entry)
    }

    /// Mutate a variable-size resource in place while keeping the table's host
    /// heap usage accounting up-to-date.
    ///
    /// Use this method for types that implement [`HostHeapUsage`] but **not**
    /// [`FixedHostHeapUsage`] — i.e. types whose heap footprint can change
    /// through mutation (those with owned `Vec`, `String`, `HashMap`, etc.).
    ///
    /// The `updater` closure receives a `&mut T`, may modify it freely, and
    /// may return any value `R`.  After the closure returns, the resource's new
    /// heap usage is computed via [`HostHeapUsage::host_heap_usage`] and the
    /// table's running total is adjusted.  The closure's return value is
    /// propagated as the `Ok` half of the result, making it easy to extract a
    /// value from the resource in a single step:
    ///
    /// ```ignore
    /// let taken = table.update_resource(&id, |r| r.field.take())?;
    /// // `taken` is the value that was in `r.field`; the table is already updated.
    /// ```
    ///
    /// If the new usage would exceed the configured maximum, the mutation is
    /// still applied (not rolled back), but `Err` is returned so the caller
    /// can react.
    ///
    /// For types whose size can never change through mutation, use the simpler
    /// [`get_mut`](ResourceTable::get_mut) instead.
    ///
    /// # Errors
    ///
    /// Returns [`ResourceTableError::NotPresent`] if the resource is not in the
    /// table, [`ResourceTableError::WrongType`] if the type does not match, and
    /// [`ResourceTableError::HostMemoryLimitExceeded`] if the post-mutation
    /// usage exceeds the configured limit (if any).
    pub fn update_resource<T, F, R>(
        &mut self,
        resource: &Resource<T>,
        updater: F,
    ) -> Result<R, ResourceTableError>
    where
        T: Any + Sized + HostHeapUsage,
        F: FnOnce(&mut T) -> R,
    {
        let key = resource.rep();
        let entry = self.occupied_mut(key)?;
        let t = entry
            .entry
            .downcast_mut::<T>()
            .ok_or(ResourceTableError::WrongType)?;
        let old_usage = t.host_heap_usage();
        let result = updater(t);
        let new_usage = t.host_heap_usage();

        self.update_usage(old_usage, new_usage)?;
        Ok(result)
    }

    /// Remove the specified entry from the table.
    pub fn delete<T>(&mut self, resource: Resource<T>) -> Result<T, ResourceTableError>
    where
        T: Any + HostHeapUsage,
    {
        self.delete_maybe_debug(resource, DELETE_WITH_TOMBSTONE)
    }

    fn delete_maybe_debug<T>(
        &mut self,
        resource: Resource<T>,
        debug: bool,
    ) -> Result<T, ResourceTableError>
    where
        T: Any + HostHeapUsage,
    {
        debug_assert!(resource.owned());
        let entry = self.delete_entry(resource.rep(), debug)?;
        // Sample the usage from the value before consuming the entry.
        let usage = entry
            .entry
            .downcast_ref::<T>()
            .map_or(0, |t| t.host_heap_usage());
        let _ = self.update_usage(usage, 0);
        match entry.entry.downcast() {
            Ok(t) => Ok(*t),
            Err(_e) => Err(ResourceTableError::WrongType),
        }
    }

    fn delete_entry(&mut self, key: u32, debug: bool) -> Result<TableEntry, ResourceTableError> {
        if !self.occupied(key)?.children.is_empty() {
            return Err(ResourceTableError::HasChildren);
        }
        let e = self.free_entry(key as usize, debug);
        if let Some(parent) = e.parent {
            // Remove deleted resource from parent's child list.
            // Parent must still be present because it can't be deleted while still having
            // children:
            self.occupied_mut(parent)
                .expect("missing parent")
                .remove_child(key);
        }
        Ok(e)
    }

    /// Zip the values of the map with mutable references to table entries corresponding to each
    /// key. As the keys in the `BTreeMap` are unique, this iterator can give mutable references
    /// with the same lifetime as the mutable reference to the [ResourceTable].
    pub fn iter_entries<'a, T>(
        &'a mut self,
        map: BTreeMap<u32, T>,
    ) -> impl Iterator<Item = (Result<&'a mut dyn Any, ResourceTableError>, T)> {
        map.into_iter().map(move |(k, v)| {
            let item = self
                .occupied_mut(k)
                .map(|e| Box::as_mut(&mut e.entry))
                // Safety: extending the lifetime of the mutable reference.
                .map(|item| unsafe { &mut *(item as *mut dyn Any) });
            (item, v)
        })
    }

    /// Iterate over all children belonging to the provided parent
    pub fn iter_children<T>(
        &self,
        parent: &Resource<T>,
    ) -> Result<impl Iterator<Item = &(dyn Any + Send)> + use<'_, T>, ResourceTableError>
    where
        T: 'static,
    {
        let parent_entry = self.occupied(parent.rep())?;
        Ok(parent_entry.children.iter().map(|child_index| {
            let child = self.occupied(*child_index).expect("missing child");
            child.entry.as_ref()
        }))
    }

    /// Iterate over all the entries in this table.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut (dyn Any + Send)> {
        self.entries.iter_mut().filter_map(|entry| match entry {
            Entry::Occupied { entry } => Some(&mut *entry.entry),
            Entry::Free { .. } => None,
        })
    }
}

impl Default for ResourceTable {
    fn default() -> Self {
        ResourceTable::new()
    }
}

impl fmt::Debug for ResourceTable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[")?;
        let mut wrote = false;
        for (index, entry) in self.entries.iter().enumerate() {
            if let Entry::Occupied { entry } = entry {
                if entry.entry.downcast_ref::<Tombstone>().is_none() {
                    if wrote {
                        write!(f, ", ")?;
                    } else {
                        wrote = true;
                    }
                    write!(f, "{index}")?;
                }
            }
        }
        write!(f, "]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_free_list() {
        let mut table = ResourceTable::new();

        let x = table.push(()).unwrap();
        assert_eq!(x.rep(), 0);

        let y = table.push(()).unwrap();
        assert_eq!(y.rep(), 1);

        // Deleting x should put it on the free list, so the next entry should have the same rep.
        table.delete_maybe_debug(x, false).unwrap();
        let x = table.push(()).unwrap();
        assert_eq!(x.rep(), 0);

        // Deleting x and then y should yield indices 1 and then 0 for new entries.
        table.delete_maybe_debug(x, false).unwrap();
        table.delete_maybe_debug(y, false).unwrap();

        let y = table.push(()).unwrap();
        assert_eq!(y.rep(), 1);

        let x = table.push(()).unwrap();
        assert_eq!(x.rep(), 0);

        // As the free list is empty, this entry will have a new id.
        let x = table.push(()).unwrap();
        assert_eq!(x.rep(), 2);
    }

    // ---- heap-tracking tests ----

    /// Variable-size resource: `host_heap_usage` returns a runtime value that
    /// can change through mutation. Uses `update_resource` for mutation.
    struct Tracked(usize);
    impl HostHeapUsage for Tracked {
        fn host_heap_usage(&self) -> usize {
            self.0
        }
    }

    /// Fixed-size resource: `host_heap_usage` is always `size_of::<Fixed>()`.
    /// Implements `FixedHostHeapUsage` so `get_mut` is available.
    struct Fixed(u64);
    impl FixedHostHeapUsage for Fixed {}

    #[test]
    fn test_heap_usage_push_delete() {
        let mut table = ResourceTable::new();
        assert_eq!(table.current_host_heap_usage(), 0);

        let r1 = table.push(Tracked(100)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);

        let r2 = table.push(Tracked(200)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 300);

        table.delete(r1).unwrap();
        assert_eq!(table.current_host_heap_usage(), 200);

        table.delete(r2).unwrap();
        assert_eq!(table.current_host_heap_usage(), 0);
    }

    #[test]
    fn test_heap_usage_limit_enforced() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(250));

        table.push(Tracked(100)).unwrap();
        table.push(Tracked(100)).unwrap();

        // This would take us to 300, exceeding the limit.
        let err = table.push(Tracked(101)).unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));

        // Usage should remain at 200 since the third push was rejected.
        assert_eq!(table.current_host_heap_usage(), 200);
    }

    #[test]
    fn test_heap_usage_limit_exact_boundary() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(200));

        // Exactly at the limit should succeed.
        table.push(Tracked(100)).unwrap();
        table.push(Tracked(100)).unwrap();

        // One byte over should fail.
        let err = table.push(Tracked(1)).unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));
    }

    // ---- get_mut: fixed-size types only ----

    #[test]
    fn test_get_mut_fixed_no_tracking_overhead() {
        // get_mut is available for Fixed and returns &mut T directly.
        // The counter never changes because the size is fixed.
        let size = core::mem::size_of::<Fixed>();
        let mut table = ResourceTable::new();
        let r = table.push(Fixed(1)).unwrap();
        assert_eq!(table.current_host_heap_usage(), size);

        // Mutate through get_mut — counter must remain stable.
        table.get_mut(&r).unwrap().0 = 42;
        assert_eq!(table.current_host_heap_usage(), size);

        table.delete(r).unwrap();
        assert_eq!(table.current_host_heap_usage(), 0);
    }

    // ---- update_resource: variable-size types ----

    #[test]
    fn test_update_resource_grow() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(300));

        let r = table.push(Tracked(100)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);

        // Grow the resource.
        table.update_resource(&r, |t| t.0 = 200).unwrap();
        assert_eq!(table.current_host_heap_usage(), 200);

        // Shrink the resource.
        table.update_resource(&r, |t| t.0 = 50).unwrap();
        assert_eq!(table.current_host_heap_usage(), 50);
    }

    #[test]
    fn test_update_resource_exceeds_limit() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(150));

        let r = table.push(Tracked(100)).unwrap();

        // Growing to 200 exceeds the limit — error is returned immediately.
        let err = table.update_resource(&r, |t| t.0 = 200).unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));

        // Counter still reflects the actual (over-limit) size.
        assert_eq!(table.current_host_heap_usage(), 200);
    }

    #[test]
    fn test_update_resource_delete_after_growth() {
        // After an update_resource growth, delete should correctly subtract the
        // new cached size, returning the counter to zero.
        let mut table = ResourceTable::new();
        let r = table.push(Tracked(100)).unwrap();

        table.update_resource(&r, |t| t.0 = 300).unwrap();
        assert_eq!(table.current_host_heap_usage(), 300);

        table.delete(r).unwrap();
        assert_eq!(table.current_host_heap_usage(), 0);
    }

    #[test]
    fn test_heap_usage_push_child() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(250));

        let parent = table.push(Tracked(100)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);

        let child = table.push_child(Tracked(100), &parent).unwrap();
        assert_eq!(table.current_host_heap_usage(), 200);

        // A child that would exceed the limit is rejected.
        let err = table.push_child(Tracked(51), &parent).unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));
        assert_eq!(table.current_host_heap_usage(), 200);

        table.delete(child).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);
    }

    #[test]
    fn test_retain_and_release_host_heap_usage() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(300));

        // Reserve 200 bytes manually.
        table.retain_host_heap_usage(200).unwrap();
        assert_eq!(table.current_host_heap_usage(), 200);

        // Reserving 101 more should fail (would be 301 > 300).
        let err = table.retain_host_heap_usage(101).unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));

        // Release 100, leaving 100.
        table.release_host_heap_usage(100);
        assert_eq!(table.current_host_heap_usage(), 100);

        // Now reserving 200 should succeed (total = 300).
        table.retain_host_heap_usage(200).unwrap();
        assert_eq!(table.current_host_heap_usage(), 300);
    }

    #[test]
    fn test_no_limit_no_error() {
        let mut table = ResourceTable::new();
        // No limit set — pushing large values should always succeed.
        for _ in 0..100 {
            table.push(Tracked(1_000_000)).unwrap();
        }
        assert_eq!(table.current_host_heap_usage(), 100_000_000);
    }

    #[test]
    fn test_remove_limit() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(100));

        table.push(Tracked(100)).unwrap();
        assert!(table.push(Tracked(1)).is_err());

        // Remove the limit — now the same push should succeed.
        table.set_max_host_heap_usage(None);
        table.push(Tracked(1)).unwrap();
    }

    // ---- borrow_mut / ResourceBorrow tests ----

    #[test]
    fn test_borrow_mut_finish_updates_counter() {
        let mut table = ResourceTable::new();
        let r = table.push(Tracked(100)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);

        {
            let mut guard = table.borrow_mut(&r).unwrap();
            guard.0 = 250;
            guard.finish().unwrap();
        }
        assert_eq!(table.current_host_heap_usage(), 250);

        table.delete(r).unwrap();
        assert_eq!(table.current_host_heap_usage(), 0);
    }

    #[test]
    fn test_borrow_mut_finish_returns_error_on_over_limit() {
        let mut table = ResourceTable::new();
        table.set_max_host_heap_usage(Some(150));

        let r = table.push(Tracked(100)).unwrap();

        let mut guard = table.borrow_mut(&r).unwrap();
        guard.0 = 200;
        let err = guard.finish().unwrap_err();
        assert!(matches!(err, ResourceTableError::HostMemoryLimitExceeded));

        // Counter is updated to reflect reality even on error.
        assert_eq!(table.current_host_heap_usage(), 200);
    }

    #[test]
    fn test_borrow_mut_drop_without_finish_still_updates() {
        let mut table = ResourceTable::new();
        let r = table.push(Tracked(100)).unwrap();

        {
            let mut guard = table.borrow_mut(&r).unwrap();
            guard.0 = 300;
            // drop without calling finish()
        }
        // Safety-net drop should have updated the counter.
        assert_eq!(table.current_host_heap_usage(), 300);
    }

    #[test]
    fn test_borrow_mut_no_mutation_is_noop() {
        let mut table = ResourceTable::new();
        let r = table.push(Tracked(100)).unwrap();

        {
            let guard = table.borrow_mut(&r).unwrap();
            guard.finish().unwrap();
        }
        assert_eq!(table.current_host_heap_usage(), 100);
    }

    #[test]
    fn test_borrow_mut_shrink() {
        let mut table = ResourceTable::new();
        let r = table.push(Tracked(200)).unwrap();

        {
            let mut guard = table.borrow_mut(&r).unwrap();
            guard.0 = 50;
            guard.finish().unwrap();
        }
        assert_eq!(table.current_host_heap_usage(), 50);
    }

    #[test]
    fn test_heap_usage_not_leaked_on_push_full() {
        // When push() fails because the table is full, the speculatively-added
        // usage must be rolled back so the counter stays accurate.
        let mut table = ResourceTable::new();
        table.set_max_capacity(1);
        table.set_max_host_heap_usage(Some(1000));

        let r = table.push(Tracked(100)).unwrap();
        assert_eq!(table.current_host_heap_usage(), 100);

        // The table is now full (capacity 1, one entry); this push should fail
        // with Full, not HostMemoryLimitExceeded.
        let err = table.push(Tracked(100)).unwrap_err();
        assert!(matches!(err, ResourceTableError::Full));

        // Counter must be unchanged — no leak.
        assert_eq!(table.current_host_heap_usage(), 100);

        table.delete(r).unwrap();
        assert_eq!(table.current_host_heap_usage(), 0);
    }
}

#[test]
fn test_max_capacity() {
    let mut table = ResourceTable::new();
    assert_eq!(table.max_capacity(), DEFAULT_MAX_CAPACITY);

    table.set_max_capacity(0);
    assert_eq!(table.max_capacity(), 0);
    assert!(table.push(()).is_err());

    table.set_max_capacity(1);
    assert_eq!(table.max_capacity(), 1);
    let x = table.push(()).unwrap();
    assert!(table.push(()).is_err());

    table.set_max_capacity(0);
    assert!(table.push(()).is_err());
    table.delete(x).unwrap();
    let x = table.push(()).unwrap();
    table.delete(x).unwrap();

    table.set_max_capacity(10);

    let handles = (0..10).map(|_| table.push(()).unwrap()).collect::<Vec<_>>();
    assert!(table.push(()).is_err());
    for handle in handles {
        table.delete(handle).unwrap();
    }

    table.push(()).unwrap();
}
