// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The context slots of the running thread, held in core globals.
//!
//! `context.get` and `context.set` read and write a slot of the
//! thread that runs. A guest calls them for every function that has a
//! stack frame, because a toolchain keeps its shadow stack pointer in
//! a slot, so a host function behind each of them costs a crossing of
//! the host boundary, a lock, and a lookup for every one of those
//! calls. The store makes the two imports plain WebAssembly instead.
//!
//! One small core module of the store imports two mutable `i32`
//! globals, one for each slot, and exports a `get` and a `set` function
//! for each that is one `global.get` or `global.set`. The store links
//! those functions in place of the built-ins, so a guest's call never
//! leaves WebAssembly. The globals hold the slots of one thread, the
//! live thread, and the store records which one. The slots of every
//! other thread stay in their thread records, which remain the record
//! of truth for a thread that does not run.
//!
//! The live thread has to be the thread that runs whenever guest code
//! does, so [`sync`] makes it so, and it runs at every point where
//! guest code gets control and the thread that runs could have changed:
//!
//! - before the polyfill calls into the guest, which is every call of a
//!   core function, a thread entry, a resumption, and the `start`
//!   function of a core instance, and
//! - after a host function returns to the guest, because the host
//!   function can have pushed or popped the scope of a task or a
//!   thread, as an adapter's enter and exit intrinsics, a switching
//!   built-in, and a trampoline into another component do.
//!
//! The thread that runs is whatever the store's stack of scopes says
//! it is, so no switch has to be found one by one: the stack changes
//! in many places, and the one thing they all precede is guest code
//! getting control. When the thread that runs differs from the live
//! one, [`sync`] saves the globals into the live thread's record, if
//! the thread still has one, and loads the globals from the record of
//! the thread that runs. A thread record's slots are therefore stale
//! while that thread is the live one, and [`slots_of`] reads the
//! slots the way a host reader must.
//!
//! The other half of the invariant is that `no_task` is set whenever no
//! task is on the stack, so a slot access that gets to run with none
//! traps instead of reading the last thread's values. A task ends by
//! popping its scope under the tables' lock, where no store is at hand
//! to write a global, so the store syncs once more at every point where
//! control comes back to the host: when an item of a turn ends, when a
//! driver's turn ends, when a closure run in a turn ends, and when the
//! instantiation of a core module ends. A module that has just been made
//! starts with the flag set, because no task exists yet.

use crate::concurrency::ThreadId;
use crate::error::{Error, Result};
use crate::internal::ErrorInternal;
use crate::resource::HandleTables;
use crate::runtime_layer::{
    AsContextMut, Extern as RuntimeExtern, Func as RuntimeFunc, Global as RuntimeGlobal,
    GlobalType, Imports, Module as RuntimeModule, Mutability, Val as RuntimeVal, ValType,
    instantiate, substrate_failure,
};
use crate::store::{StoreContext, StoreContextInternalExt};
use std::sync::Mutex;

/// The two slots a thread has, which `context.get` and `context.set`
/// address by index.
pub const SLOTS: usize = 2;

/// What the store holds once an instantiation made the context
/// module: the globals, the functions that read and write them, and
/// the thread whose slots the globals hold.
pub struct ContextSlots {
    globals: [RuntimeGlobal; SLOTS],
    get: [RuntimeFunc; SLOTS],
    set: [RuntimeFunc; SLOTS],
    live: Option<ThreadId>,
    /// Whether the `dead` global is set, which the module's `get` and
    /// `set` functions check and trap on.
    dead: bool,
    /// The global that is 1 while no task is on the stack.
    no_task: RuntimeGlobal,
}

