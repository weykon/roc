use bumpalo::collections::vec::Vec;
use bumpalo::Bump;
use roc_builtins::bitcode::IntWidth;
use roc_module::ident::Ident;
use roc_module::low_level::LowLevel;
use roc_module::symbol::{IdentIds, ModuleId, Symbol};

use crate::ir::{
    BranchInfo, Call, CallSpecId, CallType, Expr, HostExposedLayouts, Literal, ModifyRc, Proc,
    ProcLayout, SelfRecursive, Stmt, UpdateModeId,
};
use crate::layout::{Builtin, Layout};

const LAYOUT_BOOL: Layout = Layout::Builtin(Builtin::Bool);
const LAYOUT_UNIT: Layout = Layout::Struct(&[]);
const LAYOUT_PTR: Layout = Layout::RecursivePointer;
const LAYOUT_U32: Layout = Layout::Builtin(Builtin::Int(IntWidth::U32));

/// "Infinite" reference count, for static values
/// Ref counts are encoded as negative numbers where isize::MIN represents 1
pub const REFCOUNT_MAX: usize = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefcountOp {
    Inc,
    Dec,
    DecRef,
}

/// Generate mono IR to help with code gen
/// --------------------------------------
///
/// Some operations, such as refcounting and equality comparison, need
/// specialized helper procs to traverse data structures at runtime.
///
/// For example, when checking List equality, we need to visit each element
/// and compare them recursively. Similarly, when incrementing a List refcount,
/// we also increment the elements recursively.
/// This logic is the same for all targets, so we implement it once using mono IR.
///
/// The backend drives the process, in two steps:
/// 1) When it sees the relevant node, it calls MonoCodeGen to get the replacement IR.
///    MonoCodeGen generates a call to the helper proc, and remembers it for step 2.
/// 2) After the backend has generated all user procs, it generates the helper procs too.
///
pub struct CodeGenHelp<'a> {
    arena: &'a Bump,
    home: ModuleId,
    ptr_size: u32,
    layout_isize: Layout<'a>,
    /// List of refcounting procs to generate, specialised by Layout and RefCountOp
    /// Order of insertion is preserved, since it is important for Wasm backend
    rc_procs_to_generate: Vec<'a, (Layout<'a>, RefcountOp, Symbol)>,
}

impl<'a> CodeGenHelp<'a> {
    pub fn new(arena: &'a Bump, intwidth_isize: IntWidth, home: ModuleId) -> Self {
        CodeGenHelp {
            arena,
            home,
            ptr_size: intwidth_isize.stack_size(),
            layout_isize: Layout::Builtin(Builtin::Int(intwidth_isize)),
            rc_procs_to_generate: Vec::with_capacity_in(16, arena),
        }
    }

