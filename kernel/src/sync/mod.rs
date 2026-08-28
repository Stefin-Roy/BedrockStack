//! Preemption-aware locks for BSP-only full preemption (Option A).
//!
//! `PreemptMutex` disables preemption (not IRQs) while held, so a holder
//! that is preempted cannot deadlock a spinner on the same CPU (single-CPU
//! invariant). Used for locks that must keep IRQs enabled across the wait
//! (heap growth → shootdown IPI, block cache wait_slots → AHCI IRQ).
//!
//! `IrqMutex` (in `filesystems::vfs::irq`) disables both IRQs and preemption.

use core::ops::{Deref, DerefMut};

pub struct PreemptMutex<T> {
    inner: spin::Mutex<T>,
}

impl<T> PreemptMutex<T> {
    pub const fn new(val: T) -> Self {
        PreemptMutex { inner: spin::Mutex::new(val) }
    }

    pub fn lock(&self) -> PreemptGuard<'_, T> {
        // Disable preemption before taking the spin lock so holder cannot be
        // preempted and then deadlock spinner on same CPU.
        let was_enabled = crate::smp::preempt_is_enabled();
        if was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                pc.preempt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
            }
        }
        PreemptGuard {
            guard: Some(self.inner.lock()),
            was_enabled,
        }
    }

    pub fn try_lock(&self) -> Option<PreemptGuard<'_, T>> {
        let was_enabled = crate::smp::preempt_is_enabled();
        if was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                pc.preempt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
            }
        }
        let g = match self.inner.try_lock() {
            Some(g) => g,
            None => {
                if was_enabled {
                    crate::smp::preempt_enable_and_maybe_resched();
                }
                return None;
            }
        };
        Some(PreemptGuard { guard: Some(g), was_enabled })
    }
}

pub struct PreemptGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
}

impl<'a, T> PreemptGuard<'a, T> {
    fn take_guard(&mut self) -> spin::MutexGuard<'a, T> {
        self.guard.take().expect("PreemptGuard already consumed")
    }
}

impl<T> Deref for PreemptGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { self.guard.as_ref().unwrap().deref() }
}
impl<T> DerefMut for PreemptGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { self.guard.as_mut().unwrap().deref_mut() }
    // DerefMut::Target is provided via Deref
}
impl<T> Drop for PreemptGuard<'_, T> {
    fn drop(&mut self) {
        let g = self.take_guard();
        drop(g);
        if self.was_enabled {
            crate::smp::preempt_enable_and_maybe_resched();
        }
    }
}

pub struct PreemptRwLock<T> {
    inner: spin::RwLock<T>,
}

impl<T> PreemptRwLock<T> {
    pub const fn new(val: T) -> Self {
        PreemptRwLock { inner: spin::RwLock::new(val) }
    }

    pub fn read(&self) -> PreemptRwLockReadGuard<'_, T> {
        let was_enabled = crate::smp::preempt_is_enabled();
        if was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                pc.preempt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
            }
        }
        PreemptRwLockReadGuard {
            guard: Some(self.inner.read()),
            was_enabled,
        }
    }

    pub fn write(&self) -> PreemptRwLockWriteGuard<'_, T> {
        let was_enabled = crate::smp::preempt_is_enabled();
        if was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                pc.preempt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
            }
        }
        PreemptRwLockWriteGuard {
            guard: Some(self.inner.write()),
            was_enabled,
        }
    }
}

pub struct PreemptRwLockReadGuard<'a, T> {
    guard: Option<spin::RwLockReadGuard<'a, T>>,
    was_enabled: bool,
}

impl<'a, T> PreemptRwLockReadGuard<'a, T> {
    fn take_guard(&mut self) -> spin::RwLockReadGuard<'a, T> {
        self.guard.take().expect("PreemptRwLockReadGuard already consumed")
    }
}

impl<T> Deref for PreemptRwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { self.guard.as_ref().unwrap().deref() }
}

impl<T> Drop for PreemptRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let g = self.take_guard();
        drop(g);
        if self.was_enabled {
            crate::smp::preempt_enable_and_maybe_resched();
        }
    }
}

pub struct PreemptRwLockWriteGuard<'a, T> {
    guard: Option<spin::RwLockWriteGuard<'a, T>>,
    was_enabled: bool,
}

impl<'a, T> PreemptRwLockWriteGuard<'a, T> {
    fn take_guard(&mut self) -> spin::RwLockWriteGuard<'a, T> {
        self.guard.take().expect("PreemptRwLockWriteGuard already consumed")
    }
}

impl<T> Deref for PreemptRwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { self.guard.as_ref().unwrap().deref() }
}
impl<T> DerefMut for PreemptRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { self.guard.as_mut().unwrap().deref_mut() }
}
impl<T> Drop for PreemptRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let g = self.take_guard();
        drop(g);
        if self.was_enabled {
            crate::smp::preempt_enable_and_maybe_resched();
        }
    }
}