impl ContextSlots {
    /// The bytes of the context module. It imports `context.slot0`
    /// and `context.slot1`, mutable `i32` globals, and `context.no_task`,
    /// a third that is 1 while no task is on the store's stack, and
    /// exports `get0`, `set0`, `get1`, and `set1`:
    ///
    /// ```text
    /// (func (export "get<N>") (result i32)
    ///   (if (global.get $no_task) (then unreachable))
    ///   (global.get $slot<N>))
    /// (func (export "set<N>") (param i32)
    ///   (if (global.get $no_task) (then unreachable))
    ///   (global.set $slot<N> (local.get 0)))
    /// ```
    ///
    /// A slot read or written with no task on the stack is a broken
    /// invariant, which the built-in used to refuse with an error. The
    /// check is one global read and a branch, so the hot path still
    /// makes no host call.
    fn module_bytes() -> Vec<u8> {
        use wasm_encoder::{
            BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function,
            FunctionSection, ImportSection, Instruction, Module, TypeSection, ValType as Encoded,
        };

        let mut types = TypeSection::new();
        types.ty().function([], [Encoded::I32]);
        types.ty().function([Encoded::I32], []);
        let mut imports = ImportSection::new();
        let mut functions = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        let global = EntityType::Global(wasm_encoder::GlobalType {
            val_type: Encoded::I32,
            mutable: true,
            shared: false,
        });
        for slot in 0..SLOTS {
            imports.import("context", &format!("slot{slot}"), global);
        }
        imports.import("context", "no_task", global);
        let no_task = SLOTS as u32;
        for slot in 0..SLOTS as u32 {
            functions.function(0);
            functions.function(1);
            exports.export(&format!("get{slot}"), ExportKind::Func, slot * 2);
            exports.export(&format!("set{slot}"), ExportKind::Func, slot * 2 + 1);
            let mut get = Function::new([]);
            get.instruction(&Instruction::GlobalGet(no_task));
            get.instruction(&Instruction::If(BlockType::Empty));
            get.instruction(&Instruction::Unreachable);
            get.instruction(&Instruction::End);
            get.instruction(&Instruction::GlobalGet(slot));
            get.instruction(&Instruction::End);
            code.function(&get);
            let mut set = Function::new([]);
            set.instruction(&Instruction::GlobalGet(no_task));
            set.instruction(&Instruction::If(BlockType::Empty));
            set.instruction(&Instruction::Unreachable);
            set.instruction(&Instruction::End);
            set.instruction(&Instruction::LocalGet(0));
            set.instruction(&Instruction::GlobalSet(slot));
            set.instruction(&Instruction::End);
            code.function(&set);
        }
        let mut module = Module::new();
        module
            .section(&types)
            .section(&imports)
            .section(&functions)
            .section(&exports)
            .section(&code);
        module.finish()
    }

    /// The function a guest's `context.get` of `slot` links to.
    pub fn getter(&self, slot: usize) -> Result<RuntimeFunc> {
        self.get
            .get(slot)
            .copied()
            .ok_or_else(|| Error::internal("context slot index out of range"))
    }

    /// The function a guest's `context.set` of `slot` links to.
    pub fn setter(&self, slot: usize) -> Result<RuntimeFunc> {
        self.set
            .get(slot)
            .copied()
            .ok_or_else(|| Error::internal("context slot index out of range"))
    }
}

/// Make the context module in `store` unless an earlier instantiation
/// did. The module is a few dozen bytes, so each store compiles its
/// own.
pub async fn install<T: 'static>(store: &mut StoreContext<'_, T>) -> Result<()> {
    if store.internal().lock_tables()?.context_slots.is_some() {
        return Ok(());
    }
    let runtime = store.internal().runtime_mut();
    let mut globals = Vec::with_capacity(SLOTS + 1);
    for i in 0..=SLOTS {
        globals.push(
            RuntimeGlobal::new(
                runtime.as_context_mut(),
                GlobalType::new(ValType::I32, Mutability::Var),
                // No task is on the stack when the module is made: the
                // flag starts set, as `dead` records, and the first
                // `sync` with a task current clears it.
                RuntimeVal::I32(i32::from(i >= SLOTS)),
            )
            .map_err(substrate_failure)?,
        );
    }
    let module = RuntimeModule::new(runtime.engine(), &ContextSlots::module_bytes())
        .map_err(substrate_failure)?;
    let mut imports = Imports::default();
    for (slot, global) in globals.iter().take(SLOTS).enumerate() {
        imports.define("context", &format!("slot{slot}"), *global);
    }
    imports.define("context", "no_task", globals[SLOTS]);
    let instance = instantiate(&mut *runtime, &module, &imports)
        .await
        .map_err(substrate_failure)?;
    let mut export = |name: String| -> Result<RuntimeFunc> {
        instance
            .get_export(&mut *runtime, &name)
            .map_err(substrate_failure)?
            .and_then(RuntimeExtern::into_func)
            .ok_or_else(|| Error::internal("the context module lacks one of its exports"))
    };
    let slots = ContextSlots {
        globals: [globals[0], globals[1]],
        get: [export("get0".into())?, export("get1".into())?],
        set: [export("set0".into())?, export("set1".into())?],
        live: None,
        dead: true,
        no_task: globals[SLOTS],
    };
    store.internal().lock_tables()?.context_slots = Some(slots);
    Ok(())
}