    /// Expands a `Refcounting` node to a `Let` node that calls a specialized helper.
    /// The helper procs themselves are to be generated later with `generate_procs`
    pub fn expand_refcount_stmt(
        &mut self,
        ident_ids: &mut IdentIds,
        layout: Layout<'a>,
        modify: &ModifyRc,
        following: &'a Stmt<'a>,
    ) -> (&'a Stmt<'a>, Option<(Symbol, ProcLayout<'a>)>) {
        if !Self::layout_is_supported(&layout) {
            // Just a warning, so we can decouple backend development from refcounting development.
            // When we are closer to completion, we can change it to a panic.
            println!(
                "WARNING! MEMORY LEAK! Refcounting not yet implemented for Layout {:?}",
                layout
            );
            return (following, None);
        }

        let arena = self.arena;

        match modify {
            ModifyRc::Inc(structure, amount) => {
                let layout_isize = self.layout_isize;

                let (is_existing, proc_name) =
                    self.get_proc_symbol(ident_ids, layout, RefcountOp::Inc);

                // Define a constant for the amount to increment
                let amount_sym = self.create_symbol(ident_ids, "amount");
                let amount_expr = Expr::Literal(Literal::Int(*amount as i128));
                let amount_stmt = |next| Stmt::Let(amount_sym, amount_expr, layout_isize, next);

                // Call helper proc, passing the Roc structure and constant amount
                let arg_layouts = arena.alloc([layout, layout_isize]);
                let call_result_empty = self.create_symbol(ident_ids, "call_result_empty");
                let call_expr = Expr::Call(Call {
                    call_type: CallType::ByName {
                        name: proc_name,
                        ret_layout: &LAYOUT_UNIT,
                        arg_layouts,
                        specialization_id: CallSpecId::BACKEND_DUMMY,
                    },
                    arguments: arena.alloc([*structure, amount_sym]),
                });
                let call_stmt = Stmt::Let(call_result_empty, call_expr, LAYOUT_UNIT, following);
                let rc_stmt = arena.alloc(amount_stmt(arena.alloc(call_stmt)));

                // Create a linker symbol for the helper proc if this is the first usage
                let new_proc_info = if is_existing {
                    None
                } else {
                    Some((
                        proc_name,
                        ProcLayout {
                            arguments: arg_layouts,
                            result: LAYOUT_UNIT,
                        },
                    ))
                };

                (rc_stmt, new_proc_info)
            }

            ModifyRc::Dec(structure) => {
                let (is_existing, proc_name) =
                    self.get_proc_symbol(ident_ids, layout, RefcountOp::Dec);

                // Call helper proc, passing the Roc structure
                let arg_layouts = arena.alloc([layout, self.layout_isize]);
                let call_result_empty = self.create_symbol(ident_ids, "call_result_empty");
                let call_expr = Expr::Call(Call {
                    call_type: CallType::ByName {
                        name: proc_name,
                        ret_layout: &LAYOUT_UNIT,
                        arg_layouts: arena.alloc([layout]),
                        specialization_id: CallSpecId::BACKEND_DUMMY,
                    },
                    arguments: arena.alloc([*structure]),
                });

                let rc_stmt = arena.alloc(Stmt::Let(
                    call_result_empty,
                    call_expr,
                    LAYOUT_UNIT,
                    following,
                ));

                // Create a linker symbol for the helper proc if this is the first usage
                let new_proc_info = if is_existing {
                    None
                } else {
                    Some((
                        proc_name,
                        ProcLayout {
                            arguments: arg_layouts,
                            result: LAYOUT_UNIT,
                        },
                    ))
                };

                (rc_stmt, new_proc_info)
            }

            ModifyRc::DecRef(structure) => {
                // No generated procs for DecRef, just lowlevel calls

                // Get a pointer to the refcount itself
                let rc_ptr_sym = self.create_symbol(ident_ids, "rc_ptr");
                let rc_ptr_expr = Expr::Call(Call {
                    call_type: CallType::LowLevel {
                        op: LowLevel::RefCountGetPtr,
                        update_mode: UpdateModeId::BACKEND_DUMMY,
                    },
                    arguments: arena.alloc([*structure]),
                });
                let rc_ptr_stmt = |next| Stmt::Let(rc_ptr_sym, rc_ptr_expr, LAYOUT_PTR, next);

                // Pass the refcount pointer to the lowlevel call (see utils.zig)
                let call_result_empty = self.create_symbol(ident_ids, "call_result_empty");
                let call_expr = Expr::Call(Call {
                    call_type: CallType::LowLevel {
                        op: LowLevel::RefCountDec,
                        update_mode: UpdateModeId::BACKEND_DUMMY,
                    },
                    arguments: arena.alloc([rc_ptr_sym]),
                });
                let call_stmt = Stmt::Let(call_result_empty, call_expr, LAYOUT_UNIT, following);
                let rc_stmt = arena.alloc(rc_ptr_stmt(arena.alloc(call_stmt)));

                (rc_stmt, None)
            }
        }
    }

    // TODO: consider refactoring so that we have just one place to define what's supported
    // (Probably by generating procs on the fly instead of all at the end)
    fn layout_is_supported(layout: &Layout) -> bool {
        matches!(layout, Layout::Builtin(Builtin::Str))
    }

    /// Generate refcounting helper procs, each specialized to a particular Layout.
    /// For example `List (Result { a: Str, b: Int } Str)` would get its own helper
    /// to update the refcounts on the List, the Result and the strings.
    pub fn generate_procs(
        &mut self,
        arena: &'a Bump,
        ident_ids: &mut IdentIds,
    ) -> Vec<'a, Proc<'a>> {
        // Move the vector out of self, so we can loop over it safely
        let mut procs_to_generate =
            std::mem::replace(&mut self.rc_procs_to_generate, Vec::with_capacity_in(0, arena));

        let procs_iter = procs_to_generate
            .drain(0..)
            .map(|(layout, op, proc_symbol)| {
                debug_assert!(Self::layout_is_supported(&layout));
                match layout {
                    Layout::Builtin(Builtin::Str) => {
                        self.gen_modify_str(ident_ids, op, proc_symbol)
                    }

                    _ => todo!("Please update layout_is_supported for {:?}", layout),
                }
            });

        Vec::from_iter_in(procs_iter, arena)
    }

    /// Find the Symbol of the procedure for this layout and refcount operation,
    /// or create one if needed.
    fn get_proc_symbol(
        &mut self,
        ident_ids: &mut IdentIds,
        layout: Layout<'a>,
        op: RefcountOp,
    ) -> (bool, Symbol) {
        let found = self
            .rc_procs_to_generate
            .iter()
            .find(|(l, o, _)| *l == layout && *o == op);

        if let Some((_, _, existing_symbol)) = found {
            (true, *existing_symbol)
        } else {
            let layout_name = layout_debug_name(&layout);
            let unique_idx = self.rc_procs_to_generate.len();
            let debug_name = format!("#rc{:?}_{}_{}", op, layout_name, unique_idx);
            let new_symbol: Symbol = self.create_symbol(ident_ids, &debug_name);
            self.rc_procs_to_generate.push((layout, op, new_symbol));
            (false, new_symbol)
        }
    }

    fn create_symbol(&self, ident_ids: &mut IdentIds, debug_name: &str) -> Symbol {
        let ident_id = ident_ids.add(Ident::from(debug_name));
        Symbol::new(self.home, ident_id)
    }

    fn return_unit(&self, ident_ids: &mut IdentIds) -> Stmt<'a> {
        let unit = self.create_symbol(ident_ids, "unit");
        let ret_stmt = self.arena.alloc(Stmt::Ret(unit));
        Stmt::Let(unit, Expr::Struct(&[]), LAYOUT_UNIT, ret_stmt)
    }

