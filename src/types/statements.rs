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

//! Walking blocks and statements

use super::*;

impl<'a> Walker<'a> {
    pub(super) fn block(&mut self, block: Option<Block>, conditional: bool) {
        if let Some(block) = block {
            self.push_scope(conditional, block.syntax().text_range().end());
            self.statements(&block);
            self.pop_scope();
        }
    }

    pub(super) fn statements(&mut self, block: &Block) {
        for child in block.syntax().children() {
            match Statement::cast(child.clone()) {
                Some(statement) => self.statement(&statement),
                // Half-typed code like `frame.` that isn't a statement yet: its
                // names are still read, for hover and completion
                None if child.kind() == SyntaxKind::Identifier => {
                    if let Some(expression) = Expression::cast(child) {
                        self.expression(&expression);
                    }
                }
                None => {}
            }
        }
    }

    pub(super) fn statement(&mut self, statement: &Statement) {
        let tracked = !self.deps.get_mut().is_empty();
        if tracked {
            if let Some(body) = self.deps.get_mut().last_mut() {
                body.levels.push(DepLevel::default());
            }
        }
        self.statement_walked(statement);
        if tracked {
            if let Some(body) = self.deps.get_mut().last_mut() {
                if let Some(mut level) = body.levels.pop() {
                    level.signature |= matches!(statement, Statement::Return(_));
                    body.resolve(level);
                }
            }
        }
    }