/// Make the live thread the thread that runs, as the module
/// documentation states. A store with no context module has no guest
/// that reads a slot. A store with no task on its stack has no thread to
/// make live, and traps any slot access until a task is current.
///
/// The tables stay locked for the length of the swap, which is one
/// read and one write of each slot's global at most, so no other
/// reader sees the globals half way between two threads.
pub fn sync(mut store: impl AsContextMut, tables: &Mutex<HandleTables>) -> Result<()> {
    let mut guard = tables
        .lock()
        .map_err(|_| Error::internal("resource handle tables lock poisoned"))?;
    let HandleTables {
        tasks,
        context_slots,
        ..
    } = &mut *guard;
    let Some(slots) = context_slots.as_mut() else {
        return Ok(());
    };
    let Some(current) = tasks.current_thread() else {
        // No task is on the stack, so no guest may touch a slot: the
        // module's functions trap until a task is current again.
        if !slots.dead {
            slots
                .no_task
                .set(store.as_context_mut(), RuntimeVal::I32(1))
                .map_err(substrate_failure)?;
            slots.dead = true;
        }
        return Ok(());
    };
    if slots.dead {
        slots
            .no_task
            .set(store.as_context_mut(), RuntimeVal::I32(0))
            .map_err(substrate_failure)?;
        slots.dead = false;
    }
    if slots.live == Some(current) {
        return Ok(());
    }
    if let Some(live) = slots.live
        && let Some(record) = tasks.thread_mut(live)
    {
        record.context = slots.read(&mut store)?;
    }
    let arriving = tasks
        .thread(current)
        .map(|record| record.context)
        .ok_or_else(|| Error::internal("the current thread has no record"))?;
    for (global, value) in slots.globals.iter().zip(arriving) {
        global
            .set(store.as_context_mut(), RuntimeVal::I32(value))
            .map_err(substrate_failure)?;
    }
    slots.live = Some(current);
    Ok(())
}

/// The slots of `thread`, wherever they are now: in the globals while
/// the thread is the live one, and in its record otherwise. `None`
/// when the thread has no record. A host reader goes through this
/// rather than the record, whose slots are stale for the live thread.
pub fn slots_of(
    mut store: impl AsContextMut,
    tables: &Mutex<HandleTables>,
    thread: ThreadId,
) -> Result<Option<[i32; SLOTS]>> {
    let guard = tables
        .lock()
        .map_err(|_| Error::internal("resource handle tables lock poisoned"))?;
    match &guard.context_slots {
        Some(slots) if slots.live == Some(thread) => slots.read(&mut store).map(Some),
        _ => Ok(guard.tasks.thread(thread).map(|record| record.context)),
    }
}

