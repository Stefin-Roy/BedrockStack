use core::ops::{Deref, DerefMut};
use spin::Mutex;

/// Lock-order key for `IrqMutex` instances under the `lockdep` feature.
///
/// Class 0 means "untracked" (leaf locks and all pre-lockdep users); such
/// locks never participate in ordering checks but still push/pop nothing.
/// Tracked locks must be acquired in strictly ascending class order on any
/// one CPU: acquiring class C while holding any class >= C panics with a
/// violation report. Equal classes are therefore never nestable — which is
/// exactly the invariant the scheduler's prose rules describe (e.g. the park
/// lists are never held together).
pub const LOCKDEP_CLASS_NONE: u32 = 0;

pub struct IrqMutex<T> {
    inner: Mutex<T>,
    #[cfg(feature = "lockdep")]
    class: u32,
}

impl<T> IrqMutex<T> {
    pub const fn new(val: T) -> Self {
        #[cfg(not(feature = "lockdep"))]
        return IrqMutex { inner: Mutex::new(val) };
        #[cfg(feature = "lockdep")]
        return IrqMutex { inner: Mutex::new(val), class: LOCKDEP_CLASS_NONE };
    }

    /// Like `new`, but tags the lock with a lockdep order class. The class is
    /// ignored (and takes no space) when the `lockdep` feature is disabled,
    /// so call sites can pass keys unconditionally.
    pub const fn new_keyed(class: u32, val: T) -> Self {
        #[cfg(not(feature = "lockdep"))]
        {
            let _ = class;
            return IrqMutex { inner: Mutex::new(val) };
        }
        #[cfg(feature = "lockdep")]
        return IrqMutex { inner: Mutex::new(val), class };
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let was = crate::arch::CurrentArch::are_interrupts_enabled();
        if was {
            crate::arch::CurrentArch::disable_interrupts();
        }
        // IRQs are now off, so this CPU's lockdep stack has exactly one
        // mutator (no interrupt context can interleave) — see smp::lockdep.
        #[cfg(feature = "lockdep")]
        crate::smp::lockdep::acquire(self.class);
        IrqGuard {
            guard: Some(self.inner.lock()),
            was_enabled: was,
            #[cfg(feature = "lockdep")]
            class: self.class,
        }
    }

    pub fn try_lock(&self) -> Option<IrqGuard<'_, T>> {
        let was = crate::arch::CurrentArch::are_interrupts_enabled();
        if was {
            crate::arch::CurrentArch::disable_interrupts();
        }
        let inner_guard = self.inner.try_lock()?;
        #[cfg(feature = "lockdep")]
        crate::smp::lockdep::acquire(self.class);
        Some(IrqGuard {
            guard: Some(inner_guard),
            was_enabled: was,
            #[cfg(feature = "lockdep")]
            class: self.class,
        })
    }
}

pub struct IrqGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
    #[cfg(feature = "lockdep")]
    class: u32,
}

impl<'a, T> IrqGuard<'a, T> {
    fn take_guard(&mut self) -> spin::MutexGuard<'a, T> {
        self.guard.take().expect("IrqGuard already consumed")
    }
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap().deref()
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap().deref_mut()
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        let guard = self.take_guard();
        drop(guard);
        // Pop before re-enabling IRQs so the stack stays exclusive to this
        // context for the entire hold.
        #[cfg(feature = "lockdep")]
        crate::smp::lockdep::release(self.class);
        if self.was_enabled {
            crate::arch::CurrentArch::enable_interrupts();
        }
    }
}