    fn gen_args(&self, op: RefcountOp, layout: Layout<'a>) -> &'a [(Layout<'a>, Symbol)] {
        let roc_value = (layout, Symbol::ARG_1);
        match op {
            RefcountOp::Inc => {
                let inc_amount = (self.layout_isize, Symbol::ARG_2);
                self.arena.alloc([roc_value, inc_amount])
            }
            RefcountOp::Dec | RefcountOp::DecRef => self.arena.alloc([roc_value]),
        }
    }

    /// Generate a procedure to modify the reference count of a Str
    fn gen_modify_str(
        &mut self,
        ident_ids: &mut IdentIds,
        op: RefcountOp,
        proc_name: Symbol,
    ) -> Proc<'a> {
        let string = Symbol::ARG_1;
        let layout_isize = self.layout_isize;

        // Get the string length as a signed int
        let len = self.create_symbol(ident_ids, "len");
        let len_expr = Expr::StructAtIndex {
            index: 1,
            field_layouts: self.arena.alloc([LAYOUT_PTR, layout_isize]),
            structure: string,
        };
        let len_stmt = |next| Stmt::Let(len, len_expr, layout_isize, next);

        // Zero
        let zero = self.create_symbol(ident_ids, "zero");
        let zero_expr = Expr::Literal(Literal::Int(0));
        let zero_stmt = |next| Stmt::Let(zero, zero_expr, layout_isize, next);

        // is_big_str = (len >= 0);
        // Treat len as isize so that the small string flag is the same as the sign bit
        let is_big_str = self.create_symbol(ident_ids, "is_big_str");
        let is_big_str_expr = Expr::Call(Call {
            call_type: CallType::LowLevel {
                op: LowLevel::NumGte,
                update_mode: UpdateModeId::BACKEND_DUMMY,
            },
            arguments: self.arena.alloc([len, zero]),
        });
        let is_big_str_stmt = |next| Stmt::Let(is_big_str, is_big_str_expr, LAYOUT_BOOL, next);

        // Get the pointer to the string elements
        let elements = self.create_symbol(ident_ids, "elements");
        let elements_expr = Expr::StructAtIndex {
            index: 0,
            field_layouts: self.arena.alloc([LAYOUT_PTR, layout_isize]),
            structure: string,
        };
        let elements_stmt = |next| Stmt::Let(elements, elements_expr, LAYOUT_PTR, next);

        // Get a pointer to the refcount value, just below the elements pointer
        let rc_ptr = self.create_symbol(ident_ids, "rc_ptr");
        let rc_ptr_expr = Expr::Call(Call {
            call_type: CallType::LowLevel {
                op: LowLevel::RefCountGetPtr,
                update_mode: UpdateModeId::BACKEND_DUMMY,
            },
            arguments: self.arena.alloc([elements]),
        });
        let rc_ptr_stmt = |next| Stmt::Let(rc_ptr, rc_ptr_expr, LAYOUT_PTR, next);

        // Alignment constant
        let alignment = self.create_symbol(ident_ids, "alignment");
        let alignment_expr = Expr::Literal(Literal::Int(self.ptr_size as i128));
        let alignment_stmt = |next| Stmt::Let(alignment, alignment_expr, LAYOUT_U32, next);

        // Call the relevant Zig lowlevel to actually modify the refcount
        let zig_call_result = self.create_symbol(ident_ids, "zig_call_result");
        let zig_call_expr = match op {
            RefcountOp::Inc => Expr::Call(Call {
                call_type: CallType::LowLevel {
                    op: LowLevel::RefCountInc,
                    update_mode: UpdateModeId::BACKEND_DUMMY,
                },
                arguments: self.arena.alloc([rc_ptr, Symbol::ARG_2]),
            }),
            RefcountOp::Dec | RefcountOp::DecRef => Expr::Call(Call {
                call_type: CallType::LowLevel {
                    op: LowLevel::RefCountDec,
                    update_mode: UpdateModeId::BACKEND_DUMMY,
                },
                arguments: self.arena.alloc([rc_ptr, alignment]),
            }),
        };
        let zig_call_stmt = |next| Stmt::Let(zig_call_result, zig_call_expr, LAYOUT_UNIT, next);

        // Generate an `if` to skip small strings but modify big strings
        let then_branch = elements_stmt(self.arena.alloc(
            //
            rc_ptr_stmt(self.arena.alloc(
                //
                alignment_stmt(self.arena.alloc(
                    //
                    zig_call_stmt(self.arena.alloc(
                        //
                        Stmt::Ret(zig_call_result),
                    )),
                )),
            )),
        ));
        let if_stmt = Stmt::Switch {
            cond_symbol: is_big_str,
            cond_layout: LAYOUT_BOOL,
            branches: self.arena.alloc([(1, BranchInfo::None, then_branch)]),
            default_branch: (
                BranchInfo::None,
                self.arena.alloc(self.return_unit(ident_ids)),
            ),
            ret_layout: LAYOUT_UNIT,
        };

        // Combine the statements in sequence
        let body = len_stmt(self.arena.alloc(
            //
            zero_stmt(self.arena.alloc(
                //
                is_big_str_stmt(self.arena.alloc(
                    //
                    if_stmt,
                )),
            )),
        ));

        let args = self.gen_args(op, Layout::Builtin(Builtin::Str));

        Proc {
            name: proc_name,
            args,
            body,
            closure_data_layout: None,
            ret_layout: LAYOUT_UNIT,
            is_self_recursive: SelfRecursive::NotSelfRecursive,
            must_own_arguments: false,
            host_exposed_layouts: HostExposedLayouts::NotHostExposed,
        }
    }
}

/// Helper to derive a debug function name from a layout
fn layout_debug_name<'a>(layout: &Layout<'a>) -> &'static str {
    match layout {
        Layout::Builtin(Builtin::List(_)) => "list",
        Layout::Builtin(Builtin::Set(_)) => "set",
        Layout::Builtin(Builtin::Dict(_, _)) => "dict",
        Layout::Builtin(Builtin::Str) => "str",
        Layout::Builtin(builtin) => {
            debug_assert!(!builtin.is_refcounted());
            unreachable!("Builtin {:?} is not refcounted", builtin);
        }
        Layout::Struct(_) => "struct",
        Layout::Union(_) => "union",
        Layout::LambdaSet(_) => "lambdaset",
        Layout::RecursivePointer => "recursive_pointer",
    }
}
