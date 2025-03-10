use crate::debug_info_init;
use crate::llvm::bitcode::call_str_bitcode_fn;
use crate::llvm::build::{get_tag_id, store_roc_value, tag_pointer_clear_tag_id, Env};
use crate::llvm::build_list::{self, incrementing_elem_loop};
use crate::llvm::convert::{basic_type_from_layout, RocUnion};
use inkwell::builder::Builder;
use inkwell::module::Linkage;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum};
use inkwell::values::{BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::AddressSpace;
use roc_builtins::bitcode;
use roc_error_macros::internal_error;
use roc_module::symbol::Symbol;
use roc_mono::ir::LookupType;
use roc_mono::layout::{
    Builtin, InLayout, Layout, LayoutIds, LayoutInterner, STLayoutInterner, UnionLayout,
};
use roc_region::all::Region;

use super::build::BuilderExt;
use super::build::{
    add_func, load_roc_value, load_symbol_and_layout, use_roc_value, FunctionSpec, LlvmBackendMode,
    Scope,
};
use super::convert::struct_type_from_union_layout;

pub(crate) struct SharedMemoryPointer<'ctx>(PointerValue<'ctx>);

impl<'ctx> SharedMemoryPointer<'ctx> {
    pub(crate) fn get<'a, 'env>(env: &Env<'a, 'ctx, 'env>) -> Self {
        let start_function = if let LlvmBackendMode::BinaryDev = env.mode {
            bitcode::UTILS_EXPECT_FAILED_START_SHARED_FILE
        } else {
            bitcode::UTILS_EXPECT_FAILED_START_SHARED_BUFFER
        };

        let func = env.module.get_function(start_function).unwrap();

        let call_result = env
            .builder
            .build_call(func, &[], "call_expect_start_failed");

        let ptr = call_result
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();

        Self(ptr)
    }
}

#[derive(Debug, Clone, Copy)]
struct Cursors<'ctx> {
    offset: IntValue<'ctx>,
    extra_offset: IntValue<'ctx>,
}

fn pointer_at_offset<'ctx>(
    bd: &Builder<'ctx>,
    element_type: impl BasicType<'ctx>,
    ptr: PointerValue<'ctx>,
    offset: IntValue<'ctx>,
) -> PointerValue<'ctx> {
    unsafe { bd.new_build_in_bounds_gep(element_type, ptr, &[offset], "offset_ptr") }
}

/// Writes the module and region into the buffer
fn write_header<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
    mut offset: IntValue<'ctx>,
    condition: Symbol,
    region: Region,
) -> IntValue<'ctx> {
    let region_start = env
        .context
        .i32_type()
        .const_int(region.start().offset as _, false);

    let region_end = env
        .context
        .i32_type()
        .const_int(region.end().offset as _, false);

    let module_id: u32 = unsafe { std::mem::transmute(condition.module_id()) };
    let module_id = env.context.i32_type().const_int(module_id as _, false);

    offset = build_copy(env, ptr, offset, region_start.into());
    offset = build_copy(env, ptr, offset, region_end.into());
    offset = build_copy(env, ptr, offset, module_id.into());

    offset
}

/// Read the first two 32-bit values from the shared memory,
/// representing the total number of expect frames and the next free position
fn read_state<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
) -> (IntValue<'ctx>, IntValue<'ctx>) {
    let ptr_type = env.ptr_int().ptr_type(AddressSpace::default());
    let ptr = env.builder.build_pointer_cast(ptr, ptr_type, "");

    let one = env.ptr_int().const_int(1, false);
    let offset_ptr = pointer_at_offset(env.builder, env.ptr_int(), ptr, one);

    let count = env.builder.new_build_load(env.ptr_int(), ptr, "load_count");
    let offset = env
        .builder
        .new_build_load(env.ptr_int(), offset_ptr, "load_offset");

    (count.into_int_value(), offset.into_int_value())
}

fn write_state<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
    count: IntValue<'ctx>,
    offset: IntValue<'ctx>,
) {
    let ptr_type = env.ptr_int().ptr_type(AddressSpace::default());
    let ptr = env.builder.build_pointer_cast(ptr, ptr_type, "");

    let one = env.ptr_int().const_int(1, false);
    let offset_ptr = pointer_at_offset(env.builder, env.ptr_int(), ptr, one);

    env.builder.build_store(ptr, count);
    env.builder.build_store(offset_ptr, offset);
}

