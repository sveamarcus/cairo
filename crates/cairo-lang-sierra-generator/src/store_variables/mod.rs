//! Handles the automatic addition of store_temp() and store_local() statements.

mod known_stack;
mod state;

#[cfg(test)]
mod test;

use cairo_lang_sierra as sierra;
use cairo_lang_sierra::extensions::lib_func::{LibfuncSignature, ParamSignature, SierraApChange};
use cairo_lang_sierra::ids::ConcreteLibfuncId;
use cairo_lang_sierra::program::{GenBranchInfo, GenBranchTarget, GenStatement};
use cairo_lang_utils::extract_matches;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use itertools::zip_eq;
use state::{merge_optional_states, State};

use self::state::{DeferredVariableInfo, DeferredVariableKind, VarState};
use crate::db::SierraGenGroup;
use crate::pre_sierra;
use crate::store_variables::known_stack::KnownStack;
use crate::utils::{
    dup_libfunc_id, rename_libfunc_id, simple_statement, store_local_libfunc_id,
    store_temp_libfunc_id,
};

/// A map from variables that should be stored as local to their allocated
/// space.
pub type LocalVariables = OrderedHashMap<sierra::ids::VarId, sierra::ids::VarId>;

/// Information about a libfunc, required by the `store_variables` module.
pub struct LibfuncInfo {
    pub signature: LibfuncSignature,
}

/// Automatically adds store_temp() statements to the given list of [pre_sierra::Statement].
/// For example, a deferred reference (e.g., `[ap] + [fp - 3]`) needs to be stored as a temporary
/// or local variable before being included in additional computation.
/// The function will add the necessary `store_temp()` instruction before the first use of the
/// deferred reference.
///
/// `local_variables` is a map from variables that should be stored as local to their allocated
/// space.
pub fn add_store_statements<GetLibfuncSignature>(
    db: &dyn SierraGenGroup,
    statements: Vec<pre_sierra::Statement>,
    get_lib_func_signature: &GetLibfuncSignature,
    local_variables: LocalVariables,
    params: &[sierra::ids::VarId],
) -> Vec<pre_sierra::Statement>
where
    GetLibfuncSignature: Fn(ConcreteLibfuncId) -> LibfuncInfo,
{
    let mut handler = AddStoreVariableStatements::new(db, local_variables, params);
    // Go over the statements, restarting whenever we see a branch or a label.
    for statement in statements.into_iter() {
        handler.handle_statement(statement, get_lib_func_signature);
    }
    handler.finalize()
}

struct AddStoreVariableStatements<'a> {
    db: &'a dyn SierraGenGroup,
    local_variables: LocalVariables,
    /// A list of output statements (the original statement, together with the added statements,
    /// such as "store_temp").
    result: Vec<pre_sierra::Statement>,
    /// The current information known about the state of the variables. None means the statement is
    /// not reachable from the previous statement.
    state_opt: Option<State>,
    /// A map from [LabelId](pre_sierra::LabelId) to the known state (so far).
    ///
    /// For every branch that does not continue to the next statement, the current known state is
    /// added to the map. When the label is visited, it is merged with the known state, and removed
    /// from the map.
    future_states: OrderedHashMap<pre_sierra::LabelId, State>,
}
impl<'a> AddStoreVariableStatements<'a> {
    /// Constructs a new [AddStoreVariableStatements] object.
    fn new(
        db: &'a dyn SierraGenGroup,
        local_variables: LocalVariables,
        params: &[sierra::ids::VarId],
    ) -> Self {
        let mut state = State::default();
        state.variables.extend(params.iter().map(|var| (var.clone(), VarState::LocalVar)));

        AddStoreVariableStatements {
            db,
            local_variables,
            result: Vec::new(),
            state_opt: Some(state),
            future_states: OrderedHashMap::default(),
        }
    }