    pub(super) fn statement_walked(&mut self, statement: &Statement) {
        match statement {
            Statement::LocalAssign(s) => {
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, names.len());
                // `local x = x` reads the outer `x`: the new one is seen after the
                // statement's code, not counting the whitespace it ends with
                // Back from its last token, as a value can be a whole function
                let start = s.syntax().text_range().start();
                let after = std::iter::successors(s.syntax().last_token(), |t| t.prev_token())
                    .take_while(|t| t.text_range().start() >= start)
                    .find(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment))
                    .map_or(s.syntax().text_range().end(), |t| t.text_range().end());
                // `local x =` with its value still to be typed: what follows is the value
                let unfinished = expressions.is_empty()
                    && s.syntax().children_with_tokens().any(|e| e.kind() == SyntaxKind::Assign);
                for (i, (name, types)) in names.iter().zip(types).enumerate() {
                    let binding = self.declare(name, BindingKind::Local, types);
                    if let Some(visible) = &mut self.bindings[binding].visible {
                        let start = if unfinished { visible.end() } else { after.min(visible.end()) };
                        *visible = TextRange::new(start, visible.end());
                    }
                    if let Some(Expression::Function(def)) = expressions.get(i) {
                        self.apply(Write::AssignedTo(self.function_id(def), binding));
                    }
                }
            }
            Statement::Assign(s) => {
                let targets = s.variable_list().map(|v| v.identifiers()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, targets.len());
                for (i, (target, types)) in targets.iter().zip(types).enumerate() {
                    match (target.simple_name(), self.global_table_access(target.syntax())) {
                        (Some(name), _) => {
                            self.write(&name, types);
                            if let Some(Expression::Function(def)) = expressions.get(i) {
                                let binding = self.resolve(name.text());
                                self.apply(Write::AssignedTo(self.function_id(def), binding));
                            }
                        }
                        // `_G.name = v`: the global
                        (None, Some((token, name))) => {
                            self.identifier_parts(target.syntax());
                            let id = self.resolve_global(&name);
                            self.write_binding(&token, id, types);
                        }
                        (None, None) => match field_access(target.syntax()) {
                            Some((base_expression, field)) => {
                                let base = self.expression(&base_expression);
                                self.forget_fields();
                                // Only a value of its own, not one of several a call returns
                                let value = (expressions.len() == targets.len()).then(|| expressions.get(i)).flatten();
                                self.note_field_write(&base_expression, base, field.text(), value);
                                self.note_loop_write(base, field.text(), value);
                                self.field_write(base, field.text(), types);
                                self.field_reference(target.syntax(), &field, base, types, true);
                            }
                            None => match index_access(target.syntax()) {
                                // `t[k] = v`
                                Some((base, key)) => {
                                    let base = self.expression(&base);
                                    let key = { let key = self.index_key(&key); self.forget_fields(); key };
                                    match key {
                                        Key::Name(name, token) => {
                                            self.field_write(base, &name, types);
                                            if let Some(token) = token {
                                                self.field_reference(target.syntax(), &token, base, types, true);
                                            }
                                        }
                                        Key::Other(key) => self.entry_write(base, key, types),
                                    }
                                }
                                None => self.identifier_parts(target.syntax()),
                            },
                        },
                    }
                }
            }
            Statement::FunctionCall(call) => {
                self.function_call(call);
                // Past `assert(v)`, `v` is true: otherwise it raised an error
                if self.calls_library_global(call, "assert") {
                    if let Some(condition) = call.arguments().first() {
                        for (target, ty) in self.narrowing(condition).when_true {
                            self.narrow(&target, ty);
                        }
                    }
                }
            }
            Statement::Do(s) => self.block(s.block(), false),
            Statement::While(s) => self.loop_body(|w| {
                let proved = s.condition().map(|condition| {
                    w.expression(&condition);
                    w.narrowing(&condition)
                }).unwrap_or_default();
                w.with_narrowing(&proved.when_true, |w| w.block(s.block(), true));
            }),
            Statement::Repeat(s) => self.loop_body(|w| {
                // The `until` condition can see the block's locals
                w.push_scope(true, s.syntax().text_range().end());
                if let Some(block) = s.block() {
                    w.statements(&block);
                }
                if let Some(condition) = s.condition() {
                    w.expression(&condition);
                }
                w.pop_scope();
            }),
            Statement::If(s) => {
                // Each branch runs only if the conditions before it were false
                let mut earlier_false: Vec<(Target, Type)> = Vec::new();
                // Without an `else`, code after an `if` whose branches all
                // return runs only when every condition was false
                let mut all_return = s.else_branch().is_none();
                self.joins.push(Join { bindings: self.bindings.len(), ..Join::default() });
                // What each variable holds at the end of the branches that go
                // on past the `if`
                let mut ends: HashMap<BindingId, Type> = HashMap::default();
                for (i, branch) in s.if_branches().iter().enumerate() {
                    let block = branch.block();
                    let returns = block.as_ref().is_some_and(always_returns);
                    all_return &= returns;
                    // An `elseif` condition that may call a function may change
                    // the fields the conditions before it proved things of
                    if branch.expression().is_some_and(|condition| may_call(self, condition.syntax())) {
                        earlier_false.retain(|(target, _)| matches!(target, Target::Local(_)));
                    }
                    self.narrow_locals(&earlier_false);
                    let proved = self.with_narrowing(&fields_only(&earlier_false), |w| {
                        let condition = branch.expression()?;
                        // An `elseif` condition only runs if the ones before failed
                        w.short_circuit_depth += usize::from(i > 0);
                        w.expression(&condition);
                        w.short_circuit_depth -= usize::from(i > 0);
                        w.conditions_walked();
                        let proved = w.narrowing(&condition);
                        let taken = both(earlier_false.clone(), proved.when_true.clone());
                        w.narrow_locals(&taken);
                        w.with_narrowing(&fields_only(&taken), |w| w.branch_block(block));
                        Some(proved)
                    });
                    match proved {
                        Some(proved) => earlier_false = both(earlier_false, proved.when_false),
                        None => self.branch_block(branch.block()),
                    }
                    self.end_branch(&mut ends, !returns);
                }
                if let Some(branch) = s.else_branch() {
                    let returns = branch.block().as_ref().is_some_and(always_returns);
                    self.narrow_locals(&earlier_false);
                    self.with_narrowing(&fields_only(&earlier_false), |w| w.branch_block(branch.block()));
                    self.end_branch(&mut ends, !returns);
                }
                let join = self.joins.pop().unwrap_or_default();
                // Code after it may also run with every condition false
                let skipped = s.else_branch().is_none();
                let mut ids: Vec<BindingId> = join.before.keys().copied().collect();
                ids.extend(earlier_false.iter().filter_map(|(target, _)| match target {
                    Target::Local(id) if *id < join.bindings && !join.before.contains_key(id) => Some(*id),
                    _ => None,
                }));
                ids.sort();
                ids.dedup();
                // Changes the `if` made are changes the one around it made
                if let Some(outer) = self.joins.last_mut() {
                    for (&id, &(before, _)) in &join.before {
                        if id < outer.bindings {
                            outer.before.entry(id).or_insert((before, outer.fell_through));
                        }
                    }
                }
                for id in ids {
                    let (before, fell_through) = join.before.get(&id).copied().unwrap_or((self.current[id], 0));
                    // With every condition walked, as the code after it is
                    // when none was true
                    let conditioned = join.after_conditions.get(&id).copied().unwrap_or(before);
                    let mut joined = ends.get(&id).copied().unwrap_or_default();
                    // The branches that went on before it was changed left it as it was
                    if fell_through > 0 {
                        joined |= before;
                    }
                    if skipped {
                        let narrowed = earlier_false.iter().find(|(target, _)| *target == Target::Local(id)).map(|(_, ty)| *ty);
                        joined |= narrowed.unwrap_or(conditioned);
                    }
                    // Nothing goes on past it: nothing after it runs
                    if join.fell_through == 0 && !skipped {
                        joined = conditioned;
                    }
                    self.set_current(id, joined);
                }
                if all_return {
                    for (target, ty) in earlier_false {
                        if let Target::Field(..) = target {
                            self.narrow(&target, ty);
                        }
                    }
                }
            }
            Statement::ForCountLoop(s) => {
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                for e in &expressions {
                    self.expression(e);
                }
                self.loop_body(|w| {
                    w.push_scope(true, s.syntax().text_range().end());
                    if let Some(name) = s.name_token() {
                        w.declare(&name, BindingKind::Local, Type::of(TypeSet::NUMBER));
                    }
                    if let Some(block) = s.block() {
                        w.statements(&block);
                    }
                    w.pop_scope();
                });
            }
            Statement::ForInLoop(s) => {
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let iterator = self.iterator_name(&expressions);
                let iterator_function = expressions.iter().map(|e| self.expression(e)).next();
                // `for object in pool:EnumerateActive()`
                let enumerated = match expressions.as_slice() {
                    [Expression::FunctionCall(call)] => self.enumerated.get(&self.call_location(call)).copied(),
                    _ => None,
                };
                // `for k, v in pairs(t)` over a table whose contents are known, or
                // `for w in s:gmatch(p)`
                let iterated = match expressions.as_slice() {
                    [Expression::FunctionCall(call)] => self.iterated.get(&self.call_location(call)).cloned(),
                    _ => None,
                };
                // Otherwise each pass calls the iterator, so the variables are
                // what it returns, if it's a function we know
                let iterated = iterated.or_else(|| {
                    let function = iterator_function?.function;
                    let mut joined: Option<Values> = None;
                    for function in function.alternatives() {
                        let returns = match function {
                            FunctionRef::One(id) => self.returns_of(id),
                            _ => self.known_function(function)?.returns.clone(),
                        };
                        joined = Some(joined.map_or_else(|| returns.clone(), |j| j.union(&returns)));
                    }
                    joined
                });
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                self.loop_body(|w| {
                    w.push_scope(true, s.syntax().text_range().end());
                    for (i, name) in names.iter().enumerate() {
                        if let (0, Some(object)) = (i, enumerated) {
                            w.declare(name, BindingKind::Local, object);
                            continue
                        }
                        if let Some(values) = &iterated {
                            // The loop stops when the first variable is nil
                            let types = if i == 0 { values.get(0).without(TypeSet::NIL) } else { values.get(i) };
                            w.declare(name, BindingKind::Local, types);
                            continue
                        }
                        let types = match (i, iterator.as_deref()) {
                            (0, Some("ipairs")) => TypeSet::NUMBER,
                            (1, Some("ipairs" | "pairs")) => TypeSet::UNKNOWN.without(TypeSet::NIL),
                            // The loop stops when the first variable is nil
                            (0, _) => TypeSet::UNKNOWN.without(TypeSet::NIL),
                            _ => TypeSet::UNKNOWN,
                        };
                        w.declare(name, BindingKind::Local, Type::of(types));
                    }
                    if let Some(block) = s.block() {
                        w.statements(&block);
                    }
                    w.pop_scope();
                });
            }
            Statement::FunctionDefinition(def) => {
                if def.is_local() {
                    // Declared before the body so the function can call itself
                    if let Some(name) = def.local_name() {
                        let binding = self.declare(&name, BindingKind::Local, Type::function(self.function_id(def)));
                        self.apply(Write::AssignedTo(self.function_id(def), binding));
                    }
                    self.function_body(def, None);
                } else if let Some(identifier) = def.identifier() {
                    match identifier.simple_name() {
                        Some(name) => {
                            self.write(&name, Type::function(self.function_id(def)));
                            let binding = self.resolve(name.text());
                            self.apply(Write::AssignedTo(self.function_id(def), binding));
                            self.function_body(def, None);
                        }
                        // `function a.b:c()` assigns field `c` of `a.b`
                        None => {
                            let owner = self.function_name_owner(&identifier);
                            if let Some((owner, field)) = &owner {
                                let function = Type::function(self.function_id(def));
                                self.forget_fields();
                                self.field_write(*owner, field.text(), function);
                                self.field_reference(identifier.syntax(), field, *owner, function, true);
                            }
                            // `self` is the table the method is defined on
                            let self_type = identifier.is_call_to_self().then(|| {
                                let owner = owner.map_or(Type::UNKNOWN, |(owner, _)| owner);
                                match owner.named {
                                    // Unknown, so assume a table, with fields of its own
                                    NamedRef::None | NamedRef::Unknown => Type::table(self.function_id(def)),
                                    // A mixin: `self` is the WoW object it's mixed into, with
                                    // the mixin's fields too
                                    NamedRef::Table(at) => match { self.note(Read::MixedInto(at)); self.prior.and_then(|prior| prior.mixed_into.get(&at)).or_else(|| self.layer.mixed_into.get(&at)) } {
                                        Some(&(named, extended)) => Type { named: NamedRef::Instance { named, at, extended }, ..Type::of(TypeSet::TABLE) },
                                        None => Type { named: owner.named, ..Type::of(TypeSet::TABLE) },
                                    },
                                    named => {
                                        let set = owner.set.intersection(OBJECTS);
                                        let set = if set == TypeSet::NEVER { TypeSet::TABLE } else { set };
                                        Type { named, ..Type::of(set) }
                                    }
                                }
                            });
                            // Only given as a script handler, never seen called
                            // on the table: the frames the game calls it with.
                            // With those it's mixed into too, if it's a mixin,
                            // as their template's scripts may call it
                            let self_type = self_type.map(|owner_self| {
                                let id = self.function_id(def);
                                self.note(Read::MethodSelf(id));
                                match self.prior.and_then(|prior| prior.method_selves.get(&id).copied()) {
                                    Some(frames) if matches!(owner_self.named, NamedRef::Table(_) | NamedRef::Tables(_) | NamedRef::Unknown) => frames,
                                    Some(frames) => owner_self | frames,
                                    None => owner_self,
                                }
                            });
                            self.function_body(def, self_type);
                        }
                    }
                }
            }
            Statement::Return(s) => {
                for expression in s.expressions().iter().flat_map(loop_operands) {
                    if let Some(index) = self.own_parameter_read(&expression) {
                        if let Some(returned) = self.returned_parameters.last_mut() {
                            returned.push(index);
                        }
                    }
                }
                let returned_fields: Vec<Option<ReturnedField>> = s.expressions().iter().map(|e| self.returned_field(e)).collect();
                if let Some(fields) = self.returned_fields.last_mut() {
                    *fields = Some(match fields.take() {
                        None => returned_fields,
                        // Every return has to agree
                        Some(earlier) => (0..earlier.len().max(returned_fields.len()))
                            .map(|i| match (earlier.get(i), returned_fields.get(i)) {
                                (Some(Some(a)), Some(Some(b))) if a == b => Some(a.clone()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
                let values = self.expression_values(&s.expressions());
                if !self.fresh.is_empty() {
                    let made: Vec<Option<FreshObject>> = values.values.iter()
                        .map(|value| {
                            let made = value.named.identity()
                                .filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at)))?;
                            let mixed_parameters = self.parameter_mixins.get(&made).cloned().unwrap_or_default();
                            let pool_mixed_parameters = { self.note(Read::PoolObject(made)); self.pool_objects.get(&made) }.and_then(|object| object.named.identity())
                                .map(|object| self.parameter_mixins.get(&object).cloned().unwrap_or_default());
                            Some(FreshObject { made: Some(made), mixed_parameters, pool_mixed_parameters, mixin_globals: Vec::new(), kind_parameter: self.kind_parameters.get(&made).copied(), fields: Vec::new() })
                        })
                        .collect();
                    if let Some(fresh) = self.fresh.last_mut() {
                        *fresh = Some(match fresh.take() {
                            None => made,
                            // Every return has to agree on what's new
                            Some(earlier) => (0..earlier.len().max(made.len()))
                                .map(|i| match (earlier.get(i), made.get(i)) {
                                    (Some(Some(a)), Some(Some(b))) if a == b => Some(a.clone()),
                                    _ => None,
                                })
                                .collect(),
                        });
                    }
                }
                // `return ipairs(t)`: what a loop over it gets, for a loop
                // over a call to this function
                let iterated = match s.expressions().as_slice() {
                    [Expression::FunctionCall(call)] => self.iterated.get(&self.call_location(call)).cloned(),
                    _ => None,
                };
                if let Some(iterating) = self.iterating.last_mut() {
                    *iterating = Some(match (iterating.take(), iterated) {
                        (None, iterated) => iterated,
                        (Some(Some(earlier)), Some(iterated)) => Some(earlier.union(&iterated)),
                        _ => None,
                    });
                }
                // At the top of the file there's no function to return from
                if let Some(returns) = self.returns.last_mut() {
                    *returns = Some(match returns.take() {
                        Some(earlier) => earlier.union(&values),
                        None => values,
                    });
                }
            }
        }
    }

    /// `pairs`/`ipairs` when they are the unshadowed globals
    pub(super) fn iterator_name(&self, expressions: &[Expression]) -> Option<String> {
        let Some(Expression::FunctionCall(call)) = expressions.first() else { return None };
        let Some(Expression::Identifier(callee)) = call.callee() else { return None };
        let name = callee.simple_name()?;
        let name = name.text();
        (matches!(name, "pairs" | "ipairs") && self.is_global(name)).then(|| name.to_string())
    }

    /// `_G.name` or `_G["name"]`, while `_G` is the globals table: the token
    /// naming the global (the string literal for `_G["name"]`) and its name
    pub(super) fn global_table_access(&self, identifier: &SyntaxNode) -> Option<(SyntaxToken, String)> {
        let is_globals = |base: &Expression| matches!(base, Expression::Identifier(i)
            if i.simple_name().is_some_and(|n| n.text() == "_G") && self.is_global("_G"));
        if let Some((base, field)) = field_access(identifier) {
            return is_globals(&base).then(|| (field.clone(), field.text().to_string()))
        }
        let (base, Expression::Literal(key)) = index_access(identifier)? else { return None };
        if !is_globals(&base) {
            return None
        }
        let name = string_field_name(&key.get_string()?)?.to_string();
        let token = key.syntax().descendants_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == SyntaxKind::String)?;
        Some((token, name))
    }

