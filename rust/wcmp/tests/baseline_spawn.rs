// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Baseline tests for tasks spawned into a store.
//!
//! `Store::spawn` registers a future with the store without awaiting
//! it, and hands back a `JoinHandle`. The future makes progress
//! whenever the store is driven, and it can call async exports
//! through the accessor it is handed, so a host starts a long-lived
//! guest task once and drives it in bounded bursts of
//! `run_until_stalled`, entering synchronous exports between them.
//!
//! Each burst of the tests below ends at the guest's `thread.yield`,
//! which is what a store that stalls with work left looks like. The
//! handle observes the output, aborts the task only when asked, and
//! resolves to `None` for a task the store dropped unfinished.

#![cfg(test)]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use wcmp::{Component, Engine, Func, Instance, Linker, Store, Val};
use wcmp_macros::component;

#[cfg(target_arch = "wasm32")]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Three exports of one instance. `count` is an async export that
/// gives way `n` times before it returns `n`: it keeps `n` in the
/// high bits of its task's context slot and the turns left in the
/// low byte, and every yield is a point where a driver stalls.
/// `stuck` waits on a set no turn fills, so it never resolves.
/// `double` is a synchronous export.
const COUNTERS: &[u8] = component!(
    r#"
    (component
      (core func $task-return (canon task.return (result u32)))
      (core func $context-get (canon context.get i32 0))
      (core func $context-set (canon context.set i32 0))
      (core func $set-new (canon waitable-set.new))
      (core module $m
        (import "" "task.return" (func $task-return (param i32)))
        (import "" "context.get" (func $context-get (result i32)))
        (import "" "context.set" (func $context-set (param i32)))
        (import "" "waitable-set.new" (func $set-new (result i32)))
        (func (export "count") (param i32) (result i32)
          (call $context-set
            (i32.or (i32.shl (local.get 0) (i32.const 8)) (local.get 0)))
          ;; Yield.
          (i32.const 1))
        (func (export "count-callback") (param i32 i32 i32) (result i32)
          (call $context-set (i32.sub (call $context-get) (i32.const 1)))
          (if (i32.eqz (i32.and (call $context-get) (i32.const 255)))
            (then
              (call $task-return (i32.shr_u (call $context-get) (i32.const 8)))
              ;; Exit.
              (return (i32.const 0))))
          ;; Yield.
          (i32.const 1))
        (func (export "stuck") (param i32) (result i32)
          ;; Wait on a fresh set.
          (i32.or (i32.shl (call $set-new) (i32.const 4)) (i32.const 2)))
        (func (export "stuck-callback") (param i32 i32 i32) (result i32) unreachable)
        (func (export "double") (param i32) (result i32)
          local.get 0 i32.const 2 i32.mul))
      (core instance $i (instantiate $m
        (with "" (instance
          (export "task.return" (func $task-return))
          (export "context.get" (func $context-get))
          (export "context.set" (func $context-set))
          (export "waitable-set.new" (func $set-new))))))
      (func (export "count") async (param "n" u32) (result u32)
        (canon lift (core func $i "count") async
          (callback (core func $i "count-callback"))))
      (func (export "stuck") async (param "x" u32) (result u32)
        (canon lift (core func $i "stuck") async
          (callback (core func $i "stuck-callback"))))
      (func (export "double") (param "x" u32) (result u32)
        (canon lift (core func $i "double"))))
    "#
);

/// Instantiate [`COUNTERS`] with a store whose host data is nothing.
async fn instantiate() -> (Store<()>, Instance) {
    let engine = Engine::with_backend(crate::test_backend::backend()).expect("engine");
    let component = Component::new(&engine, COUNTERS)
        .await
        .expect("component parses");
    let mut store: Store<()> = Store::new(&engine, ()).expect("store");
    let linker: Linker<()> = Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &component)
        .await
        .expect("instantiate");
    (store, instance)
}

/// One export of the instance, by name.
fn func(instance: &Instance, name: &str) -> Func {
    instance
        .get_func(name)
        .unwrap_or_else(|| panic!("the component exports `{name}`"))
}

/// A value whose drop is counted, so a test sees a task's future go.
struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// How many bursts the tests allow before they call a task lost.
const BURSTS: usize = 64;

/// Run bursts until `done` holds, and answer how many it took.
fn burst_until(store: &mut Store<()>, mut done: impl FnMut() -> bool) -> usize {
    for bursts in 1..=BURSTS {
        store.run_until_stalled().expect("a burst");
        if done() {
            return bursts;
        }
    }
    panic!("the task was still unfinished after {BURSTS} bursts");
}

#[wcmp_macros::test]
async fn it_completes_a_spawned_call_across_bursts_and_hands_back_its_output() {
    let (mut store, instance) = instantiate().await;
    let count = func(&instance, "count");

    let handle = store
        .spawn(async move |accessor| count.call_concurrent(&accessor, &[Val::U32(4)]).await)
        .expect("spawn");
    assert!(!handle.is_finished(), "spawning polls nothing");

    let bursts = burst_until(&mut store, || handle.is_finished());
    assert!(
        bursts > 1,
        "the guest yields four times, and a yield ends a burst: finished after {bursts}"
    );

    let output = store
        .run_concurrent(async |_| handle.await)
        .await
        .expect("run the closure");
    let results = output
        .expect("the task completed")
        .expect("the call resolved");
    assert_eq!(
        results.as_ref(),
        [Val::U32(4)],
        "the handle carries what the spawned future returned"
    );
}

