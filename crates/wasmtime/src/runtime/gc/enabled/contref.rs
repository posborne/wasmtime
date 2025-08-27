use crate::runtime::gc::{GcRefImpl, GcRootIndex};
use crate::{store::StoreOpaque, HeapType, store::AutoAssertNoGc, Rooted, ContType, AsContextMut};
use anyhow::Result;
use core::mem;

/// A `contref` GC reference.
///
/// The `ContRef` type represents WebAssembly `contref`
/// values. These are references to continuation objects used
/// for stack switching operations.
#[derive(Debug)]
#[repr(transparent)]
pub struct ContRef {
    pub(super) inner: GcRootIndex,
}

unsafe impl GcRefImpl for ContRef {
    fn transmute_ref(index: &GcRootIndex) -> &Self {
        // Safety: `ContRef` is a newtype of a `GcRootIndex`.
        let me: &Self = unsafe { mem::transmute(index) };

        // Assert we really are just a newtype of a `GcRootIndex`.
        assert!(matches!(
            me,
            Self {
                inner: GcRootIndex { .. },
            }
        ));

        me
    }
}

impl ContRef {

    pub(crate) fn _from_raw(_store: &mut AutoAssertNoGc, raw: u32) -> Option<Rooted<Self>> {
        // For now, return None to handle null references
        if raw == 0 {
            None
        } else {
            // TODO: Implement proper continuation reference handling
            // This is a stub to get compilation working
            todo!()
        }
    }

    /// Converts this [`ContRef`] to a raw value suitable to store within a
    /// [`ValRaw`].
    ///
    /// Returns an error if this `contref` has been unrooted.
    ///
    /// # Correctness
    ///
    /// Produces a raw value which is only valid to pass into a store if a GC
    /// doesn't happen between when the value is produce and when it's passed
    /// into the store.
    ///
    /// [`ValRaw`]: crate::ValRaw
    pub fn to_raw(&self, mut store: impl AsContextMut) -> Result<u32> {
        let mut store = AutoAssertNoGc::new(store.as_context_mut().0);
        self._to_raw(&mut store)
    }

    pub(crate) fn _to_raw(&self, _store: &mut AutoAssertNoGc<'_>) -> Result<u32> {
        // TODO: Implement proper continuation reference serialization
        // This is a stub to get compilation working
        Ok(0)
    }

    pub(crate) fn _ty(&self, _store: &StoreOpaque) -> Result<ContType> {
        // TODO: Implement proper type handling for continuations
        // For now return a stub continuation type
        todo!()
    }


    pub(crate) fn _matches_ty(&self, _store: &StoreOpaque, _ty: &HeapType) -> Result<bool> {
        // TODO: Implement proper type matching
        // For now just check if it's a continuation type
        Ok(matches!(_ty, HeapType::Cont | HeapType::ConcreteCont(_)))
    }

    #[inline]
    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        self.inner.comes_from_same_store(store)
    }

    pub(crate) fn try_gc_ref<'a>(&self, store: &'a AutoAssertNoGc<'a>) -> Result<&'a crate::runtime::vm::VMGcRef> {
        self.inner.try_gc_ref(store)
    }

    pub(crate) fn try_clone_gc_ref(&self, store: &mut AutoAssertNoGc) -> Result<crate::runtime::vm::VMGcRef> {
        self.inner.try_clone_gc_ref(store)
    }
}