    /// The table and field a `function a.b:c()` statement assigns: `a.b` and `c`
    pub(super) fn function_name_owner(&mut self, identifier: &Identifier) -> Option<(Type, SyntaxToken)> {
        let names: Vec<SyntaxToken> = identifier.syntax().children_with_tokens()
            .filter_map(|t| t.into_token())
            .filter(|t| t.kind() == SyntaxKind::Name)
            .collect();
        let (first, rest) = names.split_first()?;
        let (last, middle) = rest.split_last()?;
        let mut owner = self.read(first);
        for name in middle {
            let base = owner;
            owner = self.field_read(base, name.text());
            self.field_reference(identifier.syntax(), name, base, owner, false);
        }
        Some((owner, last.clone()))
    }

    /// `access` is the code reaching the field (`a.b` for `b`), or for a table
    /// constructor's key, the field
    /// entry. `base` is what the field is on
    pub(super) fn field_reference(&mut self, access: &SyntaxNode, name: &SyntaxToken, base: Type, types: Type, write: bool) {
        let table = base.named.identity();
        self.field_references.push(FieldReference { file: self.file, range: name.text_range(), access: crate::syntax::SyntaxNodePtr::new(access), token: name.green().to_owned(), types, table, base: base.named, write, callee: false, tested: !write && only_tested(access), call_returns: None, forwarded: None });
    }

