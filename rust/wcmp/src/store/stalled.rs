// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Why a drive of a store stopped.

/// Why [`Store::run_until_stalled`] stopped.
///
/// A drive runs ready work with a no-op waker, so it cannot schedule its own
/// next invocation. What the host does next depends on whether the store
/// still holds work of its own: a guest that yielded is runnable again as
/// soon as the host lets it, but a guest waiting on a host task is runnable
/// only after that task wakes.
///
/// [`Store::run_until_stalled`]: crate::Store::run_until_stalled
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stalled {
    /// Nothing is ready. The store is idle, or every task is waiting on a host
    /// task, a waitable, or a thread left for its suspend provider, and
    /// drives again only after external work makes one of them ready.
    Idle,
    /// A task gave way with `thread.yield` or `task.yield` and is ready to
    /// run. The drive returns so the host's other work is not starved; the
    /// host drives again, as soon as it chooses.
    Yielded,
}
