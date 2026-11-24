//! A bounded lexical summary of active persistent reads and writes.
//!
//! A stable write-only prefix can retain `simple`, whereas a read before a
//! later write observes a slot carried from another execution. Discover that
//! dependency before consumers are checked, including reads in an initializer.
//! This pass never executes calls. A unique, parameterless helper can supply a
//! bounded guard fact from a single effect-free expression; other function
//! bodies remain opaque rather than becoming execution sites.

use crate::{
    ast, is_cast_name, BuiltinFunctionKind, BuiltinRegistry, PureFunc, Qualifier, Span,
    MAX_ANALYSIS_NESTING,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, PartialEq)]
enum Closed {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

#[derive(Clone, Copy)]
pub(super) struct Floors {
    pub declaration: Option<Qualifier>,
    pub assignment: Qualifier,
}

#[derive(Default)]
pub(super) struct Summary {
    pub floors: HashMap<Span, Floors>,
    /// Temporal dependence is independent of an explicit qualifier ceiling.
    /// Only complete scans and separately known possible writes supply proof.
    pub known_carried_reads: HashSet<Span>,
    /// A complete scan found a carried read, or an unsafe call effect could
    /// hide one before a possibly reachable write. Normal typed HIR must
    /// discharge unresolved guards before fixed capture.
    pub potential_carried_reads: HashSet<Span>,
    #[cfg(test)]
    pub(super) operation_counts: OperationCounts,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OperationCounts {
    /// Scope-level work in unknown-effect and clear operations.
    pub scope_marker_touches: usize,
    /// Binding work proportional to branch writes, never active bindings.
    pub conditional_binding_visits: usize,
    /// Undo records inspected when collecting branch-local changes.
    pub conditional_log_records_scanned: usize,
}

#[derive(Clone, Copy)]
enum HelperGuard {
    Bool(bool),
    /// An established non-Const dependency, not proof the result changes value.
    NonConst,
}

type HelperDefinitions<'p> = HashMap<&'p str, Option<&'p ast::FuncDef>>;

#[derive(Default)]
struct Binding {
    declaration: Option<Span>,
    scope_id: usize,
    declared_unknown_epoch: u64,
    unknown_effect_open: bool,
    persistent: bool,
    explicit_qualifier: bool,
    read: bool,
    written: bool,
    carried_read: bool,
    loop_write: bool,
    unknown_effect: bool,
    closed: Option<Closed>,
    closed_epoch: u64,
    fixed: bool,
    dynamic: bool,
    known_read: bool,
    known_carried_read: bool,
    unresolved_flow: bool,
}

#[derive(Default)]
struct ScopeState {
    names: HashMap<String, usize>,
    clear_epoch: u64,
    unknown_effect_epoch: u64,
}

#[derive(Clone)]
struct ClosedState {
    value: Option<Closed>,
    epoch: u64,
    fixed: bool,
}

struct ClosedUndo {
    id: usize,
    before: ClosedState,
}

struct ClearUndo {
    scope_id: usize,
    before_epoch: u64,
}

struct BranchOutcome {
    clear_epochs: HashMap<usize, u64>,
    entries: HashMap<usize, ClosedState>,
    values: HashMap<usize, Option<Closed>>,
}

struct Scan<'r, 'p> {
    registry: &'r BuiltinRegistry,
    helpers: &'r HelperDefinitions<'p>,
    /// Scope records stay allocated after pop so bindings can retain their
    /// event epochs without an end-of-block binding scan.
    scopes: Vec<ScopeState>,
    active_scopes: Vec<usize>,
    bindings: Vec<Binding>,
    event_epoch: u64,
    branch_depth: u32,
    closed_undo: Vec<ClosedUndo>,
    clear_undo: Vec<ClearUndo>,
    loop_depth: u32,
    depth: u32,
    complete: bool,
    readonly: &'r HashSet<Span>,
    loop_written: &'r HashSet<Span>,
    unresolved_control: u32,
    dynamic_control: u32,
    #[cfg(test)]
    operation_counts: OperationCounts,
}

/// Only main-program declarations receive new support. Unresolved calls,
/// reference effects, loop writes, and helper-local declarations retain the
/// analyzer's existing conservative persistence rule.
pub(super) fn summarize(program: &ast::Program, registry: &BuiltinRegistry) -> Summary {
    // Count raw plain definitions before testing eligibility. Filtering an
    // overload, invalid duplicate or defaulted definition must not manufacture
    // a unique helper. Methods occupy a separate namespace in the analyzer.
    let mut helpers = HelperDefinitions::new();
    for item in &program.items {
        if let ast::Item::FuncDef(def) = item {
            if def.receiver.is_none() {
                helpers
                    .entry(def.name.name.as_str())
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(def));
            }
        }
    }
    // First over-approximate mutations without trusting persistent
    // initializers. Only proven unwritten scalar facts may become closed
    // aliases on the second pass. Pine value parameters cannot mutate these
    // caller scalars; unknown calls still record captured-read effects. An initially false
    // persistent guard that is later written must never prune its own flow.
    let empty = HashSet::new();
    let mut initial = Scan::new(registry, &helpers, &empty, &empty);
    initial.program(program);
    if !initial.complete {
        return Summary::default();
    }
    let readonly = initial
        .bindings
        .iter()
        .filter(|b| !b.written)
        .filter_map(|b| b.declaration)
        .collect();
    let loop_written = initial
        .bindings
        .iter()
        .filter(|b| b.loop_write)
        .filter_map(|b| b.declaration)
        .collect();
    #[cfg(test)]
    let initial_operation_counts = initial.operation_counts;
    let mut scan = Scan::new(registry, &helpers, &readonly, &loop_written);
    scan.program(program);
    // The real analyzer owns its existing nesting diagnostic. If this pure
    // summary cannot finish within that same bound, retain old behavior rather
    // than introducing a different limit or trusting an incomplete summary.
    if !scan.complete {
        return Summary::default();
    }
    let unknown_effect_epochs = scan
        .scopes
        .iter()
        .map(|scope| scope.unknown_effect_epoch)
        .collect::<Vec<_>>();
    let potential_carried_reads = scan
        .bindings
        .iter()
        .filter(|b| {
            b.persistent
                && b.written
                && (b.carried_read || has_unknown_effect(b, &unknown_effect_epochs))
        })
        .filter_map(|b| b.declaration)
        .collect();
    let known_carried_reads = scan
        .bindings
        .iter()
        .filter(|b| b.persistent && b.written && b.known_carried_read)
        .filter_map(|b| b.declaration)
        .collect();
    let floors = scan
        .bindings
        .into_iter()
        .filter(|b| b.persistent && b.written && !b.explicit_qualifier)
        .filter_map(|b| {
            b.declaration.map(|span| {
                let floor = if b.carried_read
                    || b.loop_write
                    || has_unknown_effect(&b, &unknown_effect_epochs)
                {
                    Qualifier::Series
                } else {
                    Qualifier::Simple
                };
                let floors = if b.unresolved_flow {
                    // An unsupported closed guard might be false. Preserve
                    // old early reads instead of converting uncertainty into
                    // a newly asserted backwards dependency. A separately
                    // known active carried write still proves Series.
                    Floors {
                        declaration: b.known_carried_read.then_some(Qualifier::Series),
                        assignment: Qualifier::Series,
                    }
                } else {
                    Floors {
                        declaration: Some(floor),
                        assignment: floor,
                    }
                };
                (span, floors)
            })
        })
        .collect();
    #[cfg(test)]
    let operation_counts = OperationCounts {
        scope_marker_touches: initial_operation_counts.scope_marker_touches
            + scan.operation_counts.scope_marker_touches,
        conditional_binding_visits: initial_operation_counts.conditional_binding_visits
            + scan.operation_counts.conditional_binding_visits,
        conditional_log_records_scanned: initial_operation_counts.conditional_log_records_scanned
            + scan.operation_counts.conditional_log_records_scanned,
    };
    Summary {
        floors,
        known_carried_reads,
        potential_carried_reads,
        #[cfg(test)]
        operation_counts,
    }
}

/// Function bodies are instantiated by the normal analyzer. Retain only a
/// possible temporal dependency here; parameter/capture values and overloads
/// are resolved by that actual instantiation, never by this lexical pass.
/// No declaration floor or known-Series proof is imported from this scan.
pub(super) fn function_potential_reads(
    body: &[ast::Stmt],
    registry: &BuiltinRegistry,
) -> HashSet<Span> {
    let helpers = HelperDefinitions::new();
    let empty = HashSet::new();
    let mut scan = Scan::new(registry, &helpers, &empty, &empty);
    for statement in body {
        scan.stmt(statement);
    }
    if !scan.complete {
        return HashSet::new();
    }
    let unknown_effect_epochs = scan
        .scopes
        .iter()
        .map(|scope| scope.unknown_effect_epoch)
        .collect::<Vec<_>>();
    scan.bindings
        .into_iter()
        .filter(|binding| {
            binding.persistent
                && binding.written
                && (binding.carried_read
                    || has_unknown_effect(binding, &unknown_effect_epochs)
                    || binding.loop_write)
        })
        .filter_map(|binding| binding.declaration)
        .collect()
}

fn has_unknown_effect(binding: &Binding, scope_epochs: &[u64]) -> bool {
    binding.unknown_effect
        || (binding.unknown_effect_open
            && scope_epochs[binding.scope_id] > binding.declared_unknown_epoch)
}

impl<'r, 'p> Scan<'r, 'p> {
    fn new(
        registry: &'r BuiltinRegistry,
        helpers: &'r HelperDefinitions<'p>,
        readonly: &'r HashSet<Span>,
        loop_written: &'r HashSet<Span>,
    ) -> Self {
        Self {
            registry,
            helpers,
            readonly,
            loop_written,
            scopes: vec![ScopeState::default()],
            active_scopes: vec![0],
            bindings: Vec::new(),
            event_epoch: 0,
            branch_depth: 0,
            closed_undo: Vec::new(),
            clear_undo: Vec::new(),
            loop_depth: 0,
            depth: 0,
            complete: true,
            unresolved_control: 0,
            dynamic_control: 0,
            #[cfg(test)]
            operation_counts: OperationCounts::default(),
        }
    }