pub(crate) fn notify_parent_expect(env: &Env, shared_memory: &SharedMemoryPointer) {
    let func = env
        .module
        .get_function(bitcode::NOTIFY_PARENT_EXPECT)
        .unwrap();

    env.builder.build_call(
        func,
        &[shared_memory.0.into()],
        "call_expect_failed_finalize",
    );
}

pub(crate) fn notify_parent_dbg(env: &Env, shared_memory: &SharedMemoryPointer) {
    let func = env.module.get_function(bitcode::NOTIFY_PARENT_DBG).unwrap();

    env.builder.build_call(
        func,
        &[shared_memory.0.into()],
        "call_expect_failed_finalize",
    );
}

// Shape of expect frame:
//
//     ===
//     Fixed-size header
//     ===
// /-- ptr_lookup_1  (ptr_size)
// |   var_lookup_1  (u32)
// |   ..
// |   ptr_lookup_n  (ptr_size)
// |   var_lookup_n  (u32)
// \-> lookup_val_1  (varsize)
//     ..
//     lookup_val_n  (varsize)
//
pub(crate) fn clone_to_shared_memory<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    scope: &Scope<'a, 'ctx>,
    layout_ids: &mut LayoutIds<'a>,
    shared_memory: &SharedMemoryPointer<'ctx>,
    condition: Symbol,
    region: Region,
    lookups: &[Symbol],
    lookup_variables: &[LookupType],
) {
    let original_ptr = shared_memory.0;

    let (count, mut offset) = read_state(env, original_ptr);

    offset = write_header(env, original_ptr, offset, condition, region);

    let after_header = offset;

    let space_for_offsets = env.ptr_int().const_int(
        (lookups.len() * env.target_info.ptr_size() + lookups.len() * std::mem::size_of::<u32>())
            as _,
        false,
    );

    let mut lookup_starts = bumpalo::collections::Vec::with_capacity_in(lookups.len(), env.arena);

    offset = env
        .builder
        .build_int_add(offset, space_for_offsets, "offset");

    for lookup in lookups.iter() {
        lookup_starts.push(offset);

        let (value, layout) = load_symbol_and_layout(scope, lookup);

        let stack_size = env
            .ptr_int()
            .const_int(layout_interner.stack_size(layout) as u64, false);

        let mut extra_offset = env.builder.build_int_add(offset, stack_size, "offset");

        let cursors = Cursors {
            offset,
            extra_offset,
        };

        extra_offset = build_clone(
            env,
            layout_interner,
            layout_ids,
            original_ptr,
            cursors,
            value,
            layout,
        );

        offset = extra_offset;
    }

    {
        let mut offset = after_header;

        for (lookup_start, lookup_var) in lookup_starts.into_iter().zip(lookup_variables) {
            // Store the pointer to the value
            {
                build_copy(env, original_ptr, offset, lookup_start.into());

                let ptr_width = env
                    .ptr_int()
                    .const_int(env.target_info.ptr_size() as _, false);

                offset = env.builder.build_int_add(offset, ptr_width, "offset");
            }

            // Store the specialized variable of the value
            {
                let ptr = unsafe {
                    env.builder.new_build_in_bounds_gep(
                        env.context.i8_type(),
                        original_ptr,
                        &[offset],
                        "at_current_offset",
                    )
                };

                let u32_ptr = env.context.i32_type().ptr_type(AddressSpace::default());
                let ptr = env
                    .builder
                    .build_pointer_cast(ptr, u32_ptr, "cast_ptr_type");

                let var_value = env
                    .context
                    .i32_type()
                    .const_int(lookup_var.index() as _, false);

                env.builder.build_store(ptr, var_value);

                let var_size = env
                    .ptr_int()
                    .const_int(std::mem::size_of::<u32>() as _, false);

                offset = env.builder.build_int_add(offset, var_size, "offset");
            }
        }
    }

    let one = env.ptr_int().const_int(1, false);
    let new_count = env.builder.build_int_add(count, one, "inc");
    write_state(env, original_ptr, new_count, offset)
}

