//Copyright (C) 2025-  plusmouse and other contributors
//
//This program is free software: you can redistribute it and/or modify
//it under the terms of the GNU General Public License as published by
//the Free Software Foundation, either version 3 of the License, or
//(at your option) any later version.
//
//This program is distributed in the hope that it will be useful,
//but WITHOUT ANY WARRANTY; without even the implied warranty of
//MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//GNU General Public License for more details.
//
//You should have received a copy of the GNU General Public License
//along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Walking function bodies (or replaying their records), and what calls
//! of them require and return

use super::*;

impl<'a> Walker<'a> {
    pub(super) fn function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>) -> FunctionId {
        let id = self.function_id(def);
        self.function_ends.insert(id, def.syntax().text_range().end());
        for param in def.params().map(|p| p.parameter_tokens()).unwrap_or_default() {
            self.parameter_pseudos.insert(Location { slot: PSEUDO, ..self.location(param.text_range().start()) });
        }
        if let Some(record) = self.replayable(id, self_type) {
            self.replay(id, &record);
            return id
        }
        self.walked += 1;
        let parent = self.function_ids.last().copied();
        self.frames.push(Frame {
            bindings_start: self.bindings.len(),
            references_start: self.references.len(),
            field_references_start: self.field_references.len(),
            diagnostics_start: self.diagnostics.len(),
            pending_calls_start: self.pending_calls.len(),
            deferred_start: self.deferred.len(),
            globals_created_start: self.globals_created.len(),
            function_count_start: self.function_count,
            writes: Vec::new(),
            own_writes: Vec::new(),
            upvalues: Vec::new(),
        });
        self.reads.borrow_mut().push(Default::default());
        let bindings_start = self.bindings.len();
        // The statement defining a method decides what its `self` is
        if self_type.is_some() {
            if let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
                level.self_types.push(id);
            }
        }
        self.deps.get_mut().push(BodyDeps { bindings_start, levels: vec![DepLevel::default()], ..BodyDeps::default() });
        self.walk_function_body(def, self_type, id);
        let mut body = self.deps.get_mut().pop().expect("pushed above");
        // Its top: what's done outside any statement, like setting up its
        // parameters and writing its signature at the end
        let mut top = Dep::default();
        while let Some(mut level) = body.levels.pop() {
            top.extend(&Dep::of(&level.reads, &level.locals));
            // Left to depend on all `top` does, below
            level.writes.clear();
            body.resolve(level);
        }
        let frame = self.frames.pop().expect("pushed above");
        // What the functions inside it read, and what it did
        let mut reads = self.reads.borrow_mut().pop().expect("pushed above");
        reads.extend(body.noted.iter().map(|number| body.keys[number as usize].clone()));
        let own_function = frame.function_count_start + 1;
        let own_writes: Arc<[Arc<Write>]> = frame.own_writes.into();
        let pending: Vec<(FunctionId, Arc<Dep>)> = body.pending_deps.drain(..)
            .filter_map(|(index, dep)| Some((self.pending_calls.get(index)?.callee, dep)))
            .collect();
        let influences = BodyInfluences {
            function: id,
            own_bindings: (frame.bindings_start..self.bindings.len()).filter(|&b| self.bindings[b].function == own_function).collect(),
            own_writes: own_writes.clone(),
            pending,
            top,
            body,
            found: std::sync::OnceLock::new(),
        };
        let record = Record {
            self_type,
            with_prior: self.prior.is_some(),
            bindings_start: frame.bindings_start,
            function_count_start: frame.function_count_start,
            bindings: self.bindings[frame.bindings_start..].into(),
            current: self.current[frame.bindings_start..].into(),
            references: frame.references_start..self.references.len(),
            field_references: frame.field_references_start..self.field_references.len(),
            diagnostics: frame.diagnostics_start..self.diagnostics.len(),
            pending_calls: frame.pending_calls_start..self.pending_calls.len(),
            deferred: self.deferred[frame.deferred_start..].iter().map(|index| index - frame.references_start).collect(),
            globals: self.globals_created[frame.globals_created_start..].iter()
                .filter_map(|name| Some((name.clone(), *self.globals.get(name)?)))
                .collect(),
            function_count: self.function_count - frame.function_count_start,
            writes: frame.writes.into(),
            upvalues: frame.upvalues.into(),
            reads: Arc::new(reads),
            parent,
            own_writes,
            own_influences: Arc::new(influences),
        };
        self.mark_dirty(id, &record);
        self.pass_to_parent(&record.reads, &record.writes);
        self.upvalues_to_parent(&record.upvalues);
        self.records.insert(id, Arc::new(record));
        id
    }

    /// The record of this body from the previous walk, if none of what it
    /// read has changed since, and the walk has got to the same place
    pub(super) fn replayable(&self, id: FunctionId, self_type: Option<Type>) -> Option<Arc<Record>> {
        if !self.replay {
            return None
        }
        let record = self.prior?.records.get(&id)?;
        let same_place = record.with_prior
            && record.self_type == self_type
            && record.bindings_start == self.bindings.len()
            && record.function_count_start == self.function_count
            && record.globals.iter().all(|(name, _)| !self.globals.contains_key(name));
        let unchanged = || record.reads.iter().all(|read| !self.changed.contains(read) && !self.dirty.contains(read))
            && record.upvalues.iter().all(|&(binding, types)| self.current.get(binding) == Some(&types));
        (same_place && unchanged()).then(|| record.clone())
    }

    /// Does what walking the body did, as recorded on the previous walk
    pub(super) fn replay(&mut self, id: FunctionId, record: &Record) {
        let prior = self.prior.expect("only records from a previous walk are replayed");
        self.replayed += 1;
        let references_start = self.references.len();
        let field_references_start = self.field_references.len();
        let diagnostics_start = self.diagnostics.len();
        let pending_calls_start = self.pending_calls.len();
        self.bindings.extend(record.bindings.iter().cloned());
        self.current.extend(record.current.iter().copied());
        self.references.extend(prior.references[record.references.clone()].iter().cloned());
        self.field_references.extend(prior.field_references[record.field_references.clone()].iter().cloned());
        self.diagnostics.extend(prior.diagnostics[record.diagnostics.clone()].iter().cloned());
        self.pending_calls.extend(prior.pending_calls[record.pending_calls.clone()].iter().cloned());
        self.deferred.extend(record.deferred.iter().map(|index| index + references_start));
        for (name, binding) in record.globals.iter() {
            self.globals.insert(name.clone(), *binding);
            self.globals_created.push(name.clone());
        }
        self.function_count += record.function_count;
        // Recorded for the parent all at once below, rather than one by one
        for write in record.writes.iter() {
            self.effect(write);
        }
        self.pass_to_parent(&record.reads, &record.writes);
        // The same again for the next walk, at this walk's places
        let replayed = Record {
            self_type: record.self_type,
            with_prior: true,
            bindings_start: record.bindings_start,
            function_count_start: record.function_count_start,
            bindings: Arc::clone(&record.bindings),
            current: Arc::clone(&record.current),
            references: references_start..self.references.len(),
            field_references: field_references_start..self.field_references.len(),
            diagnostics: diagnostics_start..self.diagnostics.len(),
            pending_calls: pending_calls_start..self.pending_calls.len(),
            deferred: Arc::clone(&record.deferred),
            globals: Arc::clone(&record.globals),
            function_count: record.function_count,
            writes: Arc::clone(&record.writes),
            reads: Arc::clone(&record.reads),
            upvalues: Arc::clone(&record.upvalues),
            parent: record.parent,
            own_writes: Arc::clone(&record.own_writes),
            own_influences: Arc::clone(&record.own_influences),
        };
        self.upvalues_to_parent(&record.upvalues);
        self.records.insert(id, Arc::new(replayed));
    }

    /// Bodies later in this walk read some of what a body writes as it is on
    /// this walk, not the previous one: a function's returns, an object's
    /// parent, a pool's object. A body walked again that writes those
    /// differently from last time makes the bodies reading them walk again
    pub(super) fn mark_dirty(&mut self, id: FunctionId, record: &Record) {
        #[derive(PartialEq)]
        enum Shared<'a> {
            Write(&'a Write),
            Signature(Signature<'a>),
        }
        fn shared(writes: &[Arc<Write>]) -> HashMap<Read, Vec<Shared<'_>>> {
            let mut shared: HashMap<Read, Vec<Shared>> = HashMap::default();
            for write in writes {
                match &**write {
                    Write::Function(id, info) => {
                        shared.entry(Read::Function(*id)).or_default().push(Shared::Signature(info.signature()));
                    }
                    Write::Parent(at, _) => shared.entry(Read::Parent(*at)).or_default().push(Shared::Write(write)),
                    Write::Triggered(at, event, _) => shared.entry(Read::Triggered(*at, *event)).or_default().push(Shared::Write(write)),
                    Write::PoolObject(at, _) => shared.entry(Read::PoolObject(*at)).or_default().push(Shared::Write(write)),
                    _ => {}
                }
            }
            shared
        }
        let now = shared(&record.writes);
        let before = self.prior.and_then(|prior| prior.records.get(&id)).map(|old| shared(&old.writes));
        for (read, values) in now {
            if before.as_ref().and_then(|before| before.get(&read)) != Some(&values) {
                self.dirty.insert(read);
            }
        }
    }

    pub(super) fn walk_function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>, id: FunctionId) {
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true, def.syntax().text_range().end());
        let outer_function_scope = std::mem::replace(&mut self.function_scope, self.scopes.len() - 1);
        let outer_short_circuit = std::mem::take(&mut self.short_circuit_depth);
        // Defining a function doesn't run it: what's narrowed around it holds
        // on after it, and nothing narrowed there holds inside it
        let outer_fields = (std::mem::take(&mut self.narrowed_fields), self.field_epoch);
        self.returns.push(None);
        self.calls.push(Vec::new());
        self.loop_edges.push(Vec::new());
        self.returned_parameters.push(Vec::new());
        self.function_ranges.push((def.syntax().text_range(), self.function));
        self.closure_loop_depths.push(self.loop_depth);
        self.function_ids.push(id);
        self.function_numbers.insert(self.function, id);
        self.fresh.push(None);
        self.kept_outside.push(HashSet::default());
        self.forwarded.push(Vec::new());
        self.field_writes.push(Vec::new());
        self.returned_fields.push(None);
        self.iterating.push(None);
        self.self_bindings.push(None);
        if let Some(self_type) = self_type {
            let depth = self.scopes.len() - 1;
            // Implicit, so it's declared by the method's name: `b` in `function a:b()`
            let declaration = def.identifier().and_then(|identifier| identifier.syntax().children_with_tokens()
                .filter_map(|t| t.into_token())
                .filter(|t| t.kind() == SyntaxKind::Name)
                .last())
                .map(|name| name.text_range());
            // As the statement defining the method decides
            let function = self.function_ids.last().copied();
            self.begin_declaration();
            let id = self.new_binding("self", BindingKind::Parameter, declaration, self_type, self.function, depth);
            self.end_declaration(function.map(Read::SelfType));
            self.bindings[id].assigned = true;
            self.bindings[id].visible = Some(def.syntax().text_range());
            self.scopes[depth].names.insert("self".to_string(), id);
            if let Some(binding) = self.self_bindings.last_mut() {
                *binding = Some(id);
            }
        }
        let params = def.params();
        let parameters = params.as_ref().map(|p| p.parameter_tokens()).unwrap_or_default();
        let parameter_bindings: Vec<BindingId> = parameters.iter().enumerate()
            .map(|(i, param)| {
                // What calls pass it, and nothing else read at the top
                self.begin_declaration();
                let types = self.prior_parameter_types(id, i);
                let binding = self.declare(param, BindingKind::Parameter, types);
                self.end_declaration(None);
                self.parameter_index.insert(binding, i);
                self.parameter_entry.insert(binding, self.current[binding]);
                binding
            })
            .collect();
        let block = def.block();
        if let Some(block) = &block {
            self.statements(block);
        }
        let returned = self.returns.pop().flatten();
        let calls = self.calls.pop().unwrap_or_default();
        let loop_edges = self.loop_edges.pop().unwrap_or_default();
        let mut returned_parameters = self.returned_parameters.pop().unwrap_or_default();
        returned_parameters.sort();
        returned_parameters.dedup();
        self.function_ranges.pop();
        self.closure_loop_depths.pop();
        self.function_ids.pop();
        let fresh_returns = self.fresh.pop().flatten().unwrap_or_default();
        self.kept_outside.pop();
        let forwarded_calls = self.forwarded.pop().unwrap_or_default();
        self.self_bindings.pop();
        let returned_fields = self.returned_fields.pop().flatten().unwrap_or_default();
        // Only when every return gives a loop what to iterate, and it can't
        // reach `end` without returning
        let iterated = self.iterating.pop().flatten().flatten()
            .filter(|_| def.block().as_ref().is_some_and(always_returns));
        // A table it makes is given by the return that gives it new
        let mut field_writes: Vec<FieldWrite> = Vec::new();
        for (on, field, value) in self.field_writes.pop().unwrap_or_default() {
            let on = match on {
                Ok(on) => on,
                Err(made) => match fresh_returns.iter().position(|fresh| fresh.as_ref().is_some_and(|f| f.made == Some(made))) {
                    Some(slot) => On::Returned(slot),
                    None => continue,
                },
            };
            let write = FieldWrite { on, field, value };
            if !field_writes.contains(&write) {
                field_writes.push(write);
            }
        }
        let returns = match returned {
            Some(returns) if block.as_ref().is_some_and(always_returns) => returns,
            // Reaching `end` returns nothing
            Some(returns) => returns.union(&Values::none()),
            None => Values::none(),
        };
        let returns = self.with_returned_templates(def, returns);
        // Kept as its signature: more tables than are kept apart are a stand-in
        let returns = Values {
            values: returns.values.iter().enumerate().map(|(i, &value)| self.stored(Place::Return(id, i), value, true)).collect(),
            rest: self.stored(Place::Return(id, usize::MAX), returns.rest, true),
        };
        self.pop_scope();
        self.function = outer_function;
        self.function_scope = outer_function_scope;
        self.short_circuit_depth = outer_short_circuit;
        (self.narrowed_fields, self.field_epoch) = outer_fields;

        let parameters = parameters.iter().zip(&parameter_bindings).map(|(token, binding)| Parameter {
            name: token.text().to_string(),
            // Replaced by `settle_parameter_types` once all calls are seen
            types: self.current[*binding],
            mandated: self.mandates.get(binding).copied().flatten(),
        }).collect();
        let varargs = params.is_some_and(|p| p.ellipsis())
            .then(|| Parameter { name: "...".to_string(), types: Type::UNKNOWN, mandated: None });
        self.apply(Write::Function(id, FunctionInfo { parameters, method: self_type.is_some(), varargs, returns, fresh_returns, calls, forwarded_calls, field_writes, returned_fields, iterated, only: None, loop_edges, returned_parameters }));
    }

    /// `returns`, with the templates the definitions say objects the function
    /// returns are made with (see `Definitions::returned_templates`): each of
    /// that kind is of the template's type, and may have members beyond it,
    /// as others may be made with more templates
    pub(super) fn with_returned_templates(&mut self, def: &FunctionDefinition, returns: Values) -> Values {
        let Some(path) = function_path(def) else { return returns };
        let declared = self.definitions.returned_templates(&path).to_vec();
        if declared.is_empty() {
            return returns
        }
        let templates: Vec<(NamedId, NamedId)> = declared.iter()
            .filter_map(|(kind, template)| {
                let kind = self.definitions.named_type_id(kind)?;
                let (template, element) = self.definitions.template(template)?;
                (element == kind).then_some((kind, template))
            })
            .collect();
        let made: std::cell::RefCell<Vec<(TableId, NamedId)>> = Default::default();
        let with = |ty: Type| -> Type {
            let named = ty.named.map_objects(|object| {
                let Some((id, _)) = object.object_type() else { return object };
                let Some(&(_, template)) = templates.iter().find(|&&(kind, _)| kind == id) else { return object };
                match object {
                    NamedRef::Instance { at, .. } => {
                        made.borrow_mut().push((at, template));
                        NamedRef::Instance { named: template, at, extended: true }
                    }
                    _ => NamedRef::Extended(template),
                }
            });
            Type { named, ..ty }
        };
        let returns = Values { values: returns.values.iter().map(|&v| with(v)).collect(), rest: with(returns.rest) };
        // What it's made in has the template's mixins as it would made with it
        for (at, template) in made.into_inner() {
            let parent = self.known_parent(at).unwrap_or(Type::UNKNOWN);
            self.add_lua_mixins(at, template, parent);
        }
        returns
    }

    pub(super) fn prior_parameter_types(&self, id: FunctionId, index: usize) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::ParameterTypes(id));
        prior.functions.get(&id)
            .and_then(|function| function.parameters.get(index))
            .map_or(Type::UNKNOWN, |parameter| parameter.types)
    }

    /// The function being called, as far as this walk knows it
    pub(super) fn known_function(&self, function: FunctionRef) -> Option<&FunctionInfo> {
        match function {
            FunctionRef::One(id) => {
                self.note(Read::Function(id));
                self.functions.get(&id)
                    .or_else(|| self.layer.functions.get(&id))
                    .or_else(|| self.prior?.functions.get(&id))
            }
            FunctionRef::Builtin(id) => self.definitions.function(id),
            _ => None,
        }
    }

    /// Whether the current point runs every time the current function does
    pub(super) fn runs_unconditionally(&self) -> bool {
        self.function != MAIN_CHUNK
            && self.short_circuit_depth == 0
            && self.scopes[self.function_scope + 1..].iter().all(|scope| !scope.conditional)
    }

    pub(super) fn local_binding(&self, name: &str) -> Option<BindingId> {
        self.scopes.iter().rev().find_map(|scope| scope.names.get(name).copied())
    }

    /// Of two requirements, the one that satisfies both: the same type, or a
    /// named type that inherits from the other (`Frame` over `ScriptRegion`).
    /// None if they conflict
    pub(super) fn narrower(&self, a: Type, b: Type) -> Option<Type> {
        if a == b {
            return Some(a)
        }
        let id = |t: Type| t.named.named_id();
        let (x, y) = (id(a)?, id(b)?);
        if a.set != b.set {
            return None
        }
        if self.definitions.is_a(x, y) {
            Some(a)
        } else if self.definitions.is_a(y, x) {
            Some(b)
        } else {
            None
        }
    }

    /// Parameters of the current function passed straight on as arguments that
    /// require one type have to be that type, if the call always runs and the
    /// parameter still holds what the caller passed
    pub(super) fn require_arguments(&mut self, arguments: &[Expression], required: &[Option<Type>]) {
        if !self.runs_unconditionally() {
            return
        }
        for (argument, required) in arguments.iter().zip(required) {
            let (Some(required), Expression::Identifier(identifier)) = (required, argument) else { continue };
            let Some(id) = identifier.simple_name().and_then(|name| self.local_binding(name.text())) else { continue };
            let binding = &self.bindings[id];
            if binding.kind != BindingKind::Parameter || binding.function != self.function || binding.reassigned {
                continue
            }
            // Required to be a `ScriptRegion` means at least one: it could be a
            // Frame or a FontString, with more methods
            let required = match required.named {
                NamedRef::One(named) if self.definitions.has_subtypes(named) => Type { named: NamedRef::Extended(named), ..*required },
                _ => *required,
            };
            let merged = match self.mandates.get(&id) {
                None => Some(required),
                Some(earlier) => earlier.and_then(|earlier| self.narrower(earlier, required)),
            };
            // Only a requirement of its own parameter says what it takes
            self.deps_to_requirements();
            self.mandates.insert(id, merged);
        }
    }

    pub(super) fn returns_of(&mut self, id: FunctionId) -> Values {
        let returns = self.known_returns(id);
        let Some(&caller) = self.function_ids.last() else { return returns };
        // What it calls is found from how the callee was reached, when a
        // call says so, else from all the statement read
        let found = self.callee_found.take();
        let added = match (found, self.deps.get_mut().last_mut()) {
            (Some((level, reads, locals)), Some(body)) => match body.levels.get(level) {
                Some(level) => {
                    for &number in &level.reads[reads] {
                        body.calls.reads.insert(number);
                    }
                    for &number in &level.locals[locals] {
                        body.calls.locals.insert(number);
                    }
                    true
                }
                None => false,
            },
            _ => false,
        };
        if !added {
            self.deps_to_calls();
        }
        if let Some(calls) = self.calls.last_mut().filter(|calls| !calls.contains(&id)) {
            calls.push(id);
        }
        if !self.recursive(caller, id) {
            return returns
        }
        // A recursive call: its results include its own, so as many values
        // as it returns would be one more each walk with `return x, f()`.
        // The first is kept apart, the others are joined in `rest`
        let mut values = returns.values.into_iter();
        let first = values.next();
        let rest = values.fold(returns.rest, |rest, value| rest | value);
        Values { values: first.into_iter().collect(), rest }
    }

    /// Whether `caller` calling `callee` is recursive: the same function, or
    /// two in one cycle of calls, as the previous walk found the cycles
    pub(super) fn recursive(&self, caller: FunctionId, callee: FunctionId) -> bool {
        if caller == callee {
            return true
        }
        let Some(prior) = self.prior else { return false };
        self.note(Read::CallCycle(caller));
        self.note(Read::CallCycle(callee));
        match (prior.call_cycles.get(&caller), prior.call_cycles.get(&callee)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    pub(super) fn known_returns(&self, id: FunctionId) -> Values {
        self.note(Read::Function(id));
        // Not walked yet on this walk: defined later in the file, or a recursive call
        if let Some(function) = self.functions.get(&id).or_else(|| self.layer.functions.get(&id)) {
            return function.returns.clone()
        }
        match self.prior {
            None => Values::nothing(),
            Some(prior) => prior.functions.get(&id).map_or_else(Values::unknown, |f| f.returns.clone()),
        }
    }
}

/// How the definitions name a function defined with a name, like
/// `Mixin.Method` for `function Mixin:Method()`, or `f` for `local function f()`
pub(super) fn function_path(def: &FunctionDefinition) -> Option<String> {
    if let Some(name) = def.local_name() {
        return Some(name.text().to_string())
    }
    let identifier = def.identifier()?;
    let path: String = identifier.syntax().text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
    Some(path.replace(':', "."))
}
