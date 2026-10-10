use super::resources::HostHeapUsage;
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::hash::Hash;
use core::ops::Deref;
use std::collections::{HashMap, HashSet};

/// A [`Vec`] wrapper that maintains a running tally of its aggregate host heap
/// usage in constant time.
///
/// Standard [`Vec`] traversal scales linearly with the number of elements when
/// querying child heap usage. For resources stored in a [`ResourceTable`](super::ResourceTable)
/// that undergo frequent mutations via [`ResourceTable::update_resource`](super::ResourceTable::update_resource),
/// `TrackedVec` maintains the collection's child heap footprint incrementally on
/// every push and pop, avoiding re-scanning elements on every mutation.
#[derive(Debug, Clone)]
pub struct TrackedVec<T> {
    inner: Vec<T>,
    heap_size: usize,
}

impl<T> Default for TrackedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TrackedVec<T> {
    /// Creates a new, empty `TrackedVec`.
    pub fn new() -> Self {
        Self {
            inner: Vec::new(),
            heap_size: 0,
        }
    }

    /// Creates a new `TrackedVec` with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let heap_size = capacity * core::mem::size_of::<T>();
        Self {
            inner: Vec::with_capacity(capacity),
            heap_size,
        }
    }

    /// Returns the total number of elements the vector can hold without reallocating.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Appends an element to the back of the collection, updating the tracked
    /// heap size in constant time.
    pub fn push(&mut self, value: T)
    where
        T: HostHeapUsage,
    {
        let old_cap = self.inner.capacity();
        let elem_size = value.heap_size_of_children();
        self.inner.push(value);
        self.heap_size += elem_size;
        let new_cap = self.inner.capacity();
        if new_cap > old_cap {
            self.heap_size += (new_cap - old_cap) * core::mem::size_of::<T>();
        }
    }

    /// Removes the last element from the vector and returns it, updating the
    /// tracked heap size in constant time.
    pub fn pop(&mut self) -> Option<T>
    where
        T: HostHeapUsage,
    {
        let val = self.inner.pop()?;
        self.heap_size = self.heap_size.saturating_sub(val.heap_size_of_children());
        Some(val)
    }

    /// Clears the vector, removing all values while retaining capacity.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.heap_size = self.inner.capacity() * core::mem::size_of::<T>();
    }

    /// Extracts the wrapped [`Vec`].
    pub fn into_inner(self) -> Vec<T> {
        self.inner
    }

    /// Returns a reference to the underlying [`Vec`].
    pub fn as_inner(&self) -> &Vec<T> {
        &self.inner
    }
}

impl<T> Deref for TrackedVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.inner
    }
}

impl<T> HostHeapUsage for TrackedVec<T> {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        self.heap_size
    }
}

impl<T: HostHeapUsage> FromIterator<T> for TrackedVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut vec = Self::new();
        for item in iter {
            vec.push(item);
        }
        vec
    }
}

/// A [`HashSet`] wrapper that maintains a running tally of its aggregate host
/// heap usage in constant time.
///
/// Querying heap usage on a standard [`HashSet`] requires a linear traversal over
/// all elements. For resources stored in a [`ResourceTable`](super::ResourceTable)
/// that undergo frequent mutations via [`ResourceTable::update_resource`](super::ResourceTable::update_resource),
/// `TrackedHashSet` maintains the collection's child heap footprint incrementally
/// on every insertion and removal, ensuring that all table usage accounting runs
/// in constant time.
#[derive(Debug, Clone)]
pub struct TrackedHashSet<T> {
    inner: HashSet<T>,
    heap_size: usize,
}

impl<T> Default for TrackedHashSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TrackedHashSet<T> {
    /// Creates a new, empty `TrackedHashSet`.
    pub fn new() -> Self {
        Self {
            inner: HashSet::new(),
            heap_size: 0,
        }
    }