    fn program(&mut self, program: &ast::Program) {
        for item in &program.items {
            match item {
                ast::Item::Stmt(stmt) => self.stmt(stmt),
                ast::Item::Indicator(decl) => self.args(&decl.args),
                ast::Item::Strategy(decl) => self.args(&decl.args),
                ast::Item::Library(decl) => self.args(&decl.args),
                // Definitions are templates, not execution sites.
                ast::Item::FuncDef(_)
                | ast::Item::EnumDef(_)
                | ast::Item::TypeDef(_)
                | ast::Item::Import(_) => {}
            }
        }
    }
    fn enter(&mut self) -> bool {
        if self.depth >= MAX_ANALYSIS_NESTING {
            self.complete = false;
            return false;
        }
        self.depth += 1;
        true
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.active_scopes
            .iter()
            .rev()
            .find_map(|&scope_id| self.scopes[scope_id].names.get(name).copied())
    }

    fn tick(&mut self) -> u64 {
        self.event_epoch += 1;
        self.event_epoch
    }

    fn push_scope(&mut self) {
        let scope_id = self.scopes.len();
        self.scopes.push(ScopeState::default());
        self.active_scopes.push(scope_id);
    }

    fn pop_scope(&mut self) {
        debug_assert!(self.active_scopes.len() > 1);
        let scope_id = self.active_scopes.pop().unwrap();
        // Keep only the small event summary after a block ends. Its names are
        // no longer visible to lookup, and retaining each popped HashMap's
        // buckets would turn this optimization into unnecessary per-block memory.
        drop(std::mem::take(&mut self.scopes[scope_id].names));
    }

    fn declare(&mut self, name: &ast::Ident, mut binding: Binding) {
        let scope_id = *self.active_scopes.last().unwrap();
        if let Some(previous) = self.scopes[scope_id].names.get(&name.name).copied() {
            // Invalid duplicate declarations remain the analyzer's diagnostic.
            // Freeze whether the previous visible declaration had seen an
            // unresolved effect before this name stops being active.
            let effect = self.binding_has_unknown_effect(previous);
            let previous = &mut self.bindings[previous];
            previous.unknown_effect = effect;
            previous.unknown_effect_open = false;
        }
        binding.scope_id = scope_id;
        binding.declared_unknown_epoch = self.scopes[scope_id].unknown_effect_epoch;
        binding.unknown_effect_open = true;
        if binding.closed.is_some() {
            binding.closed_epoch = self.tick();
        } else {
            binding.closed_epoch = self.event_epoch;
        }
        let id = self.bindings.len();
        self.bindings.push(Binding {
            declaration: Some(name.span),
            ..binding
        });
        // Invalid duplicate declarations remain the analyzer's diagnostic.
        self.scopes[scope_id].names.insert(name.name.clone(), id);
    }

