//! IRQ-safe mutex: a `spin::Mutex` guarded by a local-interrupt disable.
//!
//! The VFS (`vfs/irq.rs`) and the universal timer previously each carried a
//! private copy of this pattern; this module is the single shared
//! implementation.  IRQs are disabled for the whole critical section so an
//! ISR (e.g. the timer tick) cannot re-enter the same lock.  The `order`
//! key (see [`crate::services::lockorder`]) feeds a debug-only per-CPU
//! lockdep stack that reports nested-acquire violations instead of letting
//! the `spin::Mutex` hang silently.

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use spin::Mutex;

/// A mutex that holds local interrupts disabled while locked.
///
/// `order` is the lockdep ordering key (see `crate::services::lockorder`);
/// `0` means the lock is exempt from ordering checks.  Under the `lockdep`
/// feature, `lock()` records the acquire on this CPU's lockdep stack, which
/// asserts strictly-increasing acquisition order and catches same-lock
/// recursion before the `spin::Mutex` would hang silently.
pub struct IrqLock<T> {
    inner: Mutex<T>,
    order: u8,
}

impl<T> IrqLock<T> {
    /// Create an IRQ-safe lock with no lockdep ordering key (`order = 0`).
    pub const fn new(val: T) -> Self {
        IrqLock { inner: Mutex::new(val), order: 0 }
    }

    /// Create an IRQ-safe lock with a lockdep ordering key.
    ///
    /// Ordering keys must be strictly increasing across nested acquires on a
    /// given CPU (see `crate::services::lockorder`).
    pub const fn with_order(val: T, order: u8) -> Self {
        IrqLock { inner: Mutex::new(val), order }
    }

    /// Lock, disabling local interrupts for the duration of the guard.
    ///
    /// Interrupts are restored on drop (re-enabled iff they were enabled on
    /// entry).  The debug-only lockdep check runs before the spin lock is
    /// taken so a same-lock recursion is reported instead of hanging.
    pub fn lock(&self) -> IrqGuard<'_, T> {
        let was_enabled = crate::arch::CurrentArch::are_interrupts_enabled();
        if was_enabled {
            crate::arch::CurrentArch::disable_interrupts();
        }

        // No-op unless the `lockdep` feature is enabled (and `order != 0`).
        // Runs before the spin lock so recursion is caught, not hung on.
        crate::smp::lockdep_push(self.order);

        IrqGuard {
            guard: ManuallyDrop::new(self.inner.lock()),
            was_enabled,
            order: self.order,
        }
    }
}

/// RAII guard returned by [`IrqLock::lock`].
///
/// Dereferences to the locked value; on drop the mutex is released, the
/// lockdep stack popped, and interrupts restored to their prior state.
pub struct IrqGuard<'a, T> {
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
    order: u8,
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.deref()
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.deref_mut()
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        // The wrapped `MutexGuard` is dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        crate::smp::lockdep_pop(self.order);
        if self.was_enabled {
            crate::arch::CurrentArch::enable_interrupts();
        }
    }
}
