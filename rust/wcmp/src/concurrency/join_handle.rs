// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The handle of a task spawned into a store.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::Result;
use crate::internal::JoinHandleInternal;
use crate::store::StoreId;
use crate::value::Val;

use super::accessor::Accessor;
use super::host_task::HostTask;
use super::host_task_body::HostTaskBody;
use super::maybe_send::MaybeSend;
use crate::internal::AccessorInternal;

/// The handle of a task spawned into a store, through
/// [`Store::spawn`](crate::Store::spawn) or [`Accessor::spawn`].
///
/// A spawned task is a future the store owns and polls as one of its
/// host tasks. It is registered without being awaited: it makes
/// progress whenever something drives the store, which is a
/// `run_concurrent` entry, [`Store::run_until_stalled`], or a call into
/// an export, and it can call async exports through the accessor it
/// was handed, as the closure of `run_concurrent` can. A task that
/// is waiting leaves the store free for synchronous calls, and for
/// other tasks, which progress independently of it.
///
/// The store polls host tasks, a spawned task among them, in a turn
/// that finds no guest resumption deferred by a `thread.yield`. While
/// guest tasks keep yielding, a spawned future is not polled again,
/// so a continuation it has after a call resolved, and the handle's
/// resolution, wait until the yielding tasks have run out of yields.
///
/// # Output
///
/// The handle is a future. It resolves to `Some(output)` once the task
/// ran to completion, and to `None` when the task ended without
/// producing one: it was aborted, or the store dropped it unfinished,
/// either because the store itself dropped or because a trap
/// poisoned the store and discarded its work. The future resolves
/// after the task's own future was dropped, so what that future held
/// is gone by then. It is awaited inside a `run_concurrent` entry, or
/// by any executor: a wake the task's completion sends reaches the
/// waker the handle was last polled with. [`is_finished`] answers
/// without polling, for a host that watches the task between bursts of
/// [`Store::run_until_stalled`].
///
/// The output is handed out once. A handle polled again after it
/// resolved resolves to `None`.
///
/// # Abort
///
/// Dropping the handle does not affect the task, as in Wasmtime: the
/// task stays in the store and runs on until it completes or the store
/// drops. [`abort`] is the only way to cancel it. It flags the task
/// and wakes it, and the next turn that polls the task drops its
/// future instead of polling it, inside the turn, where the future's
/// destructors can reach the store through the accessor it kept. A
/// task whose future is mid-poll when `abort` is called finishes that
/// poll first. What the task already started in the guest is not
/// cancelled: a call into an export the future made keeps running as
/// the call future does when it is dropped.
///
/// The flag is polled by the store's turns, so an aborted task is
/// dropped only once something drives the store. Wasmtime's handle
/// behaves the same.
///
/// [`is_finished`]: Self::is_finished
/// [`abort`]: Self::abort
/// [`Store::run_until_stalled`]: crate::Store::run_until_stalled
pub struct JoinHandle<R> {
    shared: Arc<Mutex<Shared<R>>>,
}

/// What the handle and the task's body share.
struct Shared<R> {
    /// Whether the task has ended, either way.
    ended: bool,
    /// What the task produced, until the handle takes it.
    output: Option<R>,
    /// Whether [`JoinHandle::abort`] was called while the task ran.
    abort_requested: bool,
    /// The waker of the task's latest poll, which an abort wakes so
    /// that the next turn takes the task and finds the flag.
    task_waker: Option<Waker>,
    /// The waker of the handle's latest poll.
    join_waker: Option<Waker>,
}

impl<R> Shared<R> {
    fn lock(shared: &Mutex<Self>) -> MutexGuard<'_, Self> {
        // The lock is held for field accesses and never across a poll
        // or a drop of the task's future, so it is poisoned only by a
        // panic in this module.
        shared.lock().expect("join state lock poisoned")
    }
}