    fn binding_has_unknown_effect(&self, id: usize) -> bool {
        let binding = &self.bindings[id];
        binding.unknown_effect
            || (binding.unknown_effect_open
                && self.scopes[binding.scope_id].unknown_effect_epoch
                    > binding.declared_unknown_epoch)
    }

    fn closed_state(&self, id: usize) -> ClosedState {
        let binding = &self.bindings[id];
        ClosedState {
            value: binding.closed.clone(),
            epoch: binding.closed_epoch,
            fixed: binding.fixed,
        }
    }

    fn state_value_at_clear(state: &ClosedState, clear_epoch: u64) -> Option<Closed> {
        if !state.fixed && state.epoch < clear_epoch {
            None
        } else {
            state.value.clone()
        }
    }

    fn closed_now(&self, id: usize) -> Option<Closed> {
        let state = self.closed_state(id);
        let clear_epoch = self.scopes[self.bindings[id].scope_id].clear_epoch;
        Self::state_value_at_clear(&state, clear_epoch)
    }

    fn set_closed(&mut self, id: usize, value: Option<Closed>) {
        let before = self.closed_state(id);
        if self.branch_depth > 0 {
            self.closed_undo.push(ClosedUndo { id, before });
        }
        let epoch = self.tick();
        let binding = &mut self.bindings[id];
        binding.closed = value;
        binding.closed_epoch = epoch;
    }

    fn set_scope_clear_epoch(&mut self, scope_id: usize, epoch: u64) {
        let before_epoch = self.scopes[scope_id].clear_epoch;
        if before_epoch == epoch {
            return;
        }
        if self.branch_depth > 0 {
            self.clear_undo.push(ClearUndo {
                scope_id,
                before_epoch,
            });
        }
        self.scopes[scope_id].clear_epoch = epoch;
    }

    fn restore_closed(&mut self, mark: usize) {
        while self.closed_undo.len() > mark {
            let undo = self.closed_undo.pop().unwrap();
            let binding = &mut self.bindings[undo.id];
            binding.closed = undo.before.value;
            binding.closed_epoch = undo.before.epoch;
        }
    }

    fn restore_clear_epochs(&mut self, mark: usize) {
        while self.clear_undo.len() > mark {
            let undo = self.clear_undo.pop().unwrap();
            self.scopes[undo.scope_id].clear_epoch = undo.before_epoch;
        }
    }

    fn observe_branch(&mut self, mark: usize, parent_scopes: &[usize]) -> BranchOutcome {
        let parent_scope_ids = parent_scopes.iter().copied().collect::<HashSet<_>>();
        let mut clear_epochs = HashMap::with_capacity(parent_scopes.len());
        for &scope_id in parent_scopes {
            clear_epochs.insert(scope_id, self.scopes[scope_id].clear_epoch);
        }
        let mut entries = HashMap::new();
        for undo in &self.closed_undo[mark..] {
            if parent_scope_ids.contains(&self.bindings[undo.id].scope_id) {
                entries
                    .entry(undo.id)
                    .or_insert_with(|| undo.before.clone());
            }
        }
        #[cfg(test)]
        {
            self.operation_counts.scope_marker_touches += parent_scopes.len();
            self.operation_counts.conditional_binding_visits += entries.len();
            self.operation_counts.conditional_log_records_scanned += self.closed_undo.len() - mark;
        }
        let values = entries
            .keys()
            .map(|&id| (id, self.closed_now(id)))
            .collect();
        BranchOutcome {
            clear_epochs,
            entries,
            values,
        }
    }

    fn branch_clear_epoch(outcome: &BranchOutcome, scope_id: usize) -> u64 {
        outcome.clear_epochs[&scope_id]
    }

    fn finish_outermost_branch(&mut self, closed_mark: usize, clear_mark: usize) {
        if self.branch_depth == 0 {
            self.closed_undo.truncate(closed_mark);
            self.clear_undo.truncate(clear_mark);
        }
    }

    fn read(&mut self, name: &str) {
        // Dotted field/method receivers are reads of the resolved head; a
        // namespace with no variable binding contributes no scalar read.
        let head = name.split('.').next().unwrap_or(name);
        if let Some(id) = self.lookup(head) {
            let binding = &mut self.bindings[id];
            binding.read = true;
            binding.known_read |= self.unresolved_control == 0;
            binding.unresolved_flow |= self.unresolved_control > 0;
        }
    }

    fn unknown_effect(&mut self) {
        let epoch = self.tick();
        for index in 0..self.active_scopes.len() {
            let scope_id = self.active_scopes[index];
            self.scopes[scope_id].unknown_effect_epoch = epoch;
            self.set_scope_clear_epoch(scope_id, epoch);
        }
        #[cfg(test)]
        {
            self.operation_counts.scope_marker_touches += self.active_scopes.len();
        }
    }

    fn clear_nonfixed(&mut self) {
        let epoch = self.tick();
        for index in 0..self.active_scopes.len() {
            let scope_id = self.active_scopes[index];
            self.set_scope_clear_epoch(scope_id, epoch);
        }
        #[cfg(test)]
        {
            self.operation_counts.scope_marker_touches += self.active_scopes.len();
        }
    }

    fn args(&mut self, args: &[ast::CallArg]) {
        for arg in args {
            self.expr(&arg.value);
        }
    }

    fn block(&mut self, body: &[ast::Stmt]) {
        self.push_scope();
        for stmt in body {
            self.stmt(stmt);
        }
        self.pop_scope();
    }

    fn conditional(
        &mut self,
        cond: &ast::Expr,
        then_branch: &[ast::Stmt],
        else_branch: Option<&[ast::Stmt]>,
    ) {
        self.conditional_with_activation(cond, then_branch, else_branch, false);
    }

