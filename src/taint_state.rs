use crate::config::{self, Config};
use crate::function_summary::FunctionSummary;
use crate::function_taint_state::FunctionTaintState;
use crate::globals::Globals;
use crate::modules::Modules;
use crate::named_structs::{Index, NamedStructs, NamedStructInitialDef};
use crate::taint_result::TaintResult;
use crate::tainted_type::TaintedType;
use crate::worklist::Worklist;
use either::Either;
use itertools::Itertools;
use llvm_ir::instruction::{groups, BinaryOp, HasResult, UnaryOp};
use llvm_ir::*;
use llvm_ir_analysis::CrossModuleAnalysis;
use log::debug;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::convert::TryInto;
use std::iter::FromIterator;
use std::rc::Rc;

pub(crate) struct TaintState<'m> {
    /// `CrossModuleAnalysis` for the llvm-ir `Module`(s) we're analyzing
    analysis: CrossModuleAnalysis<'m>,

    /// The configuration for the analysis
    config: &'m Config,

    /// The `FunctionTaintState`s we're working with
    fn_taint_states: FunctionTaintStates<'m>,

    /// Map from function name to the `FunctionSummary` for that function
    fn_summaries: HashMap<&'m str, FunctionSummary<'m>>,

    /// Named structs used in the module(s), and their definitions (taint statuses)
    named_structs: Rc<RefCell<NamedStructs<'m>>>,

    /// Globals used in the module(s), and their definitions (taint statuses)
    globals: Rc<RefCell<Globals<'m>>>,

    /// Set of functions which need to be processed again because there's been a
    /// change to taint information which might be relevant to them
    worklist: Rc<RefCell<Worklist<'m>>>,

    /// Name of the function currently being processed
    cur_fn: &'m str,

    /// Module of the function currently being processed
    cur_mod: &'m Module,

    /// Name of the block currently being processed, if any
    cur_block: Option<&'m Name>,
}

/// Owns all of the `FunctionTaintState`s which we're working with
///
/// To create one of these, use `.collect()` --- see the `FromIterator`
/// implementation below
struct FunctionTaintStates<'m> {
    /// Map from function name to the `FunctionTaintState` for that function
    map: HashMap<&'m str, FunctionTaintState<'m>>,

    /// Name of the function currently being processed
    cur_fn: &'m str,
}

impl<'m> FunctionTaintStates<'m> {
    /// Get the `FunctionTaintState` for the current function, panicking if one
    /// does not already exist.
    ///
    /// Be sure to have set the current function properly, with
    /// `set_current_fn()`.
    fn get_current(&mut self) -> &mut FunctionTaintState<'m> {
        let cur_fn = self.cur_fn;
        self.map.get_mut(cur_fn).unwrap_or_else(|| {
            panic!("no taint state found for current function {:?}", cur_fn)
        })
    }

    /// Get the `FunctionTaintState` for the current function, or if one does
    /// not exist, use the given closure to create one for it first.
    fn get_current_or_insert_with(&mut self, f: impl FnOnce() -> FunctionTaintState<'m>) -> &mut FunctionTaintState<'m> {
        self.map.entry(self.cur_fn).or_insert_with(f)
    }

    /// Set the current function name
    fn set_current_fn(&mut self, fn_name: &'m str) {
        self.cur_fn = fn_name;
    }
}

impl<'m> FromIterator<(&'m str, FunctionTaintState<'m>)> for FunctionTaintStates<'m> {
    fn from_iter<T>(iter: T) -> Self
        where T: IntoIterator<Item = (&'m str, FunctionTaintState<'m>)>
    {
        Self {
            map: iter.into_iter().collect(),
            cur_fn: "", // must call `set_current_fn()` before `get_current()`
        }
    }
}

impl<'m> TaintState<'m> {
    /// Compute the tainted state of all variables using our fixpoint algorithm,
    /// and return the resulting `TaintState`.
    ///
    /// `start_fn_name`: name of the function to start the analysis in
    pub fn do_analysis_single_function(
        modules: impl IntoIterator<Item = &'m Module>,
        config: &'m Config,
        start_fn_name: &str,
        args: Option<Vec<TaintedType>>,
        nonargs: HashMap<Name, TaintedType>,
        named_structs: HashMap<String, NamedStructInitialDef>,
    ) -> Self {
        let modules: Modules<'m> = modules.into_iter().collect();
        let analysis = CrossModuleAnalysis::new(modules.iter());
        let (f, _) = analysis.get_func_by_name(start_fn_name).unwrap_or_else(|| {
            panic!(
                "Failed to find function named {:?} in the given module(s)",
                start_fn_name
            )
        });
        let mut initial_taintmap = nonargs;
        if let Some(args) = args {
            for (name, ty) in f
                .parameters
                .iter()
                .map(|p| p.name.clone())
                .zip_eq(args.into_iter())
            {
                initial_taintmap.insert(name, ty);
            }
        }

        let fn_taint_maps = std::iter::once((f.name.as_str(), initial_taintmap)).collect();
        let mut ts = Self::new(modules, analysis, config, std::iter::once(f.name.as_str()).collect(), fn_taint_maps, named_structs);
        ts.compute();
        ts
    }

