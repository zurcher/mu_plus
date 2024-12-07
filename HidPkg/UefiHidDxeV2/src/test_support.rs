//! DXE Core Test Support
//!
//! Code to help support testing.
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
use r_efi::efi;
use std::any::Any;

/// A global mutex that can be used for tests to synchronize on access to global state.
/// Usage model is for tests that affect or assert things against global state to acquire this mutex to ensure that
/// other tests run in parallel do not modify or interact with global state non-deterministically.
/// The test should acquire the mutex when it starts to care about or modify global state, and release it when it no
/// longer cares about global state or modifies it (typically this would be the start and end of a test case,
/// respectively).
static GLOBAL_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// All tests should run from inside this.
pub(crate) fn with_global_lock<F: Fn() + std::panic::RefUnwindSafe>(f: F) -> Result<(), Box<dyn Any + Send>> {
    let _guard = GLOBAL_STATE_TEST_LOCK.lock().unwrap();
    std::panic::catch_unwind(|| {
        f();
    })
}
