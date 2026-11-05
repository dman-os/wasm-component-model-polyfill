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
}

impl ContextSlots {
    /// The bytes of the context module. It imports `context.slot0`
    /// and `context.slot1`, mutable `i32` globals, and exports `get0`,
    /// `set0`, `get1`, and `set1`:
    ///
    /// ```text
    /// (func (export "get<N>") (result i32) (global.get $slot<N>))
    /// (func (export "set<N>") (param i32) (global.set $slot<N> (local.get 0)))
    /// ```
    fn module_bytes() -> Vec<u8> {
        use wasm_encoder::{
            CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
            ImportSection, Instruction, Module, TypeSection, ValType as Encoded,
        };

        let mut types = TypeSection::new();
        types.ty().function([], [Encoded::I32]);
        types.ty().function([Encoded::I32], []);
        let mut imports = ImportSection::new();
        let mut functions = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        for slot in 0..SLOTS as u32 {
            imports.import(
                "context",
                &format!("slot{slot}"),
                EntityType::Global(wasm_encoder::GlobalType {
                    val_type: Encoded::I32,
                    mutable: true,
                    shared: false,
                }),
            );
            functions.function(0);
            functions.function(1);
            exports.export(&format!("get{slot}"), ExportKind::Func, slot * 2);
            exports.export(&format!("set{slot}"), ExportKind::Func, slot * 2 + 1);
            let mut get = Function::new([]);
            get.instruction(&Instruction::GlobalGet(slot));
            get.instruction(&Instruction::End);
            code.function(&get);
            let mut set = Function::new([]);
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
    let mut globals = Vec::with_capacity(SLOTS);
    for _ in 0..SLOTS {
        globals.push(
            RuntimeGlobal::new(
                runtime.as_context_mut(),
                GlobalType::new(ValType::I32, Mutability::Var),
                RuntimeVal::I32(0),
            )
            .map_err(substrate_failure)?,
        );
    }
    let module = RuntimeModule::new(runtime.engine(), &ContextSlots::module_bytes())
        .map_err(substrate_failure)?;
    let mut imports = Imports::default();
    for (slot, global) in globals.iter().enumerate() {
        imports.define("context", &format!("slot{slot}"), *global);
    }
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
    };
    store.internal().lock_tables()?.context_slots = Some(slots);
    Ok(())
}

/// Make the live thread the thread that runs, as the module
/// documentation states. A store with no context module has no guest
/// that reads a slot, and a store with no task on its stack has no
/// thread to make live, so neither changes anything.
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
        return Ok(());
    };
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