    /// Creates a new `TrackedHashSet` with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let heap_size = if capacity > 0 {
            capacity * (core::mem::size_of::<T>() + 1)
        } else {
            0
        };
        Self {
            inner: HashSet::with_capacity(capacity),
            heap_size,
        }
    }

    /// Adds a value to the set.
    ///
    /// Returns whether the value was newly inserted, updating the tracked
    /// heap size in constant time.
    pub fn insert(&mut self, value: T) -> bool
    where
        T: Eq + Hash + HostHeapUsage,
    {
        let old_cap = self.inner.capacity();
        let elem_size = value.heap_size_of_children();
        if self.inner.insert(value) {
            self.heap_size += elem_size;
            let new_cap = self.inner.capacity();
            if new_cap > old_cap {
                self.heap_size += (new_cap - old_cap) * (core::mem::size_of::<T>() + 1);
            }
            true
        } else {
            false
        }
    }

    /// Removes a value from the set, updating the tracked heap size in constant time.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q> + Eq + Hash + HostHeapUsage,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(removed) = self.inner.take(value) {
            self.heap_size = self
                .heap_size
                .saturating_sub(removed.heap_size_of_children());
            true
        } else {
            false
        }
    }

    /// Clears the set, removing all values while retaining capacity.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.heap_size = if self.inner.capacity() > 0 {
            self.inner.capacity() * (core::mem::size_of::<T>() + 1)
        } else {
            0
        };
    }

    /// Extracts the wrapped [`HashSet`].
    pub fn into_inner(self) -> HashSet<T> {
        self.inner
    }

    /// Returns a reference to the underlying [`HashSet`].
    pub fn as_inner(&self) -> &HashSet<T> {
        &self.inner
    }
}

impl<T> Deref for TrackedHashSet<T> {
    type Target = HashSet<T>;

    fn deref(&self) -> &HashSet<T> {
        &self.inner
    }
}

impl<T> HostHeapUsage for TrackedHashSet<T> {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        self.heap_size
    }
}

impl<T: Eq + Hash + HostHeapUsage> FromIterator<T> for TrackedHashSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut set = Self::new();
        for item in iter {
            set.insert(item);
        }
        set
    }
}

/// A [`HashMap`] wrapper that maintains a running tally of its aggregate host
/// heap usage in constant time.
///
/// Querying heap usage on a standard [`HashMap`] requires a linear traversal over
/// all keys and values. For resources stored in a [`ResourceTable`](super::ResourceTable)
/// that undergo frequent mutations via [`ResourceTable::update_resource`](super::ResourceTable::update_resource),
/// `TrackedHashMap` maintains the collection's child heap footprint incrementally
/// on every insertion and removal, ensuring that all table usage accounting runs
/// in constant time.
#[derive(Debug, Clone)]
pub struct TrackedHashMap<K, V> {
    inner: HashMap<K, V>,
    heap_size: usize,
}

impl<K, V> Default for TrackedHashMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> TrackedHashMap<K, V> {
    /// Creates a new, empty `TrackedHashMap`.
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
            heap_size: 0,
        }
    }

    /// Creates a new `TrackedHashMap` with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let heap_size = if capacity > 0 {
            capacity * (core::mem::size_of::<K>() + core::mem::size_of::<V>() + 1)
        } else {
            0
        };
        Self {
            inner: HashMap::with_capacity(capacity),
            heap_size,
        }
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not have this key present, `None` is returned.
    /// If the map did have this key present, the value is updated, and the old
    /// value is returned. The tracked heap size is updated in constant time.
    pub fn insert(&mut self, key: K, value: V) -> Option<V>
    where
        K: Eq + Hash + HostHeapUsage,
        V: HostHeapUsage,
    {
        let old_cap = self.inner.capacity();
        let key_size = key.heap_size_of_children();
        let val_size = value.heap_size_of_children();
        let prev = self.inner.insert(key, value);
        if let Some(ref old_val) = prev {
            self.heap_size = self
                .heap_size
                .saturating_sub(old_val.heap_size_of_children());
            self.heap_size += val_size;
        } else {
            self.heap_size += key_size + val_size;
        }
        let new_cap = self.inner.capacity();
        if new_cap > old_cap {
            self.heap_size +=
                (new_cap - old_cap) * (core::mem::size_of::<K>() + core::mem::size_of::<V>() + 1);
        }
        prev
    }

    /// Removes a key from the map, returning the value at the key if the key was previously in the map.
    /// The tracked heap size is updated in constant time.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q> + Eq + Hash + HostHeapUsage,
        Q: Hash + Eq + ?Sized,
        V: HostHeapUsage,
    {
        if let Some((k, v)) = self.inner.remove_entry(key) {
            let freed = k.heap_size_of_children() + v.heap_size_of_children();
            self.heap_size = self.heap_size.saturating_sub(freed);
            Some(v)
        } else {
            None
        }
    }

    /// Removes a key from the map, returning the stored key and value if the key was previously in the map.
    /// The tracked heap size is updated in constant time.
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q> + Eq + Hash + HostHeapUsage,
        Q: Hash + Eq + ?Sized,
        V: HostHeapUsage,
    {
        let entry = self.inner.remove_entry(key);
        if let Some((ref k, ref v)) = entry {
            let freed = k.heap_size_of_children() + v.heap_size_of_children();
            self.heap_size = self.heap_size.saturating_sub(freed);
        }
        entry
    }

    /// Clears the map, removing all key-value pairs while retaining capacity.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.heap_size = if self.inner.capacity() > 0 {
            self.inner.capacity() * (core::mem::size_of::<K>() + core::mem::size_of::<V>() + 1)
        } else {
            0
        };
    }

    /// Extracts the wrapped [`HashMap`].
    pub fn into_inner(self) -> HashMap<K, V> {
        self.inner
    }

    /// Returns a reference to the underlying [`HashMap`].
    pub fn as_inner(&self) -> &HashMap<K, V> {
        &self.inner
    }
}