#[wcmp_macros::test]
async fn it_runs_a_synchronous_export_while_a_spawned_call_is_suspended() {
    let (mut store, instance) = instantiate().await;
    let stuck = func(&instance, "stuck");
    let double = func(&instance, "double");

    let handle = store
        .spawn(async move |accessor| stuck.call_concurrent(&accessor, &[Val::U32(0)]).await)
        .expect("spawn");
    store.run_until_stalled().expect("a burst");
    assert!(!handle.is_finished(), "the spawned call waits for ever");

    let doubled = double
        .call(&mut store, &[Val::U32(21)])
        .await
        .expect("the synchronous export is entered between bursts");
    assert_eq!(doubled.as_ref(), [Val::U32(42)]);
    assert!(
        !handle.is_finished(),
        "the synchronous call did not disturb the suspended task"
    );
}

#[wcmp_macros::test]
async fn it_progresses_two_spawned_tasks_independently() {
    let (mut store, instance) = instantiate().await;
    let (counting, waiting) = (func(&instance, "count"), func(&instance, "stuck"));

    let finishes = store
        .spawn(async move |accessor| counting.call_concurrent(&accessor, &[Val::U32(3)]).await)
        .expect("spawn the task that finishes");
    let stays = store
        .spawn(async move |accessor| waiting.call_concurrent(&accessor, &[Val::U32(0)]).await)
        .expect("spawn the task that waits for ever");

    burst_until(&mut store, || finishes.is_finished());
    assert!(
        !stays.is_finished(),
        "the first task finished while the second is still suspended"
    );
    let results = store
        .run_concurrent(async |_| finishes.await)
        .await
        .expect("run the closure");
    assert_eq!(
        results.expect("completed").expect("resolved").as_ref(),
        [Val::U32(3)]
    );
}

#[wcmp_macros::test]
async fn it_hands_each_of_two_overlapping_tasks_its_own_output() {
    let (mut store, instance) = instantiate().await;
    let (short, long) = (func(&instance, "count"), func(&instance, "count"));

    let first = store
        .spawn(async move |accessor| short.call_concurrent(&accessor, &[Val::U32(2)]).await)
        .expect("spawn the short task");
    let second = store
        .spawn(async move |accessor| long.call_concurrent(&accessor, &[Val::U32(9)]).await)
        .expect("spawn the long task");

    burst_until(&mut store, || first.is_finished() && second.is_finished());
    let (first, second) = store
        .run_concurrent(async |_| (first.await, second.await))
        .await
        .expect("run the closure");
    assert_eq!(
        first.expect("completed").expect("resolved").as_ref(),
        [Val::U32(2)]
    );
    assert_eq!(
        second.expect("completed").expect("resolved").as_ref(),
        [Val::U32(9)]
    );
}

#[wcmp_macros::test]
async fn it_cancels_a_task_when_its_handle_aborts_it_and_not_when_it_drops() {
    let (mut store, instance) = instantiate().await;
    let stuck = func(&instance, "stuck");
    let count = func(&instance, "count");

    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(dropped.clone());
    let aborted = store
        .spawn(async move |accessor| {
            let _guard = guard;
            stuck.call_concurrent(&accessor, &[Val::U32(0)]).await
        })
        .expect("spawn the task to abort");

    // A task whose handle is dropped runs on.
    let completed = Arc::new(AtomicBool::new(false));
    let flag = completed.clone();
    drop(
        store
            .spawn(async move |accessor| {
                let results = count.call_concurrent(&accessor, &[Val::U32(3)]).await;
                flag.store(true, Ordering::Release);
                results
            })
            .expect("spawn the task whose handle drops"),
    );

    store.run_until_stalled().expect("a burst");
    assert_eq!(dropped.load(Ordering::Acquire), 0, "the task is waiting");

    aborted.abort();
    assert!(
        !aborted.is_finished(),
        "an abort flags the task, and a turn drops it"
    );
    burst_until(&mut store, || completed.load(Ordering::Acquire));

    assert!(aborted.is_finished());
    assert_eq!(
        dropped.load(Ordering::Acquire),
        1,
        "the turn after the abort dropped the task's future"
    );
    assert!(
        store
            .run_concurrent(async |_| aborted.await)
            .await
            .expect("run the closure")
            .is_none(),
        "an aborted task has no output"
    );
    assert!(
        completed.load(Ordering::Acquire),
        "dropping a handle cancelled nothing"
    );
}

#[wcmp_macros::test]
async fn it_drops_a_pending_spawned_task_with_its_store() {
    let (mut store, instance) = instantiate().await;
    let stuck = func(&instance, "stuck");

    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(dropped.clone());
    let handle = store
        .spawn(async move |accessor| {
            let _guard = guard;
            stuck.call_concurrent(&accessor, &[Val::U32(0)]).await
        })
        .expect("spawn");
    store.run_until_stalled().expect("a burst");
    assert!(!handle.is_finished());
    assert_eq!(dropped.load(Ordering::Acquire), 0);

    drop(store);

    assert_eq!(
        dropped.load(Ordering::Acquire),
        1,
        "the store dropped the pending task's future"
    );
    assert!(handle.is_finished(), "a task the store dropped has ended");
    let mut handle = Box::pin(handle);
    let mut context = Context::from_waker(Waker::noop());
    assert!(
        matches!(
            Pin::as_mut(&mut handle).poll(&mut context),
            Poll::Ready(None)
        ),
        "the handle of a dropped task resolves to no output"
    );
}

#[wcmp_macros::test]
async fn it_drops_a_spawned_task_that_never_ran_with_its_store() {
    let (mut store, _instance) = instantiate().await;
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(dropped.clone());
    let handle = store
        .spawn(async move |_accessor| {
            let _guard = guard;
            core::future::pending::<()>().await
        })
        .expect("spawn");

    drop(store);

    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert!(handle.is_finished());
}

#[path = "support/backend.rs"]
mod test_backend;