    /// The field read just recorded is the function being called
    pub(super) fn last_field_is_callee(&mut self) {
        if let Some(field) = self.field_references.last_mut() {
            field.callee = true;
        }
    }
}

/// Whether every path through the block ends in a `return`. Conservative:
/// loops and `error()` calls count as possibly falling through
/// An `if` statement being walked: what each older variable it changed held
/// before, and how many of its branches had gone on past it by then
#[derive(Default)]
pub(super) struct Join {
    /// The bindings made before it, the only ones that matter after it
    pub(super) bindings: usize,
    pub(super) before: HashMap<BindingId, (Type, usize)>,
    /// What each holds after the conditions walked so far, which run
    /// whichever branch is taken, so each later branch starts from it
    pub(super) after_conditions: HashMap<BindingId, Type>,
    /// Branches walked so far that go on past it
    pub(super) fell_through: usize,
}

/// The fields among narrowed targets
pub(super) fn fields_only(narrowed: &[(Target, Type)]) -> Vec<(Target, Type)> {
    narrowed.iter().filter(|(target, _)| matches!(target, Target::Field(..))).cloned().collect()
}

pub(super) fn always_returns(block: &Block) -> bool {
    match block.statements().last() {
        Some(Statement::Return(_)) => true,
        Some(Statement::Do(d)) => d.block().is_some_and(|b| always_returns(&b)),
        Some(Statement::If(chain)) => {
            chain.else_branch().and_then(|e| e.block()).is_some_and(|b| always_returns(&b))
                && chain.if_branches().iter().all(|branch| branch.block().is_some_and(|b| always_returns(&b)))
        }
        _ => false,
    }
}