impl<K, V> Deref for TrackedHashMap<K, V> {
    type Target = HashMap<K, V>;

    fn deref(&self) -> &HashMap<K, V> {
        &self.inner
    }
}

impl<K, V> HostHeapUsage for TrackedHashMap<K, V> {
    #[inline]
    fn heap_size_of_children(&self) -> usize {
        self.heap_size
    }
}

impl<K, V> FromIterator<(K, V)> for TrackedHashMap<K, V>
where
    K: Eq + Hash + HostHeapUsage,
    V: HostHeapUsage,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};

    #[test]
    fn test_tracked_vec() {
        let mut vec: TrackedVec<String> = TrackedVec::new();
        assert_eq!(vec.heap_size_of_children(), 0);

        vec.push("hello".to_string());
        let size1 = vec.heap_size_of_children();
        assert!(size1 > 0);

        vec.push("world! longer string".to_string());
        let size2 = vec.heap_size_of_children();
        assert!(size2 > size1);

        let popped = vec.pop().unwrap();
        assert_eq!(popped, "world! longer string");
        assert!(vec.heap_size_of_children() < size2);

        vec.clear();
        assert!(vec.heap_size_of_children() >= vec.capacity() * core::mem::size_of::<String>());
    }

    #[test]
    fn test_tracked_hash_map() {
        let mut map: TrackedHashMap<String, Vec<u8>> = TrackedHashMap::new();
        assert_eq!(map.heap_size_of_children(), 0);

        map.insert("key1".to_string(), vec![1, 2, 3, 4, 5]);
        let size1 = map.heap_size_of_children();
        assert!(size1 > 0);

        map.insert("key2".to_string(), vec![6; 100]);
        let size2 = map.heap_size_of_children();
        assert!(size2 > size1);

        // Replacing an existing key updates usage correctly
        map.insert("key2".to_string(), vec![7; 10]);
        let size3 = map.heap_size_of_children();
        assert!(size3 < size2);

        assert!(map.remove("key1").is_some());
        assert!(map.heap_size_of_children() < size3);

        map.clear();
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_tracked_hash_set() {
        let mut set: TrackedHashSet<String> = TrackedHashSet::new();
        assert_eq!(set.heap_size_of_children(), 0);

        assert!(set.insert("item1".to_string()));
        let size1 = set.heap_size_of_children();
        assert!(size1 > 0);

        // Duplicate insert does not change size
        assert!(!set.insert("item1".to_string()));
        assert_eq!(set.heap_size_of_children(), size1);

        assert!(set.insert("item2_much_longer_string".to_string()));
        assert!(set.heap_size_of_children() > size1);

        assert!(set.remove("item1"));
        assert!(!set.remove("item1"));
    }
}