    /// Handles a single statement, including adding required store statements and the statement
    /// itself.
    fn handle_statement<GetLibfuncInfo>(
        &mut self,
        statement: pre_sierra::Statement,
        get_lib_func_signature: &GetLibfuncInfo,
    ) where
        GetLibfuncInfo: Fn(ConcreteLibfuncId) -> LibfuncInfo,
    {
        match &statement {
            pre_sierra::Statement::Sierra(GenStatement::Invocation(invocation)) => {
                let libfunc_info = get_lib_func_signature(invocation.libfunc_id.clone());
                let signature = libfunc_info.signature;
                let arg_states =
                    self.prepare_libfunc_arguments(&invocation.args, &signature.param_signatures);
                match &invocation.branches[..] {
                    [GenBranchInfo { target: GenBranchTarget::Fallthrough, results }] => {
                        // A simple invocation.
                        let branch_signature = &signature.branch_signatures[0];
                        match branch_signature.ap_change {
                            SierraApChange::Unknown => {
                                // If the ap-change is unknown, variables that will be revoked
                                // otherwise should be stored as locals.
                                self.store_variables_as_locals();
                            }
                            SierraApChange::BranchAlign | SierraApChange::Known { .. } => {}
                        }

                        self.state().register_outputs(
                            results,
                            branch_signature,
                            &invocation.args,
                            &arg_states,
                        );
                    }
                    _ => {
                        // This starts a branch. Store all deferred variables.
                        if invocation.branches.len() > 1 {
                            self.store_all_possibly_lost_variables();
                        }

                        // Go over the branches. The state of a branch that points to `Fallthrough`
                        // is merged into `fallthrough_state`.
                        let mut fallthrough_state: Option<State> = None;
                        for (branch, branch_signature) in
                            zip_eq(&invocation.branches, signature.branch_signatures)
                        {
                            let mut state_at_branch = self.state().clone();
                            state_at_branch.register_outputs(
                                &branch.results,
                                &branch_signature,
                                &invocation.args,
                                &arg_states,
                            );

                            self.add_future_state(
                                &branch.target,
                                state_at_branch,
                                &mut fallthrough_state,
                            );
                        }
                        self.state_opt = fallthrough_state;
                    }
                }
                self.result.push(statement);
            }
            pre_sierra::Statement::Sierra(GenStatement::Return(_return_statement)) => {
                self.result.push(statement);
                // `return` statements are preceded by `PushValues` which takes care of pushing
                // the return values onto the stack. The rest of the variables are not
                // needed.

                self.state().variables.clear();

                // The next statement is not reachable from this one. Set `state` to `None`.
                self.state_opt = None;
            }
            pre_sierra::Statement::Label(pre_sierra::Label { id: label_id }) => {
                // Merge self.known_stack with the future_stack that corresponds to the label, if
                // any.
                self.state_opt = merge_optional_states(
                    std::mem::take(&mut self.state_opt),
                    self.future_states.swap_remove(label_id),
                );

                self.result.push(statement);
            }
            pre_sierra::Statement::PushValues(push_values) => {
                self.push_values(push_values);
            }
        }
    }

    /// Prepares the given `args` to be used as arguments for a libfunc.
    ///
    /// Returns a map from arguments' [sierra::ids::VarId] to [DeferredVariableInfo] for arguments
    /// that have a deferred value after the function (that is, they were not stored as
    /// temp/local by the function).
    fn prepare_libfunc_arguments(
        &mut self,
        args: &[sierra::ids::VarId],
        param_signatures: &[ParamSignature],
    ) -> Vec<VarState> {
        zip_eq(args, param_signatures)
            .map(|(arg, param_signature)| {
                let arg_state = self.prepare_libfunc_argument(
                    arg,
                    param_signature.allow_deferred,
                    param_signature.allow_add_const,
                    param_signature.allow_const,
                );
                // Make sure the argument is consumed.
                self.state().variables.swap_remove(arg);
                arg_state
            })
            .collect()
    }

    /// Prepares the given `arg` to be used as an argument for a libfunc.
    ///
    /// Returns the VarState of the argument.
    fn prepare_libfunc_argument(
        &mut self,
        arg: &sierra::ids::VarId,
        allow_deferred: bool,
        allow_add_const: bool,
        allow_const: bool,
    ) -> VarState {
        let var_state = self.state().variables.swap_remove(arg).unwrap_or_else(|| {
            unreachable!("Unknown state for variable `{arg}`.");
        });
        match &var_state {
            VarState::Deferred { info: deferred_info } => {
                if self.local_variables.get(arg).is_some() {
                    // If a deferred argument was marked as a local variable, then store
                    // it. This is important in case an alias of the variable is used later
                    // (for example, due to `SameAsParam` output).
                    self.store_deferred(arg, &deferred_info.ty)
                } else {
                    match deferred_info.kind {
                        state::DeferredVariableKind::Const => {
                            if !allow_const {
                                return self.store_deferred(arg, &deferred_info.ty);
                            }
                        }
                        state::DeferredVariableKind::AddConst => {
                            if !allow_add_const {
                                return self.store_deferred(arg, &deferred_info.ty);
                            }
                        }
                        state::DeferredVariableKind::Generic => {
                            if !allow_deferred {
                                return self.store_deferred(arg, &deferred_info.ty);
                            }
                        }
                    };
                    var_state
                }
            }
            VarState::TempVar { .. } => {
                self.state().variables.insert(arg.clone(), var_state.clone());
                if self.store_temp_as_local(arg) {
                    return VarState::LocalVar;
                }
                var_state
            }
            VarState::LocalVar => VarState::LocalVar,
        }
    }