    fn conditional_with_activation(
        &mut self,
        cond: &ast::Expr,
        then_branch: &[ast::Stmt],
        else_branch: Option<&[ast::Stmt]>,
        activation_guard: bool,
    ) {
        self.expr(cond);
        let source_known = self.control_bool(cond, self.depth);
        // Even a literal-true Once is gated by its persistent activation:
        // reads before the structure can carry its write on later bars.
        let known = if activation_guard && source_known != Some(false) {
            None
        } else {
            source_known
        };
        let dynamic = activation_guard || (known.is_none() && self.dynamic_expr(cond, 0));
        let unresolved = known.is_none() && !dynamic;
        self.unresolved_control += u32::from(unresolved);
        self.dynamic_control += u32::from(dynamic);
        match known {
            Some(true) => self.block(then_branch),
            Some(false) => {
                if let Some(body) = else_branch {
                    self.block(body);
                }
            }
            None => {
                let parent_scopes = self.active_scopes.clone();
                let base_clear_epochs = parent_scopes
                    .iter()
                    .map(|&scope_id| (scope_id, self.scopes[scope_id].clear_epoch))
                    .collect::<HashMap<_, _>>();
                #[cfg(test)]
                {
                    self.operation_counts.scope_marker_touches += parent_scopes.len();
                }
                let closed_mark = self.closed_undo.len();
                let clear_mark = self.clear_undo.len();
                self.branch_depth += 1;
                self.block(then_branch);
                let left = self.observe_branch(closed_mark, &parent_scopes);
                self.restore_closed(closed_mark);
                self.restore_clear_epochs(clear_mark);
                // Fixedness is intentionally monotone across branches in this
                // summary. Capture the right entry only for bindings changed
                // on the left, so its state includes that existing behavior.
                let right_entry_for_left = left
                    .values
                    .keys()
                    .map(|&id| (id, self.closed_state(id)))
                    .collect::<HashMap<_, _>>();
                if let Some(body) = else_branch {
                    self.block(body);
                }
                let right = self.observe_branch(closed_mark, &parent_scopes);

                // A clear invalidates untouched nonfixed values across the
                // join. Dirty bindings are merged separately and written
                // after the joined marker, which handles same-value resets.
                for &scope_id in &parent_scopes {
                    let base = base_clear_epochs[&scope_id];
                    let left_cleared = Self::branch_clear_epoch(&left, scope_id) > base;
                    let right_cleared = Self::branch_clear_epoch(&right, scope_id) > base;
                    if left_cleared || right_cleared {
                        let epoch = self.tick();
                        self.set_scope_clear_epoch(scope_id, epoch);
                    }
                }
                #[cfg(test)]
                {
                    self.operation_counts.scope_marker_touches += parent_scopes.len();
                }

                let mut dirty = left.values.keys().copied().collect::<HashSet<_>>();
                dirty.extend(right.values.keys().copied());
                #[cfg(test)]
                {
                    self.operation_counts.conditional_binding_visits += dirty.len();
                }
                for id in dirty {
                    let scope_id = self.bindings[id].scope_id;
                    let left_value = left.values.get(&id).cloned().unwrap_or_else(|| {
                        let state = &right.entries[&id];
                        Self::state_value_at_clear(state, Self::branch_clear_epoch(&left, scope_id))
                    });
                    let right_value = right.values.get(&id).cloned().unwrap_or_else(|| {
                        let state = &right_entry_for_left[&id];
                        Self::state_value_at_clear(
                            state,
                            Self::branch_clear_epoch(&right, scope_id),
                        )
                    });
                    self.set_closed(
                        id,
                        if left_value == right_value {
                            left_value
                        } else {
                            None
                        },
                    );
                }
                self.branch_depth -= 1;
                self.finish_outermost_branch(closed_mark, clear_mark);
            }
        }
        self.unresolved_control -= u32::from(unresolved);
        self.dynamic_control -= u32::from(dynamic);
    }

    fn switch(&mut self, switch: &ast::SwitchStmt) {
        if let Some(subject) = &switch.subject {
            self.expr(subject);
        }
        let subject = switch
            .subject
            .as_ref()
            .and_then(|e| self.closed_value(e, 0));
        let mut unresolved_prior = false;
        let mut dynamic_prior = false;
        for arm in &switch.arms {
            // A preceding alternative's ordinary writes are not facts in a
            // later alternative. Keep only immutable closed aliases here.
            self.clear_nonfixed();
            let known = match &arm.label {
                Some(label) => {
                    self.expr(label);
                    match &switch.subject {
                        None => self.control_bool(label, self.depth),
                        Some(_) => subject.clone().zip(self.closed_value(label, 0)).and_then(
                            |(subject, label)| closed_compare(ast::BinaryOp::Eq, &subject, &label),
                        ),
                    }
                }
                None => Some(true),
            };
            let dynamic = known.is_none()
                && (switch
                    .subject
                    .as_ref()
                    .is_some_and(|e| self.dynamic_expr(e, 0))
                    || arm.label.as_ref().is_some_and(|e| self.dynamic_expr(e, 0)));
            let unresolved = known.is_none() && !dynamic;
            if known != Some(false) {
                self.unresolved_control += u32::from(unresolved_prior || unresolved);
                self.dynamic_control += u32::from(dynamic_prior || dynamic);
                self.block(&arm.body);
                self.unresolved_control -= u32::from(unresolved_prior || unresolved);
                self.dynamic_control -= u32::from(dynamic_prior || dynamic);
            }
            if known == Some(true) {
                break;
            }
            unresolved_prior |= unresolved;
            dynamic_prior |= dynamic;
        }
        self.clear_nonfixed();
    }