    /// Compute the tainted state of all variables using our fixpoint algorithm,
    /// and return the resulting `TaintState`.
    ///
    /// `start_fns`: name of the functions to start the analysis in
    ///
    /// `fn_taint_maps`: Map from LLVM function name to a map from variable name
    /// to the initial `TaintedType` of that variable. Any variable not included
    /// in one of these maps will simply be inferred normally from the other
    /// variables.
    pub fn do_analysis_multiple_functions(
        modules: impl IntoIterator<Item = &'m Module>,
        config: &'m Config,
        args: HashMap<&'m str, Vec<TaintedType>>,
        nonargs: HashMap<&'m str, HashMap<Name, TaintedType>>,
        named_structs: HashMap<String, NamedStructInitialDef>,
    ) -> Self {
        let modules: Modules<'m> = modules.into_iter().collect();
        let analysis = CrossModuleAnalysis::new(modules.iter());
        let mut initial_fn_taint_maps = nonargs;
        for (funcname, argtypes) in args.into_iter() {
            let (func, _) = analysis.get_func_by_name(&funcname).unwrap_or_else(|| {
                panic!(
                    "Failed to find function named {:?} in the given module(s)",
                    funcname
                );
            });
            let initial_fn_taint_map: &mut HashMap<Name, TaintedType> = initial_fn_taint_maps.entry(funcname.clone()).or_default();
            for (name, ty) in func.parameters.iter().map(|p| p.name.clone()).zip_eq(argtypes.into_iter()) {
                initial_fn_taint_map.insert(name, ty);
            }
        }
        let all_fns = modules.all_functions().map(|(f, _)| f.name.as_str());
        let initial_worklist: Worklist<'m> = all_fns.collect();
        let mut ts = Self::new(modules, analysis, config, initial_worklist, initial_fn_taint_maps, named_structs);
        ts.compute();
        ts
    }

    fn new(
        modules: Modules<'m>,
        analysis: CrossModuleAnalysis<'m>,
        config: &'m Config,
        initial_worklist: Worklist<'m>,
        fn_taint_maps: HashMap<&'m str, HashMap<Name, TaintedType>>,
        named_structs: HashMap<String, NamedStructInitialDef>,
    ) -> Self {
        let cur_mod = modules.iter().next().unwrap(); // doesn't matter what `cur_mod` starts as - we shouldn't use it until we set `cur_fn` and `cur_mod` together
        let named_structs = Rc::new(RefCell::new(NamedStructs::with_initial_defs(modules, named_structs)));
        let globals = Rc::new(RefCell::new(Globals::new()));
        let worklist = Rc::new(RefCell::new(initial_worklist));
        let fn_taint_states = fn_taint_maps
            .into_iter()
            .map(|(s, taintmap)| {
                let (_, module) = analysis.get_func_by_name(s).expect("Function named {:?} not found");
                let fts = FunctionTaintState::from_taint_map(
                    &s,
                    taintmap,
                    module,
                    Rc::clone(&named_structs),
                    Rc::clone(&globals),
                    Rc::clone(&worklist),
                );
                (s.into(), fts)
            })
            .collect();
        Self {
            analysis,
            config,
            fn_taint_states,
            fn_summaries: HashMap::new(),
            named_structs,
            globals,
            worklist,
            cur_fn: "", // we shouldn't use `cur_fn` until it's set to the first one we pop off the worklist
            cur_mod, // likewise, we shouldn't use `cur_mod` until we set `cur_fn`
            cur_block: None,
        }
    }