    /// Adds a store_temp() or store_local() instruction for the given deferred variable.
    /// The variable should be removed from the `deferred_variables` map prior to this call.
    ///
    /// Returns the variable state after the store.
    fn store_deferred(
        &mut self,
        var: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) -> VarState {
        self.store_deferred_ex(var, var, ty)
    }

    /// Same as `store_deferred` only allows the `store_temp` case to use a different variable.
    fn store_deferred_ex(
        &mut self,
        var: &sierra::ids::VarId,
        var_on_stack: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) -> VarState {
        // Check if this variable should be a local variable.
        if let Some(uninitialized_local_var_id) = self.local_variables.get(var) {
            self.store_local(var, &uninitialized_local_var_id.clone(), ty);
            VarState::LocalVar
        } else {
            self.store_temp(var, var_on_stack, ty);
            VarState::TempVar { ty: ty.clone() }
        }
    }

    fn push_values(&mut self, push_values: &Vec<pre_sierra::PushValue>) {
        if push_values.is_empty() {
            return;
        }

        // Optimization: check if there is a prefix of `push_values` that is already on the stack.
        let prefix_size = self.known_stack().compute_on_stack_prefix_size(push_values);

        for (i, pre_sierra::PushValue { var, var_on_stack, ty, dup }) in
            push_values.iter().enumerate()
        {
            let var_state = self
                .state()
                .variables
                .swap_remove(var)
                .unwrap_or_else(|| unreachable!("Unknown state for variable `{var}`."));

            let is_on_stack = if let VarState::Deferred { info: deferred_info } = &var_state {
                let deferred_info = deferred_info.clone();
                if let DeferredVariableKind::Const = deferred_info.kind {
                    // TODO(orizi): This is an ugly fix for case of literals. Fix properly.
                    if *dup {
                        self.dup(var, var_on_stack, ty);
                        self.store_temp(var_on_stack, var_on_stack, ty);
                        self.state().variables.insert(
                            var.clone(),
                            VarState::Deferred { info: deferred_info.clone() },
                        );
                    } else {
                        self.store_temp(var, var_on_stack, ty);
                    }
                    continue;
                } else if matches!(
                    self.store_deferred_ex(var, var_on_stack, &deferred_info.ty),
                    VarState::TempVar { .. }
                ) {
                    if *dup {
                        // In the dup case we dup `var_on_stack` that is ready for push into
                        // `var` that should still be available as a temporary var.
                        self.state()
                            .variables
                            .insert(var.clone(), VarState::TempVar { ty: ty.clone() });
                        self.dup(var_on_stack, var, ty);
                    }
                    continue;
                } else {
                    false
                }
            } else {
                self.state().variables.insert(var.clone(), var_state.clone());

                // Check if this is part of the prefix. If it is, rename instead of adding
                // `store_temp`.
                i < prefix_size
            };

            if is_on_stack {
                if *dup {
                    self.state().variables.insert(var_on_stack.clone(), var_state);
                    self.dup(var, var_on_stack, ty);
                } else {
                    self.rename_var(var, var_on_stack, ty);
                }
            } else {
                let src = if *dup {
                    self.dup(var, var_on_stack, ty);
                    var_on_stack
                } else {
                    var
                };
                self.store_temp(src, var_on_stack, ty);
            }
        }
    }

    /// Stores all the variables that may possibly get misaligned or revoked.
    fn store_all_possibly_lost_variables(&mut self) {
        for (var, var_state) in self.state().variables.clone() {
            match var_state {
                VarState::TempVar { .. } => {
                    self.store_temp_as_local(&var);
                }
                VarState::Deferred { info } => {
                    if info.kind != DeferredVariableKind::Const {
                        self.state().variables.swap_remove(&var);
                        self.store_deferred(&var, &info.ty);
                    }
                }
                VarState::LocalVar => {}
            }
        }
    }

    /// Copies the given variable into a local variable if it is marked as local.
    /// Removes it from [State::variables].
    fn store_temp_as_local(&mut self, var: &sierra::ids::VarId) -> bool {
        if let Some(uninitialized_local_var_id) = self.local_variables.get(var).cloned() {
            let var_state = self.state().variables.swap_remove(var).unwrap();

            let VarState::TempVar { ty } = var_state else {
                panic!("Expected a temporary variable");
            };
            self.store_local(var, &uninitialized_local_var_id, &ty);
            return true;
        }
        false
    }

