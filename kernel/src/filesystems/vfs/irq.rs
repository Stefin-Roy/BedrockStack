use core::ops::{Deref, DerefMut};
use spin::Mutex;

use crate::arch::Arch;

pub struct IrqMutex<T> {
    inner: Mutex<T>,
}

impl<T> IrqMutex<T> {
    pub const fn new(val: T) -> Self {
        IrqMutex { inner: Mutex::new(val) }
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let was = crate::arch::CurrentArch::are_interrupts_enabled();
        if was {
            crate::arch::CurrentArch::disable_interrupts();
        }
        IrqGuard {
            guard: Some(self.inner.lock()),
            was_enabled: was,
        }
    }
}

pub struct IrqGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
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
        if self.was_enabled {
            crate::arch::CurrentArch::enable_interrupts();
        }
    }
}