    fn stmt(&mut self, stmt: &ast::Stmt) {
        if !self.enter() {
            return;
        }
        match stmt {
            ast::Stmt::Assign {
                name,
                value,
                is_var,
                is_varip,
                annotation,
                ..
            } => {
                // An initializer resolves the surrounding scope before its
                // own declaration shadows an outer name.
                self.expr(value);
                let persistent = *is_var || *is_varip;
                let qualifier = annotation.as_ref().and_then(|a| a.qualifier);
                let fixed = self.readonly.contains(&name.span)
                    || annotation
                        .as_ref()
                        .is_some_and(|a| a.qualifier == Some(ast::TypeQualifier::Const));
                let closed = ((!persistent || fixed)
                    && matches!(qualifier, None | Some(ast::TypeQualifier::Const)))
                .then(|| self.closed_value(value, 0))
                .flatten();
                let dynamic = (persistent && !self.readonly.contains(&name.span))
                    || self.loop_written.contains(&name.span)
                    || matches!(
                        qualifier,
                        Some(ast::TypeQualifier::Simple | ast::TypeQualifier::Series)
                    )
                    || self.dynamic_expr(value, 0)
                    || self.dynamic_control > 0;
                self.declare(
                    name,
                    Binding {
                        persistent,
                        explicit_qualifier: qualifier.is_some(),
                        closed,
                        fixed,
                        dynamic,
                        ..Binding::default()
                    },
                );
            }
            ast::Stmt::Reassign { name, value, .. } => {
                // A compound assignment's desugared RHS includes its read.
                self.expr(value);
                let closed = self.closed_value(value, 0);
                let dynamic = self.dynamic_expr(value, 0) || self.dynamic_control > 0;
                if name.name.contains('.') {
                    self.read(&name.name);
                    self.unknown_effect();
                } else if let Some(id) = self.lookup(&name.name) {
                    let (persistent, explicit_qualifier) = {
                        let binding = &mut self.bindings[id];
                        binding.written = true;
                        binding.carried_read |= binding.read;
                        binding.known_carried_read |=
                            binding.known_read && self.unresolved_control == 0;
                        binding.unresolved_flow |= self.unresolved_control > 0;
                        binding.loop_write |= self.loop_depth > 0;
                        binding.dynamic |= dynamic;
                        (binding.persistent, binding.explicit_qualifier)
                    };
                    // Ordinary declarations reinitialize every execution;
                    // their currently known value can fold a later guard.
                    // A mutated persistent initializer is never such proof.
                    let closed = if persistent || explicit_qualifier {
                        None
                    } else {
                        closed
                    };
                    self.set_closed(id, closed);
                    self.bindings[id].fixed = false;
                }
            }
            ast::Stmt::Destructure { targets, value, .. } => {
                self.expr(value);
                for target in targets {
                    self.declare(
                        target,
                        Binding {
                            dynamic: true,
                            ..Binding::default()
                        },
                    );
                }
            }
            ast::Stmt::Expr { expr, .. } => self.expr(expr),
            ast::Stmt::If(branch) => self.conditional(
                &branch.cond,
                &branch.then_branch,
                branch.else_branch.as_deref(),
            ),
            ast::Stmt::Once(once) => {
                let default = ast::Expr::Bool {
                    value: true,
                    span: once.span,
                };
                self.conditional_with_activation(
                    once.cond.as_ref().unwrap_or(&default),
                    &once.body,
                    None,
                    true,
                );
            }
            ast::Stmt::Switch(switch) => self.switch(switch),
            ast::Stmt::For(loop_) => {
                self.expr(&loop_.start);
                self.expr(&loop_.end);
                if let Some(step) = &loop_.step {
                    self.expr(step);
                }
                self.clear_nonfixed();
                self.push_scope();
                self.declare(
                    &loop_.counter,
                    Binding {
                        dynamic: true,
                        ..Binding::default()
                    },
                );
                self.loop_depth += 1;
                for stmt in &loop_.body {
                    self.stmt(stmt);
                }
                self.loop_depth -= 1;
                self.pop_scope();
                self.clear_nonfixed();
            }
            ast::Stmt::ForIn(loop_) => {
                self.expr(&loop_.iterable);
                self.clear_nonfixed();
                self.push_scope();
                if let Some(index) = &loop_.index {
                    self.declare(
                        index,
                        Binding {
                            dynamic: true,
                            ..Binding::default()
                        },
                    );
                }
                self.declare(
                    &loop_.element,
                    Binding {
                        dynamic: true,
                        ..Binding::default()
                    },
                );
                self.loop_depth += 1;
                for stmt in &loop_.body {
                    self.stmt(stmt);
                }
                self.loop_depth -= 1;
                self.pop_scope();
                self.clear_nonfixed();
            }
            ast::Stmt::While(loop_) => {
                self.expr(&loop_.cond);
                let known = self.control_bool(&loop_.cond, self.depth);
                let dynamic = known.is_none() && self.dynamic_expr(&loop_.cond, 0);
                let unresolved = known.is_none() && !dynamic;
                if known != Some(false) {
                    self.clear_nonfixed();
                    self.loop_depth += 1;
                    self.unresolved_control += u32::from(unresolved);
                    self.dynamic_control += u32::from(dynamic);
                    self.block(&loop_.body);
                    self.unresolved_control -= u32::from(unresolved);
                    self.dynamic_control -= u32::from(dynamic);
                    self.loop_depth -= 1;
                    self.clear_nonfixed();
                }
            }
            ast::Stmt::Break { .. } | ast::Stmt::Continue { .. } => {}
        }
        self.depth -= 1;
    }

    fn expr(&mut self, expr: &ast::Expr) {
        if !self.enter() {
            return;
        }
        match expr {
            ast::Expr::Int { .. }
            | ast::Expr::Float { .. }
            | ast::Expr::Bool { .. }
            | ast::Expr::Na { .. }
            | ast::Expr::Str { .. }
            | ast::Expr::Color { .. } => {}
            ast::Expr::Ident(ident) => self.read(&ident.name),
            ast::Expr::Unary { operand, .. } => self.expr(operand),
            ast::Expr::Group { inner, .. } => self.expr(inner),
            ast::Expr::Binary { op, lhs, rhs, .. } => {
                self.expr(lhs);
                let known = self.control_bool(lhs, self.depth);
                let skipped = matches!(
                    (op, known),
                    (ast::BinaryOp::And, Some(false)) | (ast::BinaryOp::Or, Some(true))
                );
                if !skipped {
                    let conditional =
                        matches!(op, ast::BinaryOp::And | ast::BinaryOp::Or) && known.is_none();
                    let dynamic = conditional && self.dynamic_expr(lhs, 0);
                    let unresolved = conditional && !dynamic;
                    self.unresolved_control += u32::from(unresolved);
                    self.dynamic_control += u32::from(dynamic);
                    self.expr(rhs);
                    self.unresolved_control -= u32::from(unresolved);
                    self.dynamic_control -= u32::from(dynamic);
                }
            }
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                self.expr(cond);
                let known = self.control_bool(cond, self.depth);
                let dynamic = known.is_none() && self.dynamic_expr(cond, 0);
                let unresolved = known.is_none() && !dynamic;
                self.unresolved_control += u32::from(unresolved);
                self.dynamic_control += u32::from(dynamic);
                if known != Some(false) {
                    self.expr(then_e);
                }
                if known != Some(true) {
                    self.expr(else_e);
                }
                self.unresolved_control -= u32::from(unresolved);
                self.dynamic_control -= u32::from(dynamic);
            }
            ast::Expr::Call {
                callee,
                args,
                type_args,
                ..
            } => {
                self.args(args);
                // Bare cast dispatch precedes user-function/type fallback.
                // Casts are pure, but their arguments still need the scan above.
                let builtin_cast = type_args.is_empty() && is_cast_name(&callee.name);
                if !builtin_cast && self.registry.lookup_function(&callee.name).is_none() {
                    self.read(&callee.name);
                    self.unknown_effect();
                }
            }
            ast::Expr::MethodCall { receiver, args, .. } => {
                self.expr(receiver);
                self.args(args);
                self.unknown_effect();
            }
            ast::Expr::FieldAccess { receiver, .. } => self.expr(receiver),
            ast::Expr::History { target, offset, .. } => {
                self.expr(target);
                self.expr(offset);
            }
            ast::Expr::If(branch) => self.conditional(
                &branch.cond,
                &branch.then_branch,
                branch.else_branch.as_deref(),
            ),
            ast::Expr::Switch(switch) => self.switch(switch),
            ast::Expr::Loop(stmt) => self.stmt(stmt),
            ast::Expr::Tuple { elements, .. } => {
                for element in elements {
                    self.expr(element);
                }
            }
        }
        self.depth -= 1;
    }

