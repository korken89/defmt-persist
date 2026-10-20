//! Atomics, replaced by loom's when the crate is checked with `--cfg loom`.

#[cfg(all(feature = "ecc", not(loom)))]
pub(crate) use core::sync::atomic::compiler_fence;
#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU8, AtomicU32, fence};
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU8, AtomicU32, fence};

/// Has no effect between threads, which is all that loom checks.
#[cfg(all(feature = "ecc", loom))]
pub(crate) fn compiler_fence(_: core::sync::atomic::Ordering) {}