fn build_clone<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    layout_ids: &mut LayoutIds<'a>,
    ptr: PointerValue<'ctx>,
    cursors: Cursors<'ctx>,
    value: BasicValueEnum<'ctx>,
    layout: InLayout<'a>,
) -> IntValue<'ctx> {
    match layout_interner.get(layout) {
        Layout::Builtin(builtin) => build_clone_builtin(
            env,
            layout_interner,
            layout_ids,
            ptr,
            cursors,
            value,
            builtin,
        ),

        Layout::Struct { field_layouts, .. } => build_clone_struct(
            env,
            layout_interner,
            layout_ids,
            ptr,
            cursors,
            value,
            field_layouts,
        ),

        // Since we will never actually display functions (and hence lambda sets)
        // we just write nothing to the buffer
        Layout::LambdaSet(_) => cursors.extra_offset,

        Layout::Union(union_layout) => {
            if layout_interner.safe_to_memcpy(layout) {
                let ptr = unsafe {
                    env.builder.new_build_in_bounds_gep(
                        env.context.i8_type(),
                        ptr,
                        &[cursors.offset],
                        "at_current_offset",
                    )
                };

                let ptr_type = value.get_type().ptr_type(AddressSpace::default());
                let ptr = env
                    .builder
                    .build_pointer_cast(ptr, ptr_type, "cast_ptr_type");

                store_roc_value(env, layout_interner, layout, ptr, value);

                cursors.extra_offset
            } else {
                build_clone_tag(
                    env,
                    layout_interner,
                    layout_ids,
                    ptr,
                    cursors,
                    value,
                    union_layout,
                )
            }
        }

        Layout::Boxed(inner_layout) => {
            // write the offset
            build_copy(env, ptr, cursors.offset, cursors.extra_offset.into());

            let source = value.into_pointer_value();
            let value = load_roc_value(env, layout_interner, inner_layout, source, "inner");

            let inner_width = env
                .ptr_int()
                .const_int(layout_interner.stack_size(inner_layout) as u64, false);

            let new_extra = env
                .builder
                .build_int_add(cursors.offset, inner_width, "new_extra");

            let cursors = Cursors {
                offset: cursors.extra_offset,
                extra_offset: new_extra,
            };

            build_clone(
                env,
                layout_interner,
                layout_ids,
                ptr,
                cursors,
                value,
                inner_layout,
            )
        }

        Layout::RecursivePointer(rec_layout) => {
            let layout = rec_layout;

            let bt = basic_type_from_layout(env, layout_interner, layout);

            // cast the i64 pointer to a pointer to block of memory
            let field1_cast = env.builder.build_pointer_cast(
                value.into_pointer_value(),
                bt.into_pointer_type(),
                "i64_to_opaque",
            );

            let union_layout = match layout_interner.get(rec_layout) {
                Layout::Union(union_layout) => {
                    debug_assert!(!matches!(union_layout, UnionLayout::NonRecursive(..)));
                    union_layout
                }
                _ => internal_error!(),
            };

            build_clone_tag(
                env,
                layout_interner,
                layout_ids,
                ptr,
                cursors,
                field1_cast.into(),
                union_layout,
            )
        }
    }
}

fn build_clone_struct<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    layout_ids: &mut LayoutIds<'a>,
    ptr: PointerValue<'ctx>,
    cursors: Cursors<'ctx>,
    value: BasicValueEnum<'ctx>,
    field_layouts: &[InLayout<'a>],
) -> IntValue<'ctx> {
    let layout = Layout::struct_no_name_order(field_layouts);

    if layout.safe_to_memcpy(layout_interner) {
        build_copy(env, ptr, cursors.offset, value)
    } else {
        let mut cursors = cursors;

        let structure = value.into_struct_value();

        for (i, field_layout) in field_layouts.iter().enumerate() {
            let field = env
                .builder
                .build_extract_value(structure, i as _, "extract")
                .unwrap();

            let field = use_roc_value(env, layout_interner, *field_layout, field, "field");

            let new_extra = build_clone(
                env,
                layout_interner,
                layout_ids,
                ptr,
                cursors,
                field,
                *field_layout,
            );

            let field_width = env
                .ptr_int()
                .const_int(layout_interner.stack_size(*field_layout) as u64, false);

            cursors.extra_offset = new_extra;
            cursors.offset = env
                .builder
                .build_int_add(cursors.offset, field_width, "offset");
        }

        cursors.extra_offset
    }
}