    pub(crate) fn into_taint_result(self) -> TaintResult<'m> {
        TaintResult {
            fn_taint_states: self.fn_taint_states.map,
            named_struct_types: self
                .named_structs
                .borrow()
                .all_named_struct_types()
                .map(|(name, ty)| (name.clone(), ty.clone()))
                .collect(),
        }
    }

    /// Run the fixpoint algorithm to completion.
    fn compute(&mut self) {
        // We use a worklist fixpoint algorithm where `self.worklist` contains
        // names of functions which need another pass because of changes made to
        // the `TaintedType` of variables that may affect that function's analysis.
        //
        // Within a function, we simply do a pass over all instructions in the
        // function. More sophisticated would be an instruction-level worklist
        // approach, but that would require having instruction dependency
        // information so that we know what things to put on the worklist when a
        // given variable's taint changes.
        //
        // In either case, this is guaranteed to converge because we only ever
        // change things from untainted to tainted. In the limit, everything becomes
        // tainted, and then nothing can change so the algorithm must terminate.
        let mut iter_ctr = 0;
        loop {
            let fn_name = match self.worklist.borrow_mut().pop() {
                Some(fn_name) => fn_name,
                None => break,
            };
            debug!("Popped {:?} from worklist", fn_name);
            let changed = match self.analysis.get_func_by_name(fn_name) {
                Some((func, module)) => {
                    // internal function (defined in one of the available modules):
                    // process it normally
                    self
                        .process_function(func, module)
                        .unwrap_or_else(|e| panic!("In module {:?}:\nin function {:?}:\n{}", &module.name, fn_name, e))
                },
                None => {
                    // external function (not defined in the current module):
                    // see how we're configured to handle this function
                    use config::ExternalFunctionHandling;
                    let handling = self.config.ext_functions.get(fn_name).unwrap_or(&self.config.ext_functions_default);
                    match handling {
                        ExternalFunctionHandling::IgnoreAndReturnUntainted => {
                            // no need to do anything
                            false
                        },
                        ExternalFunctionHandling::IgnoreAndReturnTainted => {
                            // mark the return value tainted, if it wasn't already.
                            // we require that anyone who places an external
                            // function on the worklist is responsible for
                            // making sure it has at least a default summary in
                            // place, so we can assume here that there is a
                            // summary
                            let summary = self.fn_summaries.get_mut(fn_name).unwrap_or_else(|| panic!("Internal invariant violated: External function {:?} on the worklist has no summary", fn_name));
                            summary.taint_ret()
                        },
                        ExternalFunctionHandling::PropagateTaintShallow => {
                            // again, we require that anyone who places an
                            // external function on the worklist is responsible
                            // for making sure it has at least a default summary
                            // in place, so we can assume here that there is a
                            // summary
                            let summary = self.fn_summaries.get_mut(fn_name).unwrap_or_else(|| panic!("Internal invariant violated: External function {:?} on the worklist has no summary", fn_name));
                            // we effectively inline self.is_type_tainted(), in order to prove to the borrow checker that `summary` borrows a different part of `self` than we need for `is_type_tainted()`
                            let mut named_structs = self.named_structs.borrow_mut();
                            let cur_fn = self.cur_fn;
                            if summary.get_params().any(|p| named_structs.is_type_tainted(p, cur_fn)) {
                                summary.taint_ret()
                            } else {
                                // no need to do anything, just like the IgnoreAndReturnUntainted case
                                false
                            }
                        },
                        ExternalFunctionHandling::PropagateTaintDeep => {
                            unimplemented!("ExternalFunctionHandling::PropagateTaintDeep")
                        },
                        ExternalFunctionHandling::Panic => {
                            panic!("Call of a function named {:?} not found in the module", fn_name)
                        },
                    }
                },
            };
            if changed {
                iter_ctr += 1;
                if iter_ctr >= 8 {
                    panic!("Infinite analysis");
                }
                self.worklist.borrow_mut().add(fn_name);
            }
        }
    }

    /// Get the `TaintedType` for the given struct name.
    /// Marks the current function as a user of this named struct.
    /// Creates an untainted `TaintedType` for this named struct if no type
    /// previously existed for it.
    pub fn get_named_struct_type(&mut self, struct_name: impl Into<String>) -> TaintedType {
        self.named_structs.borrow_mut().get_named_struct_type(struct_name.into(), &self.cur_fn).clone()
    }

    /// Is this type tainted (or, for structs, is any element of the struct tainted)
    pub fn is_type_tainted(&mut self, ty: &TaintedType) -> bool {
        self.named_structs.borrow_mut().is_type_tainted(ty, &self.cur_fn)
    }

    /// Convert this (tainted or untainted) type to the equivalent tainted type.
    ///
    /// This may have side effects, such as permanently marking struct fields or
    /// pointees as tainted.
    pub fn to_tainted(&self, ty: &TaintedType) -> TaintedType {
        self.named_structs.borrow_mut().to_tainted(ty)
    }

    /// Process the given `Function` in the given `Module`.
    ///
    /// Returns `true` if a change was made to the function's taint state, or `false` if not.
    fn process_function(&mut self, f: &'m Function, m: &'m Module) -> Result<bool, String> {
        debug!("Processing function {:?}", &f.name);
        self.cur_fn = &f.name;
        self.cur_mod = m;
        self.fn_taint_states.set_current_fn(&f.name);

        // get the taint state for the current function, creating a new one if necessary
        let cur_mod = self.cur_mod; // this is for the borrow checker - allows us to access `cur_mod` without needing to borrow `self`
        let named_structs: &Rc<_> = &self.named_structs; // similarly for the borrow checker - see note on above line
        let worklist: &Rc<_> = &self.worklist; // similarly for the borrow checker - see note on above line
        let globals: &Rc<_> = &self.globals; // similarly for the borrow checker - see note on above line
        let cur_fn = self
            .fn_taint_states
            .get_current_or_insert_with(|| {
                FunctionTaintState::from_taint_map(
                    &f.name,
                    f.parameters
                        .iter()
                        .map(|p| {
                            (p.name.clone(), TaintedType::from_llvm_type(&cur_mod.type_of(p)))
                        })
                        .collect(),
                    cur_mod,
                    Rc::clone(named_structs),
                    Rc::clone(globals),
                    Rc::clone(worklist),
                )
            });

        let summary = match self.fn_summaries.entry(&f.name) {
            Entry::Vacant(ventry) => {
                // no summary: make a starter one, assuming everything is untainted
                let cur_mod = self.cur_mod;
                let param_llvm_types = f.parameters.iter().map(|p| cur_mod.type_of(p));
                let ret_llvm_type = &f.return_type;
                ventry.insert(FunctionSummary::new_untainted(
                    param_llvm_types,
                    ret_llvm_type,
                    Rc::clone(&self.named_structs),
                ))
            },
            Entry::Occupied(oentry) => oentry.into_mut(),
        };
        // update the function parameter types from the current summary
        for (param, param_ty) in f.parameters.iter().zip_eq(summary.get_params()) {
            let _: bool = cur_fn.update_var_taintedtype(param.name.clone(), param_ty.clone()).map_err(|e| {
                format!("Encountered this error:\n  {}\nwhile processing the parameters for this function:\n  {:?}", e, &f.name)
            })?;
            // we throw away the `bool` return value of
            // `update_var_taintedtype` here: we don't care if this was
            // a change. If this was the only change from this pass,
            // there's no need to re-add this function to the worklist.
            // If a change here causes any change to the status of
            // non-parameter variables, that will result in returning
            // `true` below.
        }
        // and also update the current summary from the function parameter types
        let taint_map = cur_fn.get_taint_map();
        let param_tainted_types = f.parameters
            .iter()
            .map(|p| taint_map.get(&p.name).cloned().expect("just inserted these, so they should exist"))
            .collect();
        if summary.update_params(param_tainted_types)? {
            // summary changed: put all callers of this function on the worklist
            // because the new summary could affect inferred types in its callers
            let mut worklist = self.worklist.borrow_mut();
            for caller in self.analysis.call_graph().callers(self.cur_fn) {
                worklist.add(caller);
            }
        }

        // now do a pass over the function to propagate taints
        let mut changed = false;
        for bb in &f.basic_blocks {
            self.cur_block = Some(&bb.name);
            for inst in &bb.instrs {
                changed |= self.process_instruction(inst).map_err(|e| {
                    format!(
                        "Encountered this error:\n  {}\nwhile processing this instruction:\n  {:?}",
                        e, inst
                    )
                })?;
            }
            changed |= self.process_terminator(&bb.term).map_err(|e| {
                format!(
                    "Encountered this error:\n  {}\nwhile processing this terminator:\n  {:?}",
                    e, &bb.term
                )
            })?;
        }
        self.cur_block = None;
        Ok(changed)
    }

    /// Process the given `Instruction`, updating the current function's
    /// `FunctionTaintState` if appropriate.
    ///
    /// Returns `true` if a change was made to the `FunctionTaintState`, or `false` if not.
    fn process_instruction(&mut self, inst: &'m Instruction) -> Result<bool, String> {
        // debug!("Processing {}", brief_display_instruction(inst));
        if inst.is_binary_op() {
            let cur_fn = self.fn_taint_states.get_current();
            let bop: groups::BinaryOp = inst.clone().try_into().unwrap();
            let op0_ty = cur_fn.get_type_of_operand(bop.get_operand0())?;
            let op1_ty = cur_fn.get_type_of_operand(bop.get_operand1())?;
            let result_ty = op0_ty.join(&op1_ty)?;
            cur_fn.update_var_taintedtype(bop.get_result().clone(), result_ty)
        } else {
            match inst {
                // the unary ops which output the same type they input, in our type system
                Instruction::AddrSpaceCast(_)
                | Instruction::FNeg(_)
                | Instruction::FPExt(_)
                | Instruction::FPToSI(_)
                | Instruction::FPToUI(_)
                | Instruction::FPTrunc(_)
                | Instruction::SExt(_)
                | Instruction::SIToFP(_)
                | Instruction::Trunc(_)
                | Instruction::UIToFP(_)
                | Instruction::ZExt(_) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let uop: groups::UnaryOp = inst.clone().try_into().unwrap();
                    let op_ty = cur_fn.get_type_of_operand(uop.get_operand())?;
                    cur_fn.update_var_taintedtype(uop.get_result().clone(), op_ty)
                },
                Instruction::BitCast(bc) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let from_ty = cur_fn.get_type_of_operand(&bc.operand)?;
                    let result_ty = match &from_ty {
                        TaintedType::UntaintedValue | TaintedType::UntaintedFnPtr => {
                            TaintedType::from_llvm_type(&bc.to_type)
                        },
                        TaintedType::TaintedValue | TaintedType::TaintedFnPtr => {
                            self.to_tainted(&TaintedType::from_llvm_type(&bc.to_type))
                        },
                        TaintedType::UntaintedPointer(pointee)
                        | TaintedType::TaintedPointer(pointee) => match bc.to_type.as_ref() {
                            Type::PointerType { pointee_type, .. } => {
                                let result_pointee_type = if self.is_type_tainted(&pointee.ty()) {
                                    self.to_tainted(&TaintedType::from_llvm_type(&pointee_type))
                                } else {
                                    TaintedType::from_llvm_type(&pointee_type)
                                };
                                if self.is_type_tainted(&from_ty) {
                                    TaintedType::tainted_ptr_to(result_pointee_type)
                                } else {
                                    TaintedType::untainted_ptr_to(result_pointee_type)
                                }
                            },
                            _ => return Err("Bitcast from pointer to non-pointer".into()), // my reading of the LLVM 9 LangRef disallows this
                        },
                        from_ty @ TaintedType::ArrayOrVector(_)
                        | from_ty @ TaintedType::Struct(_) => {
                            if self.is_type_tainted(from_ty) {
                                self.to_tainted(&TaintedType::from_llvm_type(&bc.to_type))
                            } else {
                                TaintedType::from_llvm_type(&bc.to_type)
                            }
                        },
                        TaintedType::NamedStruct(name) => {
                            let def = self.get_named_struct_type(name);
                            if self.is_type_tainted(&def) {
                                self.to_tainted(&TaintedType::from_llvm_type(&bc.to_type))
                            } else {
                                TaintedType::from_llvm_type(&bc.to_type)
                            }
                        },
                    };
                    self.fn_taint_states.get_current().update_var_taintedtype(bc.get_result().clone(), result_ty)
                },
                Instruction::ExtractElement(ee) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let result_ty = if cur_fn.is_scalar_operand_tainted(&ee.index)? {
                        TaintedType::TaintedValue
                    } else {
                        cur_fn.get_type_of_operand(&ee.vector)? // in our type system, the type of a vector and the type of one of its elements are the same
                    };
                    cur_fn.update_var_taintedtype(ee.get_result().clone(), result_ty)
                },
                Instruction::InsertElement(ie) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let result_ty = if cur_fn.is_scalar_operand_tainted(&ie.index)?
                        || cur_fn.is_scalar_operand_tainted(&ie.element)?
                    {
                        TaintedType::TaintedValue
                    } else {
                        cur_fn.get_type_of_operand(&ie.vector)? // in our type system, inserting an untainted element does't change the type of the vector
                    };
                    cur_fn.update_var_taintedtype(ie.get_result().clone(), result_ty)
                },
                Instruction::ShuffleVector(sv) => {
                    // Vector operands are still scalars in our type system
                    let cur_fn = self.fn_taint_states.get_current();
                    let op0_ty = cur_fn.get_type_of_operand(&sv.operand0)?;
                    let op1_ty = cur_fn.get_type_of_operand(&sv.operand1)?;
                    let result_ty = op0_ty.join(&op1_ty)?;
                    cur_fn.update_var_taintedtype(sv.get_result().clone(), result_ty)
                },
                Instruction::ExtractValue(ev) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    // We make a pointer to the struct, and add an extra index
                    // representing getting element 0 of the resulting implicit
                    // array of structs, because get_element_ptr expects a pointer
                    let ptr_to_struct =
                        TaintedType::untainted_ptr_to(cur_fn.get_type_of_operand(&ev.aggregate)?);
                    let indices: Vec<u32> = std::iter::once(&0).chain(ev.indices.iter()).copied().collect();
                    let element_ptr_ty = self.get_element_ptr(&ptr_to_struct, &indices)?;
                    let element_ty = match element_ptr_ty {
                        TaintedType::UntaintedPointer(pointee) => pointee.ty().clone(),
                        _ => return Err(format!("ExtractValue: expected get_element_ptr to return an UntaintedPointer here; got {}", element_ptr_ty)),
                    };
                    self.fn_taint_states.get_current().update_var_taintedtype(ev.get_result().clone(), element_ty)
                },
                Instruction::InsertValue(iv) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let struct_ty = cur_fn.get_type_of_operand(&iv.aggregate)?;
                    let element_to_insert = cur_fn.get_type_of_operand(&iv.element)?;
                    // We make a pointer to the struct, and add an extra index
                    // representing getting element 0 of the resulting implicit
                    // array of structs, because get_element_ptr expects a pointer
                    let ptr_to_struct = TaintedType::untainted_ptr_to(struct_ty.clone());
                    let indices: Vec<u32> = std::iter::once(&0).chain(iv.indices.iter()).copied().collect();
                    let ptr_to_indicated_element = self.get_element_ptr(&ptr_to_struct, &indices)?;
                    let cur_fn = self.fn_taint_states.get_current();
                    match ptr_to_indicated_element {
                        TaintedType::UntaintedPointer(mut pointee) | TaintedType::TaintedPointer(mut pointee) => {
                            cur_fn.update_pointee_taintedtype(&mut pointee, &element_to_insert)?;
                        },
                        _ => panic!("Expected get_element_ptr to return a pointer, but got {}", ptr_to_indicated_element),
                    }
                    cur_fn.update_var_taintedtype(iv.get_result().clone(), struct_ty)
                },
                Instruction::Alloca(alloca) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let result_ty = if cur_fn.is_scalar_operand_tainted(&alloca.num_elements)? {
                        TaintedType::TaintedValue
                    } else {
                        TaintedType::untainted_ptr_to(TaintedType::from_llvm_type(
                            &alloca.allocated_type,
                        ))
                    };
                    cur_fn.update_var_taintedtype(alloca.get_result().clone(), result_ty)
                },
                Instruction::Load(load) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let addr_ty = cur_fn.get_type_of_operand(&load.address)?;
                    let result_ty = self.get_load_result_ty(&addr_ty)?;
                    self.fn_taint_states.get_current().update_var_taintedtype(load.get_result().clone(), result_ty)
                },
                Instruction::Store(store) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let mut addr_ty = cur_fn.get_type_of_operand(&store.address)?;
                    let new_value_ty = cur_fn.get_type_of_operand(&store.value)?;
                    self.process_store(&new_value_ty, &mut addr_ty)
                },
                Instruction::Fence(_) => Ok(false),
                Instruction::GetElementPtr(gep) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let ptr = cur_fn.get_type_of_operand(&gep.address)?;
                    let result_ty = self.get_element_ptr(&ptr, &gep.indices)?;
                    self.fn_taint_states.get_current().update_var_taintedtype(gep.get_result().clone(), result_ty)
                },
                Instruction::PtrToInt(pti) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    match cur_fn.get_type_of_operand(&pti.operand)? {
                        TaintedType::UntaintedPointer(_) | TaintedType::UntaintedFnPtr => {
                            cur_fn.update_var_taintedtype(pti.get_result().clone(), TaintedType::UntaintedValue)
                        },
                        TaintedType::TaintedPointer(_) | TaintedType::TaintedFnPtr => {
                            cur_fn.update_var_taintedtype(pti.get_result().clone(), TaintedType::TaintedValue)
                        },
                        TaintedType::UntaintedValue => {
                            Err(format!("PtrToInt on an UntaintedValue: {:?}", &pti.operand))
                        },
                        TaintedType::TaintedValue => {
                            Err(format!("PtrToInt on an TaintedValue: {:?}", &pti.operand))
                        },
                        TaintedType::ArrayOrVector(_) => {
                            Err(format!("PtrToInt on an array or vector: {:?}", &pti.operand))
                        },
                        TaintedType::Struct(_) | TaintedType::NamedStruct(_) => {
                            Err(format!("PtrToInt on a struct: {:?}", &pti.operand))
                        },
                    }
                },
                Instruction::IntToPtr(itp) => {
                    // we make the (potentially unsound) assumption that the
                    // pointed-to contents are both untainted and unaliased,
                    // meaning that no pointers to any part of those contents
                    // (or anything referred to by those contents) already exist
                    let untainted_ptr_ty = TaintedType::from_llvm_type(&itp.to_type);
                    // all we do is create a tainted pointer from a tainted
                    // value, and an untainted pointer from an untainted value
                    let cur_fn = self.fn_taint_states.get_current();
                    let in_ty = cur_fn.get_type_of_operand(&itp.operand)?;
                    let ptr_ty = if self.is_type_tainted(&in_ty) {
                        self.to_tainted(&untainted_ptr_ty)
                    } else {
                        untainted_ptr_ty
                    };
                    self.fn_taint_states.get_current().update_var_taintedtype(itp.get_result().clone(), ptr_ty)
                },
                Instruction::ICmp(icmp) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let op0_ty = cur_fn.get_type_of_operand(&icmp.operand0)?;
                    let op1_ty = cur_fn.get_type_of_operand(&icmp.operand1)?;
                    let result_ty = if self.is_type_tainted(&op0_ty) || self.is_type_tainted(&op1_ty) {
                        TaintedType::TaintedValue
                    } else {
                        TaintedType::UntaintedValue
                    };
                    self.fn_taint_states.get_current().update_var_taintedtype(icmp.get_result().clone(), result_ty)
                },
                Instruction::FCmp(fcmp) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let op0_ty = cur_fn.get_type_of_operand(&fcmp.operand0)?;
                    let op1_ty = cur_fn.get_type_of_operand(&fcmp.operand1)?;
                    let result_ty = if self.is_type_tainted(&op0_ty) || self.is_type_tainted(&op1_ty) {
                        TaintedType::TaintedValue
                    } else {
                        TaintedType::UntaintedValue
                    };
                    self.fn_taint_states.get_current().update_var_taintedtype(fcmp.get_result().clone(), result_ty)
                },
                Instruction::Phi(phi) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let mut incoming_types = phi
                        .incoming_values
                        .iter()
                        .map(|(op, _)| cur_fn.get_type_of_operand(op))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter();
                    let mut result_ty = incoming_types.next().expect("Phi with no incoming values");
                    for ty in incoming_types {
                        result_ty = result_ty.join(&ty)?;
                    }
                    // in addition, the result should be tainted if the attacker can influence
                    // the control flow sufficiently to choose the result of this phi.
                    //
                    // Examples: (blocks A, B, C, etc)
                    // Suppose the branch condition in A's terminator is tainted.
                    //
                    //    A          A           A          A                 A      B
                    //  /   \      /   \         | \      /   \             /   \  /   \
                    // B     C    B     C   Z    |  B    B     C <-- \     Z     D      Y
                    //  \   /      \   /   /     | /     |     |      |
                    //    D   Z      D  --       D       Y     Z --> /
                    //    | /        |   /                \   /
                    //    E          E -                    D
                    //
                    // In all of the above examples, if D has a phi node, the result of that
                    // phi should be tainted; but if E has a phi node, the result of that phi
                    // should not be tainted.
                    // In all of the above examples, either D itself is control-dependent on A
                    // (as in the fifth example), or at least one of D's predecessors is. (Not
                    // necessarily all, as the second and third example show.)
                    // But E is not control-dependent on A, and neither are any of E's
                    // predecessors.
                    // So we taint the phi result if either D is control-dependent on A or if
                    // any of D's predecessors are control-dependent on A.
                    // I.e., we taint this phi's result if the current block is control-
                    // dependent on a block with tainted terminator, or if any of the incoming
                    // phi blocks are control-dependent on a block with tainted terminator.
                    let cdg = self.analysis.module_analysis(&self.cur_mod.name).fn_analysis(self.cur_fn).control_dependence_graph();
                    let is_ctrl_dep_on_tainted_term = |block: &'m Name| {
                        cdg.get_control_dependencies(block)
                            .any(|dep| cur_fn.is_terminator_tainted(dep))
                    };
                    if is_ctrl_dep_on_tainted_term(&self.cur_block.unwrap()) {
                        result_ty = self.to_tainted(&result_ty);
                    } else if phi.incoming_values.iter().any(|(_, block)| is_ctrl_dep_on_tainted_term(block)) {
                        result_ty = self.to_tainted(&result_ty);
                    }
                    self.fn_taint_states.get_current().update_var_taintedtype(phi.get_result().clone(), result_ty)
                },
                Instruction::Select(select) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let result_ty = if cur_fn.is_scalar_operand_tainted(&select.condition)? {
                        TaintedType::TaintedValue
                    } else {
                        let true_ty = cur_fn.get_type_of_operand(&select.true_value)?;
                        let false_ty = cur_fn.get_type_of_operand(&select.false_value)?;
                        true_ty.join(&false_ty)?
                    };
                    cur_fn.update_var_taintedtype(select.get_result().clone(), result_ty)
                },
                Instruction::AtomicRMW(rmw) => {
                    let cur_fn = self.fn_taint_states.get_current();
                    let mut addr_ty = cur_fn.get_type_of_operand(&rmw.address)?;
                    let value_ty = cur_fn.get_type_of_operand(&rmw.value)?;
                    let loaded_ty = self.get_load_result_ty(&addr_ty)?;
                    let ty_to_store = loaded_ty.join(&value_ty)?;
                    self.process_store(&ty_to_store, &mut addr_ty)?;
                    self.fn_taint_states.get_current().update_var_taintedtype(rmw.get_result().clone(), loaded_ty)
                },
                Instruction::Call(call) => {
                    match &call.function {
                        Either::Right(Operand::ConstantOperand(cref)) => match cref.as_ref() {
                            Constant::GlobalReference { name: Name::Name(name), .. } => {
                                if name.starts_with("llvm.lifetime")
                                    || name.starts_with("llvm.invariant")
                                    || name.starts_with("llvm.launder.invariant")
                                    || name.starts_with("llvm.strip.invariant")
                                    || name.starts_with("llvm.dbg")
                                {
                                    Ok(false) // these are all safe to ignore
                                } else if name.starts_with("llvm.memset") {
                                    // update the address type as appropriate, just like for Store
                                    let cur_fn = self.fn_taint_states.get_current();
                                    let address_operand = call.arguments.get(0).map(|(op, _)| op).ok_or_else(|| format!("Expected llvm.memset to have at least three arguments, but it has {}", call.arguments.len()))?;
                                    let value_operand = call.arguments.get(1).map(|(op, _)| op).ok_or_else(|| format!("Expected llvm.memset to have at least three arguments, but it has {}", call.arguments.len()))?;
                                    let address_ty = cur_fn.get_type_of_operand(address_operand)?;
                                    let value_ty = cur_fn.get_type_of_operand(value_operand)?;
                                    let mut pointee = match address_ty {
                                        TaintedType::UntaintedPointer(pointee) | TaintedType::TaintedPointer(pointee) => pointee,
                                        _ => return Err(format!("llvm.memset: expected first argument to be a pointer, but it was {}", address_ty)),
                                    };
                                    cur_fn.update_pointee_taintedtype(&mut pointee, &value_ty)
                                } else {
                                    self.process_function_call(call, name)
                                }
                            },
                            Constant::GlobalReference{ name, .. } => {
                                unimplemented!("Call of a function with a numbered name: {:?}", name)
                            },
                            _ => unimplemented!("Call of a constant function pointer"),
                        },
                        Either::Right(_) => {
                            let func_ty = self.cur_mod.type_of(&call.function);
                            // Assume that this function pointer could point to any function in
                            // the analyzed module(s) that has the appropriate type
                            let targets: Vec<&'m str> = self.analysis.functions_by_type().functions_with_type(&func_ty).collect();
                            if targets.is_empty() {
                                // no valid targets for the function pointer in
                                // the analyzed module(s); treat this as a call
                                // to an external function
                                use config::ExternalFunctionHandling;
                                match self.config.ext_functions_default {
                                    ExternalFunctionHandling::IgnoreAndReturnUntainted => {
                                        match &call.dest {
                                            None => Ok(false),
                                            Some(dest) => {
                                                let untainted_ret_ty = TaintedType::from_llvm_type(&self.cur_mod.type_of(call));
                                                self.fn_taint_states.get_current().update_var_taintedtype(dest.clone(), untainted_ret_ty)
                                            },
                                        }
                                    },
                                    ExternalFunctionHandling::IgnoreAndReturnTainted => {
                                        match &call.dest {
                                            None => Ok(false),
                                            Some(dest) => {
                                                let untainted_ret_ty = TaintedType::from_llvm_type(&self.cur_mod.type_of(call));
                                                let tainted_ret_ty = self.to_tainted(&untainted_ret_ty);
                                                self.fn_taint_states.get_current().update_var_taintedtype(dest.clone(), tainted_ret_ty)
                                            },
                                        }
                                    },
                                    ExternalFunctionHandling::PropagateTaintShallow => {
                                        let cur_fn = self.fn_taint_states.get_current();
                                        if call
                                            .arguments
                                            .iter()
                                            .map(|(o, _)| cur_fn.get_type_of_operand(o))
                                            .collect::<Result<Vec<_>, String>>()?
                                            .into_iter()
                                            .any(|t| self.is_type_tainted(&t))
                                        {
                                            // just like IgnoreAndReturnTainted
                                            match &call.dest {
                                                None => Ok(false),
                                                Some(dest) => {
                                                    let untainted_ret_ty = TaintedType::from_llvm_type(&self.cur_mod.type_of(call));
                                                    let tainted_ret_ty = self.to_tainted(&untainted_ret_ty);
                                                    self.fn_taint_states.get_current().update_var_taintedtype(dest.clone(), tainted_ret_ty)
                                                },
                                            }
                                        } else {
                                            // just like IgnoreAndReturnUntainted
                                            match &call.dest {
                                                None => Ok(false),
                                                Some(dest) => {
                                                    let untainted_ret_ty = TaintedType::from_llvm_type(&self.cur_mod.type_of(call));
                                                    self.fn_taint_states.get_current().update_var_taintedtype(dest.clone(), untainted_ret_ty)
                                                },
                                            }
                                        }
                                    },
                                    ExternalFunctionHandling::PropagateTaintDeep => {
                                        unimplemented!("ExternalFunctionHandling::PropagateTaintDeep")
                                    },
                                    ExternalFunctionHandling::Panic => {
                                        panic!("Call of a function pointer")
                                    },
                                }
                            } else {
                                let mut changed = false;
                                // we could call any of these targets. Taint accordingly.
                                for target in targets {
                                    changed |= self.process_function_call(call, target)?;
                                }
                                Ok(changed)
                            }
                        },
                        Either::Left(_) => unimplemented!("inline assembly"),
                    }
                },
                _ => unimplemented!("instruction {:?}", inst),
            }
        }
    }

    /// Get the `TaintedType` of the value loaded from the given address.
    fn get_load_result_ty(&mut self, addr: &TaintedType) -> Result<TaintedType, String> {
        match addr {
            TaintedType::UntaintedValue | TaintedType::TaintedValue => {
                Err(format!(
                    "Load: address is not a pointer: got type {}",
                    addr
                ))
            },
            TaintedType::UntaintedFnPtr | TaintedType::TaintedFnPtr => {
                Err("Loading from a function pointer".into())
            },
            TaintedType::ArrayOrVector(_)
            | TaintedType::Struct(_)
            | TaintedType::NamedStruct(_) => {
                Err(format!(
                    "Load: address is not a pointer: got type {}",
                    addr
                ))
            },
            TaintedType::UntaintedPointer(pointee) => {
                Ok(pointee.ty().clone())
            },
            TaintedType::TaintedPointer(pointee) => {
                if self.config.dereferencing_tainted_ptr_gives_tainted {
                    pointee.taint(&mut self.named_structs.borrow_mut());
                }
                Ok(pointee.ty().clone())
            },
        }
    }

    /// Process the store of a value to an address.
    fn process_store(&mut self, value: &TaintedType, addr: &mut TaintedType) -> Result<bool, String> {
        match addr {
            TaintedType::UntaintedValue | TaintedType::TaintedValue => {
                Err(format!(
                    "Store: address is not a pointer: got type {}",
                    addr
                ))
            },
            TaintedType::UntaintedFnPtr | TaintedType::TaintedFnPtr => {
                Err("Storing to a function pointer".into())
            },
            TaintedType::ArrayOrVector(_)
            | TaintedType::Struct(_)
            | TaintedType::NamedStruct(_) => {
                Err(format!(
                    "Store: address is not a pointer: got type {}",
                    addr
                ))
            },
            TaintedType::UntaintedPointer(ref mut pointee) | TaintedType::TaintedPointer(ref mut pointee) => {
                // Storing to a location while control-flow is tainted also
                // needs to result in the stored value being marked tainted.
                // This is because a tainted value (in some branch condition
                // etc) influenced the value stored at this location.
                let cur_fn = self.fn_taint_states.get_current();
                let cdg = self.analysis.module_analysis(&self.cur_mod.name).fn_analysis(self.cur_fn).control_dependence_graph();
                let need_to_taint = cdg
                    .get_control_dependencies(&self.cur_block.unwrap())
                    .any(|dep| cur_fn.is_terminator_tainted(dep));

                // now update the store address's type based on the value being
                // stored through it.
                // specifically, update the pointee in that address type:
                // e.g., if we're storing a tainted value, we want
                // to update the address type to "pointer to tainted"
                if need_to_taint {
                    let tainted_val = self.to_tainted(value);
                    self.fn_taint_states.get_current().update_pointee_taintedtype(pointee, &tainted_val)
                } else {
                    cur_fn.update_pointee_taintedtype(pointee, value)
                }
            },
        }
    }

    /// Process the a call of a function with the given name.
    fn process_function_call(
        &mut self,
        call: &instruction::Call,
        funcname: &'m str,
    ) -> Result<bool, String> {
        // Get the function summary for the called function
        let summary = match self.fn_summaries.entry(funcname.clone()) {
            Entry::Occupied(oentry) => oentry.into_mut(),
            Entry::Vacant(ventry) => {
                // no summary: start with the default one (nothing tainted) and add the
                // called function to the worklist so that we can compute a better one
                self.worklist.borrow_mut().add(funcname);
                let cur_mod = self.cur_mod;
                ventry.insert(FunctionSummary::new_untainted(
                    call.arguments.iter().map(|(arg, _)| cur_mod.type_of(arg)),
                    &cur_mod.type_of(call),
                    Rc::clone(&self.named_structs),
                ))
            },
        };
        // use the `TaintedType`s of the provided arguments to update the
        // `TaintedType`s of the parameters in the function summary, if appropriate
        let cur_fn = self.fn_taint_states.get_current();
        let arg_types = call
            .arguments
            .iter()
            .map(|(arg, _)| cur_fn.get_type_of_operand(arg))
            .collect::<Result<_, _>>()?;
        if summary.update_params(arg_types)? {
            // summary changed: put all callers of the called function on the worklist
            // because the new summary could affect inferred types in its callers
            let mut worklist = self.worklist.borrow_mut();
            for caller in self.analysis.call_graph().callers(funcname) {
                worklist.add(caller);
            }
            // and also put the called function itself on the worklist
            worklist.add(funcname);
        }
        // and finally, for non-void calls, use the return type in the summary to
        // update the type of the result in this function
        let summary_ret_ty = summary.get_ret_ty().clone(); // this should end the life of `summary` and therefore its mutable borrow of `self.fn_summaries`
        match &call.dest {
            Some(varname) => {
                cur_fn.update_var_taintedtype(varname.clone(), summary_ret_ty.unwrap())
            },
            None => Ok(false), // nothing changed in the current function
        }
    }

    /// Process the given `Terminator`, updating taint states if appropriate.
    fn process_terminator(&mut self, term: &Terminator) -> Result<bool, String> {
        match term {
            Terminator::Ret(ret) => {
                // first mark the terminator tainted if necessary
                let mut changed = false;
                if let Some(ret_val) = &ret.return_operand {
                    let cur_fn = self.fn_taint_states.get_current();
                    let op_type = cur_fn.get_type_of_operand(ret_val)?;
                    if self.is_type_tainted(&op_type) {
                        let cur_fn = self.fn_taint_states.get_current();
                        changed |= cur_fn.mark_terminator_tainted(self.cur_block.cloned().unwrap());
                    }
                }
                // now update the function summary if necessary
                match self.fn_summaries.get_mut(self.cur_fn) {
                    None => {
                        // no summary: no use making one until we know we need one
                        Ok(changed)
                    },
                    Some(summary) => {
                        let cur_fn = self.fn_taint_states.get_current();
                        let ty = ret
                            .return_operand
                            .as_ref()
                            .map(|op| cur_fn.get_type_of_operand(op))
                            .transpose()?;
                        if summary.update_ret(&ty.as_ref())? {
                            // summary changed: put all our callers on the worklist
                            // because the new summary could affect inferred types in our callers
                            let mut worklist = self.worklist.borrow_mut();
                            for caller in self.analysis.call_graph().callers(self.cur_fn) {
                                worklist.add(caller);
                            }
                            changed = true;
                        }
                        Ok(changed)
                    },
                }
            },
            Terminator::CondBr(condbr) => {
                let cur_fn = self.fn_taint_states.get_current();
                let op_type = cur_fn.get_type_of_operand(&condbr.condition)?;
                if self.is_type_tainted(&op_type) {
                    let cur_fn = self.fn_taint_states.get_current();
                    Ok(cur_fn.mark_terminator_tainted(self.cur_block.cloned().unwrap()))
                } else {
                    Ok(false)
                }
            },
            Terminator::Switch(switch) => {
                let cur_fn = self.fn_taint_states.get_current();
                let op_type = cur_fn.get_type_of_operand(&switch.operand)?;
                if self.is_type_tainted(&op_type) {
                    let cur_fn = self.fn_taint_states.get_current();
                    Ok(cur_fn.mark_terminator_tainted(self.cur_block.cloned().unwrap()))
                } else {
                    Ok(false)
                }
            },
            Terminator::IndirectBr(ibr) => {
                let cur_fn = self.fn_taint_states.get_current();
                let op_type = cur_fn.get_type_of_operand(&ibr.operand)?;
                if self.is_type_tainted(&op_type) {
                    let cur_fn = self.fn_taint_states.get_current();
                    Ok(cur_fn.mark_terminator_tainted(self.cur_block.cloned().unwrap()))
                } else {
                    Ok(false)
                }
            },
            Terminator::Br(_) => Ok(false), // unconditional branches can't be tainted
            Terminator::Unreachable(_) => Ok(false),
            _ => unimplemented!("terminator {:?}", term),
        }
    }

    fn get_element_ptr<'a, 'b, I: Index + 'b>(
        &mut self,
        parent_ptr: &'a TaintedType,
        indices: impl IntoIterator<Item = &'b I>,
    ) -> Result<TaintedType, String> {
        self.named_structs.borrow_mut().get_element_ptr(&self.cur_fn, parent_ptr, indices)
    }
}

/// for debugging. E.g., if you want to print each instruction as it's being
/// processed, it's nice to have a very short description that still identifies
/// the instruction
#[allow(dead_code)]
fn brief_display_instruction(inst: &Instruction) -> String {
    match inst.try_get_result() {
        Some(name) => format!("instruction producing {}", name),
        None => match inst {
            Instruction::Store(_) => "a store".into(),
            Instruction::Call(_) => "a void call".into(),
            _ => "a void-typed instruction".into(),
        }
    }
}