    fn closed_bool(&self, expr: &ast::Expr) -> Option<bool> {
        match self.closed_value(expr, 0)? {
            Closed::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// Helper truth is usable only for an actual control. Scalar aliases keep
    /// the old closed facts, and `dynamic_expr` must not prune an initializer's
    /// dependency using a helper fact that the alias itself does not retain.
    fn control_bool(&self, expr: &ast::Expr, depth: u32) -> Option<bool> {
        if let Some(value) = self.closed_bool(expr) {
            return Some(value);
        }
        if depth >= MAX_ANALYSIS_NESTING {
            return None;
        }
        match expr {
            ast::Expr::Group { inner, .. } => self.control_bool(inner, depth + 1),
            ast::Expr::Unary {
                op: ast::UnaryOp::Not,
                operand,
                ..
            } => self.control_bool(operand, depth + 1).map(|value| !value),
            ast::Expr::Binary { op, lhs, rhs, .. }
                if matches!(op, ast::BinaryOp::And | ast::BinaryOp::Or) =>
            {
                let left = self.control_bool(lhs, depth + 1)?;
                match op {
                    ast::BinaryOp::And if !left => Some(false),
                    ast::BinaryOp::Or if left => Some(true),
                    _ => self.control_bool(rhs, depth + 1),
                }
            }
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                if self.control_bool(cond, depth + 1)? {
                    self.control_bool(then_e, depth + 1)
                } else {
                    self.control_bool(else_e, depth + 1)
                }
            }
            ast::Expr::Call { .. } => match self.helper_guard(expr, depth)? {
                HelperGuard::Bool(value) => Some(value),
                HelperGuard::NonConst => None,
            },
            _ => None,
        }
    }

    /// Inspect one unambiguous, zero-argument plain helper, without expanding a
    /// call graph or interpreting captures. Every unsupported shape is opaque.
    fn helper_guard(&self, expr: &ast::Expr, depth: u32) -> Option<HelperGuard> {
        if depth >= MAX_ANALYSIS_NESTING {
            return None;
        }
        let ast::Expr::Call {
            callee,
            args,
            type_args,
            ..
        } = expr
        else {
            return None;
        };
        if !args.is_empty()
            || !type_args.is_empty()
            || callee.name.contains('.')
            || is_cast_name(&callee.name)
            || self.registry.lookup_function(&callee.name).is_some()
            || self.registry.lookup_series(&callee.name).is_some()
            || self.registry.lookup_constant(&callee.name).is_some()
            || self.lookup(&callee.name).is_some()
        {
            return None;
        }
        let def = self.helpers.get(callee.name.as_str()).copied().flatten()?;
        if !def.params.is_empty() || def.receiver.is_some() || def.exported {
            return None;
        }
        let [ast::Stmt::Expr { expr: body, .. }] = def.body.as_slice() else {
            return None;
        };
        self.helper_body_guard(body, depth + 1)
    }

    fn helper_body_guard(&self, expr: &ast::Expr, depth: u32) -> Option<HelperGuard> {
        if depth >= MAX_ANALYSIS_NESTING {
            return None;
        }
        match expr {
            ast::Expr::Bool { value, .. } => Some(HelperGuard::Bool(*value)),
            ast::Expr::Group { inner, .. } => self.helper_body_guard(inner, depth + 1),
            ast::Expr::Unary {
                op: ast::UnaryOp::Not,
                operand,
                ..
            } => self
                .helper_body_guard(operand, depth + 1)
                .map(|fact| match fact {
                    HelperGuard::Bool(value) => HelperGuard::Bool(!value),
                    HelperGuard::NonConst => HelperGuard::NonConst,
                }),
            ast::Expr::Binary {
                op:
                    ast::BinaryOp::Lt
                    | ast::BinaryOp::Le
                    | ast::BinaryOp::Gt
                    | ast::BinaryOp::Ge
                    | ast::BinaryOp::Eq
                    | ast::BinaryOp::Ne,
                lhs,
                rhs,
                ..
            } => {
                let left = self.helper_numeric_operand(lhs, depth + 1)?;
                let right = self.helper_numeric_operand(rhs, depth + 1)?;
                (left || right).then_some(HelperGuard::NonConst)
            }
            // No calls (including recursive or transitive helpers), local
            // statements, reference access, history, allocation or controls.
            _ => None,
        }
    }

    /// Both operands must be recognized. A numeric builtin supplies a
    /// non-Const dependency; numeric literals alone supply no new truth fact.
    fn helper_numeric_operand(&self, expr: &ast::Expr, depth: u32) -> Option<bool> {
        if depth >= MAX_ANALYSIS_NESTING {
            return None;
        }
        match expr {
            ast::Expr::Int { .. } => Some(false),
            ast::Expr::Float { value, .. } if value.is_finite() => Some(false),
            ast::Expr::Group { inner, .. } => self.helper_numeric_operand(inner, depth + 1),
            ast::Expr::Ident(ident)
                if !ident.name.contains('.') && self.lookup(&ident.name).is_none() =>
            {
                self.registry
                    .lookup_series(&ident.name)
                    .filter(|series| !series.is_bool() && !series.is_string())
                    .map(|_| true)
            }
            // A helper resolves any other name at its definition, which this
            // call-site scan cannot tell apart from a caller's local of the
            // same name. Never interpret one using a caller's closed fact.
            _ => None,
        }
    }

    /// Recognize established non-Const data dependencies without evaluating
    /// calls. Unsupported closed expressions remain opaque, so they can
    /// retain the old early qualifier instead of being asserted active.
    fn dynamic_expr(&self, expr: &ast::Expr, depth: u32) -> bool {
        if depth >= MAX_ANALYSIS_NESTING {
            return false;
        }
        match expr {
            ast::Expr::Ident(ident) => {
                if let Some(id) = self.lookup(&ident.name) {
                    self.bindings[id].dynamic
                } else if ident
                    .name
                    .split_once('.')
                    .is_some_and(|(head, _)| self.lookup(head).is_some())
                {
                    true
                } else {
                    self.registry.lookup_series(&ident.name).is_some()
                        || self
                            .registry
                            .lookup_constant(&ident.name)
                            .is_some_and(|c| c.qualifier() > Qualifier::Const)
                }
            }
            ast::Expr::Unary { operand, .. } => self.dynamic_expr(operand, depth + 1),
            ast::Expr::Group { inner, .. } => self.dynamic_expr(inner, depth + 1),
            ast::Expr::Binary { lhs, rhs, .. } => {
                self.dynamic_expr(lhs, depth + 1) || self.dynamic_expr(rhs, depth + 1)
            }
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                self.dynamic_expr(cond, depth + 1)
                    || (self.closed_bool(cond) != Some(false)
                        && self.dynamic_expr(then_e, depth + 1))
                    || (self.closed_bool(cond) != Some(true)
                        && self.dynamic_expr(else_e, depth + 1))
            }
            ast::Expr::Call { callee, args, .. } => {
                let intrinsic =
                    self.registry
                        .lookup_function(&callee.name)
                        .is_some_and(|f| match &f.kind {
                            BuiltinFunctionKind::Pure(pure) => {
                                matches!(pure, PureFunc::MathRandom | PureFunc::StrFormatTime)
                                    || pure.is_strategy()
                            }
                            _ => true,
                        });
                intrinsic
                    || matches!(
                        self.helper_guard(expr, depth.saturating_add(self.depth)),
                        Some(HelperGuard::NonConst)
                    )
                    || args.iter().any(|a| self.dynamic_expr(&a.value, depth + 1))
            }
            ast::Expr::MethodCall { .. }
            | ast::Expr::FieldAccess { .. }
            | ast::Expr::History { .. } => true,
            // A value block needs full semantic/call analysis. Abstain from
            // new backwards qualification rather than inventing its result.
            ast::Expr::If(_)
            | ast::Expr::Switch(_)
            | ast::Expr::Loop(_)
            | ast::Expr::Tuple { .. }
            | ast::Expr::Int { .. }
            | ast::Expr::Float { .. }
            | ast::Expr::Bool { .. }
            | ast::Expr::Na { .. }
            | ast::Expr::Str { .. }
            | ast::Expr::Color { .. } => false,
        }
    }

