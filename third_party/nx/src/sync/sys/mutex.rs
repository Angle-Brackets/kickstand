use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::*;

//use super::*;

use crate::diag::abort;
use crate::svc;

use super::get_current_thread_handle;

const WAIT_MASK: u32 = 0x40_00_00_00;

pub struct Mutex {
    owner_thread_handle: AtomicU32,
}

impl Mutex {
    #[inline]
    pub const fn new() -> Self {
        Self {
            owner_thread_handle: AtomicU32::new(svc::INVALID_HANDLE),
        }
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        (self.owner_thread_handle.load(Relaxed) & !WAIT_MASK) != svc::INVALID_HANDLE
    }

    #[inline]
    pub fn try_lock(&self) -> bool {
        self.owner_thread_handle
            .compare_exchange(
                svc::INVALID_HANDLE,
                get_current_thread_handle(),
                Acquire,
                Relaxed,
            )
            .is_ok()
    }

    #[inline]
    pub fn lock(&self) {
        let current_thread = get_current_thread_handle();
        let _current_val = self.owner_thread_handle.load(Relaxed);
        if let Err(state) = self.owner_thread_handle.compare_exchange(
            svc::INVALID_HANDLE,
            current_thread,
            Acquire,
            Relaxed,
        ) {
            self.lock_contended(state, current_thread);
        }
    }

    #[cold]
    fn lock_contended(&self, mut state: u32, current_thread: u32) {
        // As libnx's `mutexLock`: flag the owner's handle as waited on, then let the kernel queue us until
        // the owner hands the mutex over. Every pass waits in the kernel or retries with a fresh value; it
        // used to spin without either when the wait flag was already set (a second waiter), and on one core
        // the spinning thread starved the owner forever.
        loop {
            if state == svc::INVALID_HANDLE {
                match self.owner_thread_handle.compare_exchange(svc::INVALID_HANDLE, current_thread, Acquire, Relaxed) {
                    Ok(_) => return,
                    Err(s) => {
                        state = s;
                        continue;
                    }
                }
            }
            if state & WAIT_MASK == 0 {
                if let Err(s) = self.owner_thread_handle.compare_exchange(state, state | WAIT_MASK, Acquire, Relaxed) {
                    state = s;
                    continue;
                }
                state |= WAIT_MASK;
            }
            if let Err(rc) = unsafe {
                svc::arbitrate_lock(state & !WAIT_MASK, &self.owner_thread_handle as *const _ as _, current_thread)
            } {
                abort::abort(abort::AbortLevel::Panic(), rc);
            }
            state = self.owner_thread_handle.load(Acquire);
            if state & !WAIT_MASK == current_thread {
                return;
            }
        }
    }

    /// # Safety
    ///
    /// This can only be called on a mutex that has been locked by the owner/borrower as it will directly overwrite
    /// the value without checking if it was already
    #[inline]
    pub unsafe fn unlock(&self) {
        // As libnx's `mutexUnlock`: with nobody waiting, clear the owner; otherwise leave the value to the
        // kernel, which writes the next owner's handle. (Clearing it first, as this used to, let another
        // thread take the mutex in between, and then the kernel gave it to a waiter as well.)
        let mut state = self.owner_thread_handle.load(Relaxed);
        loop {
            if state & WAIT_MASK != 0 {
                if let Err(rc) = unsafe { svc::arbitrate_unlock(&self.owner_thread_handle as *const _ as _) } {
                    abort::abort(abort::AbortLevel::Panic(), rc);
                }
                return;
            }
            match self.owner_thread_handle.compare_exchange_weak(state, svc::INVALID_HANDLE, Release, Relaxed) {
                Ok(_) => return,
                Err(s) => state = s,
            }
        }
    }
}

impl Default for Mutex {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::declare_interior_mutable_const)]
unsafe impl lock_api::RawMutex for Mutex {
    // mark that a mutex guard can be Send
    type GuardMarker = lock_api::GuardSend;

    // const initializer for the Mutex
    const INIT: Self = Self::new();

    fn is_locked(&self) -> bool {
        self.is_locked()
    }

    fn lock(&self) {
        self.lock()
    }

    fn try_lock(&self) -> bool {
        self.try_lock()
    }

    unsafe fn unlock(&self) {
        unsafe { self.unlock() }
    }
}