impl ContextSlots {
    /// The values the globals hold now.
    fn read(&self, mut store: impl AsContextMut) -> Result<[i32; SLOTS]> {
        let mut values = [0; SLOTS];
        for (value, global) in values.iter_mut().zip(&self.globals) {
            *value = match global.get(store.as_context_mut()) {
                Ok(RuntimeVal::I32(value)) => value,
                Ok(other) => {
                    return Err(Error::internal(format!(
                        "a context slot global holds {other:?} rather than an i32"
                    )));
                }
                Err(error) => return Err(substrate_failure(error)),
            };
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreInternalExt;
    use crate::{Component, Engine, Instance, Linker, Store};
    use std::sync::Arc;
    use wcmp_macros::component;

    /// A component whose core module imports `context.get` slot 0, so
    /// instantiating it makes the store's context module.
    const READS_A_SLOT: &[u8] = component!(
        r#"
        (component
          (core func $get (canon context.get i32 0))
          (core module $m
            (import "" "context.get" (func $get (result i32)))
            (func (export "run") (result i32) (call $get)))
          (core instance $i (instantiate $m
            (with "" (instance (export "context.get" (func $get))))))
          (func (export "run") (result u32) (canon lift (core func $i "run"))))
        "#
    );

    /// An export that touches no slot, beside the one that reads it.
    const RUNS_AN_EXPORT: &[u8] = component!(
        r#"
        (component
          (core func $get (canon context.get i32 0))
          (core module $m
            (import "" "context.get" (func $get (result i32)))
            (func (export "run") (result i32) (call $get)))
          (core instance $i (instantiate $m
            (with "" (instance (export "context.get" (func $get))))))
          (func (export "run") (result u32) (canon lift (core func $i "run"))))
        "#
    );

    /// Instantiate `bytes` in a fresh store, and hand back the store,
    /// the instance, the context module's getter for slot 0, and the
    /// store's tables.
    async fn instantiated(
        bytes: &[u8],
    ) -> (Store<()>, Instance, RuntimeFunc, Arc<Mutex<HandleTables>>) {
        let engine = Engine::with_backend(crate::runtime_layer::test_backend()).expect("engine");
        let component = Component::new(&engine, bytes)
            .await
            .expect("component parses");
        let mut store: Store<()> = Store::new(&engine, ()).expect("store");
        let linker: Linker<()> = Linker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &component)
            .await
            .expect("instantiate");
        let tables = store.internal().tables_handle();
        let getter = tables
            .lock()
            .expect("tables")
            .context_slots
            .as_ref()
            .expect("the component made the context module")
            .getter(0)
            .expect("slot 0");
        (store, instance, getter, tables)
    }

    #[wcmp_macros::test]
    async fn it_traps_a_slot_access_right_after_instantiation() {
        let (mut store, _instance, getter, _tables) = instantiated(READS_A_SLOT).await;
        let mut answer = [RuntimeVal::I32(-1)];
        // The test syncs nothing: the instantiation's own task ended,
        // and the flag followed it.
        getter
            .call(store.internal().inner_mut(), &[], &mut answer)
            .expect_err("a read after the instantiation's task ended traps");
    }

    #[wcmp_macros::test]
    async fn it_traps_a_slot_access_after_an_export_returned() {
        let (mut store, instance, getter, tables) = instantiated(RUNS_AN_EXPORT).await;
        let run = instance.get_func("run").expect("the export");
        let answer = run.call(&mut store, &[]).await.expect("the export runs");
        assert!(matches!(answer[0], crate::Val::U32(0)), "{answer:?}");
        let mut raw = [RuntimeVal::I32(-1)];
        getter
            .call(store.internal().inner_mut(), &[], &mut raw)
            .expect_err("a read after the export returned traps");
        assert!(
            tables
                .lock()
                .expect("tables")
                .tasks
                .current_thread()
                .is_none(),
            "no task is on the stack"
        );
    }

    #[wcmp_macros::test]
    async fn it_traps_a_slot_access_while_no_task_is_on_the_stack() {
        let engine = Engine::with_backend(crate::runtime_layer::test_backend()).expect("engine");
        let component = Component::new(&engine, READS_A_SLOT)
            .await
            .expect("component parses");
        let mut store: Store<()> = Store::new(&engine, ()).expect("store");
        let linker: Linker<()> = Linker::new(&engine);
        linker
            .instantiate(&mut store, &component)
            .await
            .expect("instantiate");
        let tables = store.internal().tables_handle();
        let getter = tables
            .lock()
            .expect("tables")
            .context_slots
            .as_ref()
            .expect("the component made the context module")
            .getter(0)
            .expect("slot 0");
        let mut answer = [RuntimeVal::I32(-1)];

        sync(store.internal().inner_mut(), &tables).expect("sync with no task");
        getter
            .call(store.internal().inner_mut(), &[], &mut answer)
            .expect_err("a read with no task on the stack traps");

        let instance = tables.lock().expect("tables").tasks.insert_instance();
        tables
            .lock()
            .expect("tables")
            .tasks
            .push_task(None, None, instance)
            .expect("room under the record cap");
        sync(store.internal().inner_mut(), &tables).expect("sync with a task");
        getter
            .call(store.internal().inner_mut(), &[], &mut answer)
            .expect("a read with a task on the stack answers");
        assert!(
            matches!(answer[0], RuntimeVal::I32(0)),
            "a fresh thread's slot is zero"
        );
    }
}