    /// Closed literals, comparisons and lexical aliases only. Calls, input
    /// defaults, series data, arithmetic and mutable persistent guards are
    /// deliberately not interpreted by this pass.
    fn closed_value(&self, expr: &ast::Expr, depth: u32) -> Option<Closed> {
        if depth >= MAX_ANALYSIS_NESTING {
            return None;
        }
        match expr {
            ast::Expr::Bool { value, .. } => Some(Closed::Bool(*value)),
            ast::Expr::Int { value, .. } => Some(Closed::Int(*value)),
            ast::Expr::Float { value, .. } if value.is_finite() => Some(Closed::Float(*value)),
            ast::Expr::Str { value, .. } => Some(Closed::String(value.clone())),
            ast::Expr::Ident(ident) => self.lookup(&ident.name).and_then(|id| self.closed_now(id)),
            ast::Expr::Group { inner, .. } => self.closed_value(inner, depth + 1),
            ast::Expr::Unary {
                op: ast::UnaryOp::Not,
                operand,
                ..
            } => match self.closed_value(operand, depth + 1)? {
                Closed::Bool(v) => Some(Closed::Bool(!v)),
                _ => None,
            },
            ast::Expr::Binary { op, lhs, rhs, .. } => {
                let lhs = self.closed_value(lhs, depth + 1)?;
                match (op, &lhs) {
                    (ast::BinaryOp::And, Closed::Bool(false)) => Some(Closed::Bool(false)),
                    (ast::BinaryOp::Or, Closed::Bool(true)) => Some(Closed::Bool(true)),
                    (ast::BinaryOp::And | ast::BinaryOp::Or, Closed::Bool(_)) => {
                        match self.closed_value(rhs, depth + 1)? {
                            Closed::Bool(v) => Some(Closed::Bool(v)),
                            _ => None,
                        }
                    }
                    _ => closed_compare(*op, &lhs, &self.closed_value(rhs, depth + 1)?)
                        .map(Closed::Bool),
                }
            }
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => match self.closed_value(cond, depth + 1)? {
                Closed::Bool(true) => self.closed_value(then_e, depth + 1),
                Closed::Bool(false) => self.closed_value(else_e, depth + 1),
                _ => None,
            },
            _ => None,
        }
    }
}