    /// Stores all the deffered and temporary variables as local variables.
    fn store_variables_as_locals(&mut self) {
        let mut vars_to_store: Vec<(
            sierra::ids::VarId,
            sierra::ids::VarId,
            sierra::ids::ConcreteTypeId,
        )> = vec![];
        for (var, var_state) in self.state_ref().variables.iter() {
            if let Some(uninitialized_local_var_id) = self.local_variables.get(var).cloned() {
                match var_state {
                    VarState::Deferred { info: DeferredVariableInfo { ty, .. } }
                    | VarState::TempVar { ty } => {
                        vars_to_store.push((var.clone(), uninitialized_local_var_id, ty.clone()))
                    }
                    VarState::LocalVar => {}
                };
            }
        }

        for (var, uninitialized_local_var_id, ty) in vars_to_store {
            assert!(self.state().variables.swap_remove(&var).is_some());
            self.store_local(&var, &uninitialized_local_var_id, &ty);
        }
    }

    fn finalize(self) -> Vec<pre_sierra::Statement> {
        assert!(
            self.state_opt.is_none(),
            "Internal compiler error: Found a reachable statement at the end of the function."
        );
        assert!(
            self.future_states.is_empty(),
            "Internal compiler error: Unhandled label in 'store_variables'."
        );
        self.result
    }

    /// Adds a `store_temp` command storing `var` into `var_on_stack`.
    fn store_temp(
        &mut self,
        var: &sierra::ids::VarId,
        var_on_stack: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) {
        self.result.push(simple_statement(
            store_temp_libfunc_id(self.db, ty.clone()),
            &[var.clone()],
            &[var_on_stack.clone()],
        ));

        self.known_stack().push(var_on_stack);
        self.state().variables.insert(var_on_stack.clone(), VarState::TempVar { ty: ty.clone() });
    }

    /// Adds a `store_local` command storing `var` into itself using the preallocated
    /// `uninitialized_local_var_id`.
    fn store_local(
        &mut self,
        var: &sierra::ids::VarId,
        uninitialized_local_var_id: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) {
        self.result.push(simple_statement(
            store_local_libfunc_id(self.db, ty.clone()),
            &[uninitialized_local_var_id.clone(), var.clone()],
            &[var.clone()],
        ));
        self.state().variables.insert(var.clone(), VarState::LocalVar);
    }

    /// Adds a call to the dup() libfunc, duplicating `var` into `dup_var`.
    fn dup(
        &mut self,
        var: &sierra::ids::VarId,
        dup_var: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) {
        self.result.push(simple_statement(
            dup_libfunc_id(self.db, ty.clone()),
            &[var.clone()],
            &[var.clone(), dup_var.clone()],
        ));
    }

    /// Adds a call to the rename() libfunc, renaming `src` to `dst`.
    fn rename_var(
        &mut self,
        src: &sierra::ids::VarId,
        dst: &sierra::ids::VarId,
        ty: &sierra::ids::ConcreteTypeId,
    ) {
        self.result.push(simple_statement(
            rename_libfunc_id(self.db, ty.clone()),
            &[src.clone()],
            &[dst.clone()],
        ));

        self.state().rename_var(src, dst);
    }

    /// Returns the current state, assuming the current statement is reachable.
    /// Fails otherwise.
    fn state(&mut self) -> &mut State {
        self.state_opt.as_mut().unwrap()
    }

    /// Same as [Self::state], except that the result is not `mut`.
    fn state_ref(&self) -> &State {
        self.state_opt.as_ref().unwrap()
    }

    /// Returns the current known stack, assuming the current statement is reachable.
    /// Fails otherwise.
    fn known_stack(&mut self) -> &mut KnownStack {
        &mut self.state().known_stack
    }

    /// Merges the given `state` into the future state that corresponds to `target`.
    /// If `target` refers to `Fallthrough`, `state` is merged into the input-output argument
    /// `fallthrough_state`.
    /// If it refers to a label, `state` is merged into `future_states`.
    fn add_future_state(
        &mut self,
        target: &GenBranchTarget<pre_sierra::LabelId>,
        state: State,
        fallthrough_state: &mut Option<State>,
    ) {
        match target {
            GenBranchTarget::Fallthrough => {
                let new_state =
                    merge_optional_states(std::mem::take(fallthrough_state), Some(state));
                *fallthrough_state = new_state;
            }
            GenBranchTarget::Statement(label_id) => {
                let new_state =
                    merge_optional_states(self.future_states.swap_remove(label_id), Some(state));
                self.future_states.insert(*label_id, extract_matches!(new_state, Some));
            }
        }
    }
}