fn build_clone_tag<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    layout_ids: &mut LayoutIds<'a>,
    ptr: PointerValue<'ctx>,
    cursors: Cursors<'ctx>,
    value: BasicValueEnum<'ctx>,
    union_layout: UnionLayout<'a>,
) -> IntValue<'ctx> {
    let layout = layout_interner.insert(Layout::Union(union_layout));
    let layout_id = layout_ids.get(Symbol::CLONE, &layout);
    let fn_name = layout_id.to_symbol_string(Symbol::CLONE, &env.interns);

    let function = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let block = env.builder.get_insert_block().expect("to be in a function");
            let di_location = env.builder.get_current_debug_location().unwrap();

            let function_type = env.ptr_int().fn_type(
                &[
                    env.context
                        .i8_type()
                        .ptr_type(AddressSpace::default())
                        .into(),
                    env.ptr_int().into(),
                    env.ptr_int().into(),
                    BasicMetadataTypeEnum::from(value.get_type()),
                ],
                false,
            );

            let function_value = add_func(
                env.context,
                env.module,
                &fn_name,
                FunctionSpec::known_fastcc(function_type),
                Linkage::Private,
            );

            let subprogram = env.new_subprogram(&fn_name);
            function_value.set_subprogram(subprogram);

            env.dibuilder.finalize();

            build_clone_tag_help(
                env,
                layout_interner,
                layout_ids,
                union_layout,
                function_value,
            );

            env.builder.position_at_end(block);
            env.builder.set_current_debug_location(di_location);

            function_value
        }
    };

    let call = env.builder.build_call(
        function,
        &[
            ptr.into(),
            cursors.offset.into(),
            cursors.extra_offset.into(),
            value.into(),
        ],
        "build_clone_tag",
    );

    call.set_call_convention(function.get_call_conventions());

    let result = call.try_as_basic_value().left().unwrap();

    result.into_int_value()
}

fn load_tag_data<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    union_layout: UnionLayout<'a>,
    tag_value: PointerValue<'ctx>,
    tag_type: BasicTypeEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    let union_struct_type = struct_type_from_union_layout(env, layout_interner, &union_layout);

    let raw_data_ptr = env
        .builder
        .new_build_struct_gep(
            union_struct_type,
            tag_value,
            RocUnion::TAG_DATA_INDEX,
            "tag_data",
        )
        .unwrap();

    let data_ptr = env.builder.build_pointer_cast(
        raw_data_ptr,
        tag_type.ptr_type(AddressSpace::default()),
        "data_ptr",
    );

    env.builder.new_build_load(tag_type, data_ptr, "load_data")
}

