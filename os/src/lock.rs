//! 短临界区。守卫在本核关闭中断，自旋锁保护跨核访问，释放时恢复原中断状态。
use crate::platform::{disable, restore};
use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};
pub struct Lock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}
// SAFETY: 访问 value 必须取得唯一守卫。守卫不可跨核移动。
unsafe impl<T: Send> Sync for Lock<T> {}
pub struct Guard<'a, T> {
    lock: &'a Lock<T>,
    irq: usize,
    _local: core::marker::PhantomData<*mut ()>,
}
impl<T> Lock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
    #[cfg(feature = "trace")]
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        let irq = disable();
        if self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard {
                lock: self,
                irq,
                _local: core::marker::PhantomData,
            })
        } else {
            restore(irq);
            None
        }
    }
    pub fn lock(&self) -> Guard<'_, T> {
        let irq = disable();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Guard {
            lock: self,
            irq,
            _local: core::marker::PhantomData,
        }
    }
}
impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 当前守卫从加锁到析构独占该值。
        unsafe { &*self.lock.value.get() }
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 互斥守卫本身也被独占借用。
        unsafe { &mut *self.lock.value.get() }
    }
}
impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        restore(self.irq)
    }
}
