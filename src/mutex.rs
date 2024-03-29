// From: https://github.com/quinn-rs/quinn/blob/c1bd43f3a6d4f583912ace35a05c4352d5fd730f/quinn/src/mutex.rs
// Copyright (c) 2018 The quinn Developers
// License: MIT OR Apache-2.0

use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

#[cfg(feature = "lock_tracking")]
mod tracking {
    use super::*;
    use std::{
        collections::VecDeque,
        time::{Duration, Instant},
    };
    use tracing::warn;

    #[derive(Debug)]
    struct Inner<T> {
        last_lock_owner: VecDeque<(&'static str, Duration)>,
        value: T,
    }

    /// A Mutex which optionally allows to track the time a lock was held and
    /// emit warnings in case of excessive lock times
    pub struct Mutex<T> {
        inner: parking_lot::Mutex<Inner<T>>,
    }

    impl<T: Debug> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.inner, f)
        }
    }

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self {
                inner: parking_lot::Mutex::new(Inner {
                    last_lock_owner: VecDeque::new(),
                    value,
                }),
            }
        }

        /// Acquires the lock for a certain purpose
        ///
        /// The purpose will be recorded in the list of last lock owners
        pub fn lock(&self, purpose: &'static str) -> MutexGuard<T> {
            let now = Instant::now();
            let guard = self.inner.lock();

            let lock_time = Instant::now();
            let elapsed = lock_time.duration_since(now);

            if elapsed > Duration::from_millis(2) {
                warn!(
                    "Locking for {} took {:?}. Last owners: {:?}",
                    purpose, elapsed, guard.last_lock_owner
                );
            }

            MutexGuard {
                guard,
                start_time: lock_time,
                purpose,
            }
        }
    }

    pub struct MutexGuard<'a, T> {
        guard: parking_lot::MutexGuard<'a, Inner<T>>,
        start_time: Instant,
        purpose: &'static str,
    }

    impl<'a, T> Debug for MutexGuard<'a, T>
    where
        T: Debug,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.guard, f)
        }
    }

    impl<'a, T> Drop for MutexGuard<'a, T> {
        fn drop(&mut self) {
            if self.guard.last_lock_owner.len() == MAX_LOCK_OWNERS {
                self.guard.last_lock_owner.pop_back();
            }

            let duration = self.start_time.elapsed();

            if duration > Duration::from_millis(2) {
                warn!(
                    "Utilizing the lock for {} took {:?}",
                    self.purpose, duration
                );
            }

            self.guard
                .last_lock_owner
                .push_front((self.purpose, duration));
        }
    }

    impl<'a, T> Deref for MutexGuard<'a, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.guard.value
        }
    }

    impl<'a, T> DerefMut for MutexGuard<'a, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.guard.value
        }
    }

    const MAX_LOCK_OWNERS: usize = 20;
}

#[cfg(feature = "lock_tracking")]
pub use tracking::{Mutex, MutexGuard};

#[cfg(not(feature = "lock_tracking"))]
mod non_tracking {
    use super::*;

    /// A Mutex which optionally allows to track the time a lock was held and
    /// emit warnings in case of excessive lock times
    #[derive(Debug)]
    pub struct Mutex<T> {
        inner: parking_lot::Mutex<T>,
    }

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self {
                inner: parking_lot::Mutex::new(value),
            }
        }

        /// Acquires the lock for a certain purpose
        ///
        /// The purpose will be recorded in the list of last lock owners
        pub fn lock(&self, _purpose: &'static str) -> MutexGuard<T> {
            MutexGuard {
                guard: self.inner.lock(),
            }
        }
    }

    pub struct MutexGuard<'a, T> {
        guard: parking_lot::MutexGuard<'a, T>,
    }

    impl<'a, T> Deref for MutexGuard<'a, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            self.guard.deref()
        }
    }

    impl<'a, T> DerefMut for MutexGuard<'a, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.guard.deref_mut()
        }
    }
}

#[cfg(not(feature = "lock_tracking"))]
pub use non_tracking::{Mutex, MutexGuard};