fn build_clone_tag_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    layout_ids: &mut LayoutIds<'a>,
    union_layout: UnionLayout<'a>,
    fn_val: FunctionValue<'ctx>,
) {
    use bumpalo::collections::Vec;

    let context = &env.context;
    let builder = env.builder;

    // Add a basic block for the entry point
    let entry = context.append_basic_block(fn_val, "entry");

    builder.position_at_end(entry);

    debug_info_init!(env, fn_val);

    // Add args to scope
    // let arg_symbol = Symbol::ARG_1;
    // tag_value.set_name(arg_symbol.as_str(&env.interns));

    let mut it = fn_val.get_param_iter();

    let ptr = it.next().unwrap().into_pointer_value();
    let offset = it.next().unwrap().into_int_value();
    let extra_offset = it.next().unwrap().into_int_value();
    let tag_value = it.next().unwrap();

    let cursors = Cursors {
        offset,
        extra_offset,
    };

    let parent = fn_val;

    debug_assert!(tag_value.is_pointer_value());

    use UnionLayout::*;

    match union_layout {
        NonRecursive(&[]) => {
            // we're comparing empty tag unions; this code is effectively unreachable
            env.builder.build_unreachable();
        }
        NonRecursive(tags) => {
            let id = get_tag_id(env, layout_interner, parent, &union_layout, tag_value);

            let switch_block = env.context.append_basic_block(parent, "switch_block");
            env.builder.build_unconditional_branch(switch_block);

            let mut cases = Vec::with_capacity_in(tags.len(), env.arena);

            for (tag_id, field_layouts) in tags.iter().enumerate() {
                let block = env.context.append_basic_block(parent, "tag_id_modify");
                env.builder.position_at_end(block);

                let layout = layout_interner.insert(Layout::struct_no_name_order(field_layouts));
                let layout = layout_interner.insert(Layout::struct_no_name_order(
                    env.arena.alloc([layout, union_layout.tag_id_layout()]),
                ));

                let basic_type = basic_type_from_layout(env, layout_interner, layout);
                let data = load_tag_data(
                    env,
                    layout_interner,
                    union_layout,
                    tag_value.into_pointer_value(),
                    basic_type,
                );

                let answer =
                    build_clone(env, layout_interner, layout_ids, ptr, cursors, data, layout);

                env.builder.build_return(Some(&answer));

                cases.push((id.get_type().const_int(tag_id as u64, false), block));
            }

            env.builder.position_at_end(switch_block);

            match cases.pop() {
                Some((_, default)) => {
                    env.builder.build_switch(id, default, &cases);
                }
                None => {
                    // we're serializing an empty tag union; this code is effectively unreachable
                    env.builder.build_unreachable();
                }
            }
        }
        Recursive(tags) => {
            let id = get_tag_id(env, layout_interner, parent, &union_layout, tag_value);

            let switch_block = env.context.append_basic_block(parent, "switch_block");
            env.builder.build_unconditional_branch(switch_block);

            let mut cases = Vec::with_capacity_in(tags.len(), env.arena);

            for (tag_id, field_layouts) in tags.iter().enumerate() {
                let block = env.context.append_basic_block(parent, "tag_id_modify");
                env.builder.position_at_end(block);

                // write the "pointer" of the current offset
                write_pointer_with_tag_id(env, ptr, offset, extra_offset, union_layout, tag_id);

                let tag_value = tag_pointer_clear_tag_id(env, tag_value.into_pointer_value());

                let layout = layout_interner.insert(Layout::struct_no_name_order(field_layouts));
                let layout = if union_layout.stores_tag_id_in_pointer(env.target_info) {
                    layout
                } else {
                    layout_interner.insert(Layout::struct_no_name_order(
                        env.arena.alloc([layout, union_layout.tag_id_layout()]),
                    ))
                };

                let basic_type = basic_type_from_layout(env, layout_interner, layout);
                let data = load_tag_data(env, layout_interner, union_layout, tag_value, basic_type);

                let (width, _) =
                    union_layout.data_size_and_alignment(layout_interner, env.target_info);

                let cursors = Cursors {
                    offset: extra_offset,
                    extra_offset: env.builder.build_int_add(
                        extra_offset,
                        env.ptr_int().const_int(width as _, false),
                        "new_offset",
                    ),
                };

                let answer =
                    build_clone(env, layout_interner, layout_ids, ptr, cursors, data, layout);

                env.builder.build_return(Some(&answer));

                cases.push((id.get_type().const_int(tag_id as u64, false), block));
            }

            env.builder.position_at_end(switch_block);

            match cases.pop() {
                Some((_, default)) => {
                    env.builder.build_switch(id, default, &cases);
                }
                None => {
                    // we're serializing an empty tag union; this code is effectively unreachable
                    env.builder.build_unreachable();
                }
            }
        }
        NonNullableUnwrapped(fields) => {
            let tag_value = tag_value.into_pointer_value();

            build_copy(env, ptr, offset, extra_offset.into());

            let layout = layout_interner.insert(Layout::struct_no_name_order(fields));
            let basic_type = basic_type_from_layout(env, layout_interner, layout);

            let (width, _) = union_layout.data_size_and_alignment(layout_interner, env.target_info);

            let cursors = Cursors {
                offset: extra_offset,
                extra_offset: env.builder.build_int_add(
                    extra_offset,
                    env.ptr_int().const_int(width as _, false),
                    "new_offset",
                ),
            };

            let data = load_tag_data(env, layout_interner, union_layout, tag_value, basic_type);

            let answer = build_clone(env, layout_interner, layout_ids, ptr, cursors, data, layout);

            env.builder.build_return(Some(&answer));
        }
        NullableWrapped {
            nullable_id,
            other_tags,
        } => {
            let switch_block = env.context.append_basic_block(parent, "switch_block");
            let null_block = env.context.append_basic_block(parent, "null_block");

            let id = get_tag_id(env, layout_interner, parent, &union_layout, tag_value);

            let comparison = env
                .builder
                .build_is_null(tag_value.into_pointer_value(), "is_null");

            env.builder
                .build_conditional_branch(comparison, null_block, switch_block);

            {
                let mut cases = Vec::with_capacity_in(other_tags.len(), env.arena);

                for i in 0..other_tags.len() + 1 {
                    if i == nullable_id as _ {
                        continue;
                    }

                    let block = env.context.append_basic_block(parent, "tag_id_modify");
                    env.builder.position_at_end(block);

                    // write the "pointer" of the current offset
                    write_pointer_with_tag_id(env, ptr, offset, extra_offset, union_layout, i);

                    let fields = if i >= nullable_id as _ {
                        other_tags[i - 1]
                    } else {
                        other_tags[i]
                    };

                    let layout = layout_interner.insert(Layout::struct_no_name_order(fields));
                    let basic_type = basic_type_from_layout(env, layout_interner, layout);

                    let (width, _) =
                        union_layout.data_size_and_alignment(layout_interner, env.target_info);

                    let cursors = Cursors {
                        offset: extra_offset,
                        extra_offset: env.builder.build_int_add(
                            extra_offset,
                            env.ptr_int().const_int(width as _, false),
                            "new_offset",
                        ),
                    };

                    let tag_value = tag_pointer_clear_tag_id(env, tag_value.into_pointer_value());
                    let data =
                        load_tag_data(env, layout_interner, union_layout, tag_value, basic_type);

                    let answer =
                        build_clone(env, layout_interner, layout_ids, ptr, cursors, data, layout);

                    env.builder.build_return(Some(&answer));

                    cases.push((id.get_type().const_int(i as u64, false), block));
                }

                env.builder.position_at_end(switch_block);

                match cases.pop() {
                    Some((_, default)) => {
                        env.builder.build_switch(id, default, &cases);
                    }
                    None => {
                        // we're serializing an empty tag union; this code is effectively unreachable
                        env.builder.build_unreachable();
                    }
                }
            }

            {
                env.builder.position_at_end(null_block);

                let value = env.ptr_int().const_zero();
                build_copy(env, ptr, offset, value.into());

                env.builder.build_return(Some(&extra_offset));
            }
        }
        NullableUnwrapped { other_fields, .. } => {
            let other_block = env.context.append_basic_block(parent, "other_block");
            let null_block = env.context.append_basic_block(parent, "null_block");

            let comparison = env
                .builder
                .build_is_null(tag_value.into_pointer_value(), "is_null");

            env.builder
                .build_conditional_branch(comparison, null_block, other_block);

            {
                env.builder.position_at_end(null_block);

                let value = env.ptr_int().const_zero();
                build_copy(env, ptr, offset, value.into());

                env.builder.build_return(Some(&extra_offset));
            }

            {
                env.builder.position_at_end(other_block);

                // write the "pointer" af the current offset
                build_copy(env, ptr, offset, extra_offset.into());

                let layout = layout_interner.insert(Layout::struct_no_name_order(other_fields));
                let basic_type = basic_type_from_layout(env, layout_interner, layout);

                let cursors = Cursors {
                    offset: extra_offset,
                    extra_offset: env.builder.build_int_add(
                        extra_offset,
                        env.ptr_int()
                            .const_int(layout_interner.stack_size(layout) as _, false),
                        "new_offset",
                    ),
                };

                let data = load_tag_data(
                    env,
                    layout_interner,
                    union_layout,
                    tag_value.into_pointer_value(),
                    basic_type,
                );

                let answer =
                    build_clone(env, layout_interner, layout_ids, ptr, cursors, data, layout);

                env.builder.build_return(Some(&answer));
            }
        }
    }
}

