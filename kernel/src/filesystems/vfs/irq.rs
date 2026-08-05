//! VFS IRQ-safe mutex.
//!
//! Thin alias over the shared [`crate::services::irqsafe::IrqLock`] so VFS
//! call sites keep the `IrqMutex` name.  `IrqMutex::new` is order-0 (no
//! lockdep key); `IrqMutex::with_order` opts a lock into the `lockdep`
//! hierarchy checks (see `crate::services::lockorder`).

pub type IrqMutex<T> = crate::services::irqsafe::IrqLock<T>;