impl<R> JoinHandle<R> {
    /// Ask the task to end. It does not drop the task at once: it
    /// flags it and wakes it, and the next turn that polls the task
    /// drops its future instead. Nothing happens for a task that has
    /// ended already.
    pub fn abort(&self) {
        let waker = {
            let mut shared = Shared::lock(&self.shared);
            if shared.ended {
                return;
            }
            shared.abort_requested = true;
            shared.task_waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether the task has ended: completed, aborted, or dropped with
    /// the store's work. Polling the handle then resolves at once.
    pub fn is_finished(&self) -> bool {
        Shared::lock(&self.shared).ended
    }
}

impl<R> Future for JoinHandle<R> {
    type Output = Option<R>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<R>> {
        let mut shared = Shared::lock(&self.shared);
        if shared.ended {
            return Poll::Ready(shared.output.take());
        }
        if shared
            .join_waker
            .as_ref()
            .is_none_or(|kept| !kept.will_wake(context.waker()))
        {
            shared.join_waker = Some(context.waker().clone());
        }
        Poll::Pending
    }
}

impl<T: 'static, R: MaybeSend + 'static> JoinHandleInternal<T, R> for JoinHandle<R> {
    fn spawned<F, Fut>(store: StoreId, task: F) -> (Self, HostTask<T>)
    where
        F: FnOnce(Accessor<T>) -> Fut,
        Fut: Future<Output = R> + MaybeSend + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared {
            ended: false,
            output: None,
            abort_requested: false,
            task_waker: None,
            join_waker: None,
        }));
        // The future is built here and polled by the store: building
        // an `async` block runs none of it, and the accessor it
        // captures reaches the store only inside a poll.
        let future = task(Accessor::new(store));
        let body = SpawnedBody {
            future: Some(Box::pin(future)),
            shared: Arc::clone(&shared),
        };
        // The task is the host's own work and resolves no subtask,
        // which is the shape of a host pipe. Its output goes to the
        // handle, so what the store lowers on completion is nothing.
        let task = HostTask::copy(
            |_store: &mut crate::store::StoreContext<'_, T>, outcome: Result<Vec<Val>>| {
                outcome.map(drop)
            },
            body,
        )
        .host_only();
        (Self { shared }, task)
    }
}

/// The body of a spawned task's host task.
struct SpawnedBody<R, Fut> {
    /// The task's future, until it completes or is aborted.
    future: Option<Pin<Box<Fut>>>,
    shared: Arc<Mutex<Shared<R>>>,
}

impl<R, Fut> SpawnedBody<R, Fut> {
    /// End the task with `output`, and wake the handle.
    fn end(&self, output: Option<R>) {
        let waker = {
            let mut shared = Shared::lock(&self.shared);
            shared.ended = true;
            shared.output = output;
            shared.task_waker = None;
            shared.join_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T: 'static, R: MaybeSend + 'static, Fut: Future<Output = R> + MaybeSend + 'static>
    HostTaskBody<T> for SpawnedBody<R, Fut>
{
    fn poll(
        &mut self,
        _accessor: &Accessor<T>,
        context: &mut Context<'_>,
    ) -> Poll<Result<Vec<Val>>> {
        let aborted = {
            let mut shared = Shared::lock(&self.shared);
            if shared
                .task_waker
                .as_ref()
                .is_none_or(|kept| !kept.will_wake(context.waker()))
            {
                shared.task_waker = Some(context.waker().clone());
            }
            shared.abort_requested
        };
        if aborted {
            // The future drops here, in the turn, with the store in
            // the poll's slot, and the handle resolves after it.
            self.future = None;
            self.end(None);
            return Poll::Ready(Ok(Vec::new()));
        }
        let future = self
            .future
            .as_mut()
            .expect("a spawned task is not polled after it ended");
        match future.as_mut().poll(context) {
            Poll::Ready(output) => {
                self.future = None;
                self.end(Some(output));
                Poll::Ready(Ok(Vec::new()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R, Fut> Drop for SpawnedBody<R, Fut> {
    /// A body the store drops unfinished ends the task for the
    /// handle: the store dropped, or a trap discarded its work.
    fn drop(&mut self) {
        let ended = Shared::lock(&self.shared).ended;
        if !ended {
            self.future = None;
            self.end(None);
        }
    }
}