fn write_pointer_with_tag_id<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
    offset: IntValue<'ctx>,
    extra_offset: IntValue<'ctx>,
    union_layout: UnionLayout<'a>,
    tag_id: usize,
) {
    if union_layout.stores_tag_id_in_pointer(env.target_info) {
        // first, store tag id as u32
        let tag_id_intval = env.context.i32_type().const_int(tag_id as _, false);
        build_copy(env, ptr, offset, tag_id_intval.into());

        // increment offset by 4
        let four = env.ptr_int().const_int(4, false);
        let offset = env.builder.build_int_add(offset, four, "");

        // cast to u32
        let extra_offset = env
            .builder
            .build_int_cast(extra_offset, env.context.i32_type(), "");

        build_copy(env, ptr, offset, extra_offset.into());
    } else {
        build_copy(env, ptr, offset, extra_offset.into());
    }
}

fn build_copy<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
    offset: IntValue<'ctx>,
    value: BasicValueEnum<'ctx>,
) -> IntValue<'ctx> {
    let ptr = unsafe {
        env.builder.new_build_in_bounds_gep(
            env.context.i8_type(),
            ptr,
            &[offset],
            "at_current_offset",
        )
    };

    let ptr_type = value.get_type().ptr_type(AddressSpace::default());
    let ptr = env
        .builder
        .build_pointer_cast(ptr, ptr_type, "cast_ptr_type");

    env.builder.build_store(ptr, value);

    let width = value.get_type().size_of().unwrap();
    env.builder.build_int_add(offset, width, "new_offset")
}