fn closed_compare(op: ast::BinaryOp, lhs: &Closed, rhs: &Closed) -> Option<bool> {
    use std::cmp::Ordering;
    // In this finite-literal folding path, float Eq/Ne uses an absolute
    // tolerance, so 5e-11 and zero compare equal. The literals 1.0000000001
    // and 1.0 remain distinct because their binary64 difference is slightly
    // above the tolerance. Mixed-type comparisons are not folded here.
    if let (Closed::Float(lhs), Closed::Float(rhs)) = (lhs, rhs) {
        return match op {
            ast::BinaryOp::Eq => Some(pinewasm_num::comparison::eq(*lhs, *rhs)),
            ast::BinaryOp::Ne => Some(!pinewasm_num::comparison::eq(*lhs, *rhs)),
            _ => None,
        };
    }
    let ordering = match (lhs, rhs) {
        (Closed::Bool(a), Closed::Bool(b)) => Some(a.cmp(b)),
        (Closed::Int(a), Closed::Int(b)) => Some(a.cmp(b)),
        (Closed::String(a), Closed::String(b)) => Some(a.cmp(b)),
        _ => None,
    }?;
    Some(match op {
        ast::BinaryOp::Eq => ordering == Ordering::Equal,
        ast::BinaryOp::Ne => ordering != Ordering::Equal,
        ast::BinaryOp::Lt => ordering == Ordering::Less,
        ast::BinaryOp::Le => ordering != Ordering::Greater,
        ast::BinaryOp::Gt => ordering == Ordering::Greater,
        ast::BinaryOp::Ge => ordering != Ordering::Less,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_parse::parse;

    fn summarize_source(source: &str) -> (ast::Program, Summary) {
        let parsed = parse(source);
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.is_error()),
            "test Pine source must parse: {:?}",
            parsed.diagnostics
        );
        let summary = summarize(&parsed.program, BuiltinRegistry::standard_ref());
        (parsed.program, summary)
    }

    fn declaration_span(program: &ast::Program, name: &str) -> Span {
        program
            .items
            .iter()
            .find_map(|item| match item {
                ast::Item::Stmt(ast::Stmt::Assign { name: ident, .. }) if ident.name == name => {
                    Some(ident.span)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing declaration for {name}"))
    }

    #[test]
    fn unknown_effect_invalidates_prior_aliases_but_not_later_declarations() {
        let source = r#"//@version=6
indicator("Unknown effect epochs")
bool disabled = false
disabled := false
mystery()
var int guarded = 1
if disabled
    guarded := 2
var int later = 1
later := 2
"#;
        let (program, summary) = summarize_source(source);
        let guarded = summary
            .floors
            .get(&declaration_span(&program, "guarded"))
            .expect("guarded persistent write");
        assert_eq!(guarded.declaration, None);
        assert_eq!(guarded.assignment, Qualifier::Series);
        let later = summary
            .floors
            .get(&declaration_span(&program, "later"))
            .expect("later persistent write");
        assert_eq!(later.declaration, Some(Qualifier::Simple));
        assert_eq!(later.assignment, Qualifier::Simple);
    }

    #[test]
    fn conditional_join_keeps_equal_closed_values_and_rejects_mismatches() {
        let same = r#"//@version=6
indicator("Equal branch facts")
bool guard = false
if close > open
    guard := false
else
    guard := false
var int shift = 1
if guard
    shift := 2
"#;
        let (_, same_summary) = summarize_source(same);
        assert!(
            same_summary.floors.is_empty(),
            "expected no floors, got {}",
            same_summary.floors.len()
        );

        let different = r#"//@version=6
indicator("Different branch facts")
bool guard = false
if close > open
    guard := false
else
    guard := true
var int shift = 1
if guard
    shift := 2
"#;
        let (_, different_summary) = summarize_source(different);
        assert_eq!(different_summary.floors.len(), 1);
    }

    #[test]
    fn conditional_clear_merges_lazily_and_dirty_equal_reset_survives() {
        let reset = r#"//@version=6
indicator("Branch clear with equal reset")
bool guard = false
if close > open
    for i = 0 to 0
        int local = i
    guard := false
var int shift = 1
if guard
    shift := 2
"#;
        let (_, reset_summary) = summarize_source(reset);
        assert!(
            reset_summary.floors.is_empty(),
            "expected no floors, got {}",
            reset_summary.floors.len()
        );

        let cleared = r#"//@version=6
indicator("One branch clear")
bool guard = false
guard := false
if close > open
    for i = 0 to 0
        int local = i
var int shift = 1
if guard
    shift := 2
"#;
        let (_, cleared_summary) = summarize_source(cleared);
        assert_eq!(cleared_summary.floors.len(), 1);
    }

    #[test]
    fn nested_if_switch_shadow_does_not_escape_its_popped_scopes() {
        let source = r#"//@version=6
indicator("Nested scope shadow")
bool guard = false
if true
    bool guard = true
    switch close > open
        true =>
            int arm = 1
        =>
            int arm = 2
var int shift = 1
if guard
    shift := 2
"#;
        let (_, summary) = summarize_source(source);
        assert!(
            summary.floors.is_empty(),
            "expected the outer false guard to remain closed, got {} floors",
            summary.floors.len()
        );
    }

    #[test]
    fn fixed_aliases_survive_loop_clear_epochs() {
        let source = r#"//@version=6
indicator("Fixed alias clear")
bool guard = false
bool alias = guard
for index = 0 to 0
    int local = index
var int shift = 1
if alias
    shift := 2
"#;
        let (_, summary) = summarize_source(source);
        assert!(
            summary.floors.is_empty(),
            "expected the fixed false alias to survive loop clears, got {} floors",
            summary.floors.len()
        );
    }

    #[test]
    fn persistent_writes_preserve_carried_metadata_with_and_without_else() {
        for branches in [
            "if close > open\n    shift := 2\nelse\n    shift := 3\n",
            "if close > open\n    shift := 2\n",
        ] {
            let source = format!(
                "//@version=6\nindicator(\"Persistent write branch\")\nvar int shift = 1\nplot(close, offset=shift)\n{branches}"
            );
            let (_, summary) = summarize_source(&source);
            assert_eq!(summary.known_carried_reads.len(), 1, "{branches}");
            assert_eq!(summary.floors.len(), 1, "{branches}");
            let floor = summary.floors.values().next().unwrap();
            assert_eq!(floor.declaration, Some(Qualifier::Series), "{branches}");
            assert_eq!(floor.assignment, Qualifier::Series, "{branches}");
        }
    }

    fn operation_source(binding_count: usize) -> String {
        let mut source = String::from("//@version=6\nindicator(\"Operation counts\")\n");
        for index in 0..binding_count {
            source.push_str(&format!("int alias_{index} = {index}\n"));
            source.push_str(&format!("alias_{index} := {index}\n"));
        }
        source.push_str("mystery()\nif close > open\n    for i = 0 to 0\n        mystery()\n");
        source
    }

    fn one_dirty_branch_source(binding_count: usize) -> String {
        let mut source = String::from("//@version=6\nindicator(\"Dirty branch counts\")\n");
        for index in 0..binding_count {
            source.push_str(&format!("int alias_{index} = {index}\n"));
            source.push_str(&format!("alias_{index} := {index}\n"));
        }
        source.push_str(
            "bool guard = false\nif close > open\n    guard := false\nelse\n    guard := false\n",
        );
        source
    }

    #[test]
    fn marker_and_branch_work_scales_with_scopes_and_writes_not_active_bindings() {
        let (_, small) = summarize_source(&operation_source(16));
        let (_, large) = summarize_source(&operation_source(512));

        assert_eq!(
            small.operation_counts.scope_marker_touches,
            large.operation_counts.scope_marker_touches
        );
        assert_eq!(small.operation_counts.conditional_binding_visits, 0);
        assert_eq!(large.operation_counts.conditional_binding_visits, 0);
        assert_eq!(small.operation_counts, large.operation_counts);
    }

    #[test]
    fn branch_work_tracks_one_dirty_binding_with_many_active_closed_bindings() {
        let (_, small) = summarize_source(&one_dirty_branch_source(16));
        let (_, large) = summarize_source(&one_dirty_branch_source(512));

        assert_eq!(
            small.operation_counts.conditional_binding_visits,
            large.operation_counts.conditional_binding_visits
        );
        assert_eq!(small.operation_counts.conditional_binding_visits, 6);
        assert_eq!(small.operation_counts.conditional_log_records_scanned, 4);
        assert_eq!(small.operation_counts, large.operation_counts);
    }
}
