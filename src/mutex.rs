// From: https://github.com/quinn-rs/quinn/blob/c1bd43f3a6d4f583912ace35a05c4352d5fd730f/quinn/src/mutex.rs
// Copyright (c) 2018 The quinn Developers
// License: MIT OR Apache-2.0

use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

/// A Mutex which optionally allows to track the time a lock was held and
/// emit warnings in case of excessive lock times
///
/// TODO: Actually add tracing impl for tracking.
#[derive(Debug)]
pub struct Mutex<T> {
    inner: std::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: std::sync::Mutex::new(value),
        }
    }

    /// Acquires the lock for a certain purpose
    ///
    /// The purpose will be recorded in the list of last lock owners
    pub fn lock(&self, purpose: &'static str) -> MutexGuard<T> {
        // eprintln!("TAKE: {}", purpose);
        let guard = self.inner.lock().unwrap();
        // eprintln!("HOLD: {}", purpose);
        MutexGuard { guard, purpose }
    }
}

pub struct MutexGuard<'a, T> {
    guard: std::sync::MutexGuard<'a, T>,
    purpose: &'static str,
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        // eprintln!("DROP: {}", self.purpose);
    }
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