fn build_clone_builtin<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_interner: &mut STLayoutInterner<'a>,
    layout_ids: &mut LayoutIds<'a>,
    ptr: PointerValue<'ctx>,
    cursors: Cursors<'ctx>,
    value: BasicValueEnum<'ctx>,
    builtin: Builtin<'a>,
) -> IntValue<'ctx> {
    use Builtin::*;

    match builtin {
        Int(_) | Float(_) | Bool | Decimal => {
            build_copy(env, ptr, cursors.offset, value);

            cursors.extra_offset
        }

        Builtin::Str => {
            //

            call_str_bitcode_fn(
                env,
                &[value],
                &[
                    ptr.into(),
                    cursors.offset.into(),
                    cursors.extra_offset.into(),
                ],
                crate::llvm::bitcode::BitcodeReturns::Basic,
                bitcode::STR_CLONE_TO,
            )
            .into_int_value()
        }
        Builtin::List(elem) => {
            let bd = env.builder;

            let list = value.into_struct_value();
            let (elements, len, _cap) = build_list::destructure(env.builder, list);

            let mut offset = cursors.offset;

            // we only copy the elements we actually have (and skip extra capacity)
            offset = build_copy(env, ptr, offset, cursors.extra_offset.into());
            offset = build_copy(env, ptr, offset, len.into());
            offset = build_copy(env, ptr, offset, len.into());

            let (element_width, _element_align) = layout_interner.stack_size_and_alignment(elem);
            let element_width = env.ptr_int().const_int(element_width as _, false);

            let elements_width = bd.build_int_mul(element_width, len, "elements_width");

            // We clone the elements into the extra_offset address.
            let _ = offset;
            let elements_start_offset = cursors.extra_offset;

            if layout_interner.safe_to_memcpy(elem) {
                // NOTE we are not actually sure the dest is properly aligned
                let dest = pointer_at_offset(bd, env.context.i8_type(), ptr, elements_start_offset);
                let src = bd.build_pointer_cast(
                    elements,
                    env.context.i8_type().ptr_type(AddressSpace::default()),
                    "to_bytes_pointer",
                );
                bd.build_memcpy(dest, 1, src, 1, elements_width).unwrap();

                bd.build_int_add(elements_start_offset, elements_width, "new_offset")
            } else {
                let element_type = basic_type_from_layout(env, layout_interner, elem);
                let elements = bd.build_pointer_cast(
                    elements,
                    element_type.ptr_type(AddressSpace::default()),
                    "elements",
                );

                // if the element has any pointers, we clone them to this offset
                let rest_offset = bd.build_alloca(env.ptr_int(), "rest_offset");

                let element_stack_size = env
                    .ptr_int()
                    .const_int(layout_interner.stack_size(elem) as u64, false);
                let rest_start_offset = bd.build_int_add(
                    cursors.extra_offset,
                    bd.build_int_mul(len, element_stack_size, "elements_width"),
                    "rest_start_offset",
                );
                bd.build_store(rest_offset, rest_start_offset);

                let body = |layout_interner, index, element| {
                    let current_offset =
                        bd.build_int_mul(element_stack_size, index, "current_offset");
                    let current_offset =
                        bd.build_int_add(elements_start_offset, current_offset, "current_offset");
                    let current_extra_offset =
                        bd.new_build_load(env.ptr_int(), rest_offset, "element_offset");

                    let offset = current_offset;
                    let extra_offset = current_extra_offset.into_int_value();

                    let cursors = Cursors {
                        offset,
                        extra_offset,
                    };

                    let new_offset = build_clone(
                        env,
                        layout_interner,
                        layout_ids,
                        ptr,
                        cursors,
                        element,
                        elem,
                    );

                    bd.build_store(rest_offset, new_offset);
                };

                let parent = env
                    .builder
                    .get_insert_block()
                    .and_then(|b| b.get_parent())
                    .unwrap();

                incrementing_elem_loop(
                    env,
                    layout_interner,
                    parent,
                    elem,
                    elements,
                    len,
                    "index",
                    body,
                );

                bd.new_build_load(env.ptr_int(), rest_offset, "rest_start_offset")
                    .into_int_value()
            }
        }
    }
}
