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

//! What a function does to what it's passed, kept as summaries to apply at
//! each call, and the loops those make

use super::*;

/// A call whose callee's summaries are being applied (see `ForwardedCall`,
/// `FieldWrite`): what it passes, and where it came from in the function
/// making it, for passing the summaries on as that function's own
pub(super) struct Passing {
    /// By the callee's parameter positions
    pub(super) passed: Values,
    /// For each position, the calling function's parameter it passes on, if
    /// it does
    pub(super) origins: Vec<Option<usize>>,
    /// Whether those past the callee's named parameters are the calling
    /// function's `...`
    pub(super) rest_origin: bool,
    /// How many parameters the callee names
    pub(super) named: usize,
    /// The types of the callee's parameters, as its body sees them
    pub(super) parameters: Vec<Type>,
    pub(super) self_value: Option<Type>,
    /// What `self` is in the calling function's terms, if a summary can be
    /// on it there (see `Walker::write_target`)
    pub(super) self_origin: Option<Result<On, TableId>>,
    /// What the call returns
    pub(super) returns: Values,
}

impl Passing {
    /// The calling function's parameter passed on at `position`, if it is one
    pub(super) fn origin(&self, position: usize) -> Option<usize> {
        self.origins.get(position).copied().flatten()
    }

    /// What's passed at `position`, kept to the parameter's type, as the
    /// callee's body sees it, which what it's passed to may narrow: the
    /// parameter's type where nothing's known of what's passed
    pub(super) fn argument(&self, position: usize) -> Type {
        let passed = self.passed.get(position);
        let Some(&parameter) = self.parameters.get(position) else { return passed };
        match says_nothing(passed) {
            true => parameter,
            false => narrowed_to(passed, parameter),
        }
    }

    /// `part` of a summary of the callee, in the calling function's terms:
    /// what it passes on of its own, or else the value passed, as `argument`
    /// gives it
    pub(super) fn in_caller_terms(&self, part: Forwarded, argument: &dyn Fn(usize) -> Type) -> Vec<Forwarded> {
        match part {
            Forwarded::Parameter(i) => vec![self.origin(i).map_or_else(|| Forwarded::Value(argument(i)), Forwarded::Parameter)],
            Forwarded::Truthy(i) => vec![self.origin(i).map_or_else(|| Forwarded::Value(argument(i).truthy()), Forwarded::Truthy)],
            Forwarded::Rest if self.rest_origin => vec![Forwarded::Rest],
            Forwarded::Rest => self.passed.values.iter().skip(self.named).map(|&ty| Forwarded::Value(ty)).collect(),
            value => vec![value],
        }
    }
}

/// Whether nothing's known of a value: it may be any value that's true (and
/// perhaps false or nil), as `unknown`, `unknown (not nil)` and `unknown
/// (truthy)` are
pub(super) fn says_nothing(ty: Type) -> bool {
    ty.set.contains(TypeSet::UNKNOWN.without(TypeSet::NIL | TypeSet::FALSE))
}

/// Another handle on the same code
pub(super) fn copy_expression(expression: &Expression) -> Option<Expression> {
    Expression::cast(expression.syntax().clone())
}

/// What a call returns for a field (`ReturnedField`), as the function's body
/// returns `body`: that field's type for the call, unless nothing is known
/// of it, kept to what the body could return
pub(super) fn narrowed_to(field: Type, body: Type) -> Type {
    if field.set == TypeSet::UNKNOWN || field.set == TypeSet::NEVER {
        return body
    }
    if body.set == TypeSet::UNKNOWN {
        return field
    }
    match field.set.intersection(body.set) {
        TypeSet::NEVER => body,
        set => field.without(TypeSet::UNKNOWN.without(set)),
    }
}

impl<'a> Walker<'a> {
    pub(super) fn add_loop_edge(&mut self, edge: LoopEdge) {
        if let Some(edges) = self.loop_edges.last_mut().filter(|edges| !edges.contains(&edge)) {
            edges.push(edge);
        }
    }

    /// Field `field` is written `value`: a step of a loop through it if
    /// that's a parameter, or what a function returns
    pub(super) fn note_loop_write(&mut self, on: Type, field: &str, value: Option<&Expression>) {
        let Some(value) = value else { return };
        let on = loop_tables(on.named);
        for value in loop_operands(value) {
            if let Some(index) = self.own_parameter_read(&value) {
                self.add_loop_edge(LoopEdge::ParameterToField(index, on.clone(), field.to_string()));
            } else if let Expression::FunctionCall(call) = &value
                && let Some(&callee) = self.callees.get(&(self.file, call.syntax().text_range())) {
                self.add_loop_edge(LoopEdge::ReturnToField(callee, on.clone(), field.to_string()));
            }
        }
    }

    /// A call to `callee` passing `arguments`, each `offset` from the
    /// parameter it fills: the fields and parameters it passes on
    pub(super) fn note_loop_call(&mut self, call: &FunctionCall, callee: FunctionId, arguments: &[Expression], offset: isize) {
        self.callees.insert((self.file, call.syntax().text_range()), callee);
        for (i, argument) in arguments.iter().enumerate() {
            let Ok(position) = usize::try_from(i as isize + offset) else { continue };
            for argument in loop_operands(argument) {
            let argument = &argument;
            if let Some(index) = self.own_parameter_read(argument) {
                self.add_loop_edge(LoopEdge::ParameterToParameter(index, callee, position));
            } else if let Some((_, field)) = match argument { Expression::Identifier(identifier) => field_access(identifier.syntax()), _ => None } {
                // What it was read from, as the read just walked found
                let on = self.field_references.iter().rev()
                    .find(|reference| reference.file == self.file && reference.range == field.text_range() && !reference.write)
                    .map(|reference| loop_tables(reference.base))
                    .unwrap_or_default();
                self.add_loop_edge(LoopEdge::FieldToParameter(on, field.text().to_string(), callee, position));
            }
            }
        }
    }

    pub(super) fn parameter_read(&self, expression: &Expression) -> Option<(usize, usize)> {
        let Expression::Identifier(identifier) = expression else { return None };
        let id = self.local_binding(identifier.simple_name()?.text())?;
        let binding = &self.bindings[id];
        if binding.reassigned {
            return None
        }
        Some((binding.function, *self.parameter_index.get(&id)?))
    }

    /// The call to `callee` as one to settle at the end of the walk, if it
    /// passes a parameter of the code calling it (or its `...`) straight on.
    /// `known` is its arguments by the callee's parameter positions
    pub(super) fn pending_call(&self, callee: FunctionId, arguments: &[Expression], known: &Values, callee_method: bool, is_method: bool) -> Option<PendingCall> {
        // How far each argument is from the parameter it fills: `self` is
        // separate for a method, whether passed with `:` or explicitly
        let offset: isize = match (callee_method, is_method) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        let mut parameters = Vec::new();
        let mut varargs = None;
        for (i, argument) in arguments.iter().enumerate() {
            let Ok(position) = usize::try_from(i as isize + offset) else { continue };
            // The top of a file has no caller: its `...` is the addon's
            if i + 1 == arguments.len() && matches!(argument, Expression::VarArgs(_)) && self.function != MAIN_CHUNK {
                varargs = self.function_ids.last().map(|&function| (position, function));
                continue
            }
            let Expression::Identifier(identifier) = argument else { continue };
            let Some(id) = identifier.simple_name().and_then(|name| self.local_binding(name.text())) else { continue };
            let binding = &self.bindings[id];
            // Only while it still holds what was passed
            if binding.reassigned {
                continue
            }
            let (Some(&index), Some(&function)) = (self.parameter_index.get(&id), self.function_numbers.get(&binding.function)) else { continue };
            // Narrowed here, as in `if type(x) == "string" then f(x) end`. An
            // enclosing function's parameter isn't narrowed
            let narrowed = (binding.function == self.function)
                .then(|| self.current[id])
                .filter(|&now| self.parameter_entry.get(&id) != Some(&now))
                .map(|now| now.set);
            parameters.push((position, function, index, narrowed));
        }
        let recursive = varargs.is_some_and(|(_, function)| self.recursive(function, callee));
        (!parameters.is_empty() || varargs.is_some()).then(|| PendingCall { callee, known: known.clone(), parameters, varargs, recursive })
    }

    /// A function's new object, for one call: a new identity at the call, with
    /// the object the function made mixed in (for the fields it set there)
    /// along with that call's arguments for the parameters it mixed in
    /// What `expression`, in a function, is that a `FieldWrite` can be on: its
    /// `self`, a parameter, or a table it made (of type `ty`)
    pub(super) fn write_target(&self, expression: &Expression, ty: Type) -> Option<Result<On, TableId>> {
        if self.function == MAIN_CHUNK {
            return None
        }
        let caller = self.function_ranges.last().map(|&(_, function)| function)?;
        if let Expression::Identifier(identifier) = expression
            && let Some(name) = identifier.simple_name() {
            let self_binding = self.self_bindings.last().copied().flatten();
            if self_binding.is_some() && self.local_binding(name.text()) == self_binding {
                return Some(Ok(On::SelfValue))
            }
        }
        if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| function == caller) {
            return Some(Ok(On::Parameter(index)))
        }
        let at = ty.named.identity().filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at)))?;
        Some(Err(at))
    }

    /// What `value` is in terms of the function's parameters, if it's one,
    /// or `p or v` with `p` one and `v` a literal or one
    pub(super) fn value_parts(&self, value: &Expression) -> Option<Vec<Forwarded>> {
        let caller = self.function_ranges.last().map(|&(_, function)| function)?;
        let parameter = |e: &Expression| self.parameter_read(e).filter(|&(function, _)| function == caller).map(|(_, index)| index);
        let literal = |e: &Expression| -> Option<Type> {
            let Expression::Literal(literal) = e else { return None };
            let set = if literal.get_number().is_some() { TypeSet::NUMBER }
                else if literal.get_string().is_some() { TypeSet::STRING }
                else { match literal.get_bool().as_deref() { Some("true") => TypeSet::TRUE, Some("false") => TypeSet::FALSE, _ => return None } };
            Some(Type::of(set))
        };
        if let Some(index) = parameter(value) {
            return Some(vec![Forwarded::Parameter(index)])
        }
        let Expression::BinaryExpression(binary) = value else { return None };
        let parts = binary.flatten();
        let [crate::ast::BinaryPart::Operand(left), crate::ast::BinaryPart::Operator(Operator::Or), crate::ast::BinaryPart::Operand(right)] = parts.as_slice() else { return None };
        let left = Forwarded::Truthy(parameter(left)?);
        let right = match (parameter(right), literal(right)) {
            (Some(index), _) => Forwarded::Parameter(index),
            (None, Some(ty)) => Forwarded::Value(ty),
            (None, None) => return None,
        };
        Some(vec![left, right])
    }

    /// Notes `base.field = value` in a function, as a `FieldWrite`, when it
    /// gives what it's passed to `self`, a parameter or a table it makes
    pub(super) fn note_field_write(&mut self, base: &Expression, ty: Type, field: &str, value: Option<&Expression>) {
        let Some(value) = value.and_then(|value| self.value_parts(value)) else { return };
        let Some(on) = self.write_target(base, ty) else { return };
        self.add_field_write(on, field.to_string(), value);
    }

    pub(super) fn add_field_write(&mut self, on: Result<On, TableId>, field: String, value: Vec<Forwarded>) {
        self.deps_to_signature();
        if let Some(writes) = self.field_writes.last_mut() {
            let write = (on, field, value);
            if !writes.contains(&write) {
                writes.push(write);
            }
        }
    }

    /// The field of `self` or a parameter `expression` reads, if it's one
    pub(super) fn returned_field(&self, expression: &Expression) -> Option<ReturnedField> {
        let Expression::Identifier(identifier) = expression else { return None };
        let (base, field) = field_access(identifier.syntax())?;
        match self.write_target(&base, Type::default())? {
            Ok(on @ (On::SelfValue | On::Parameter(_))) => Some(ReturnedField { on, field: field.text().to_string() }),
            _ => None,
        }
    }

    /// The origins of a call's arguments and `self`, in the terms of the
    /// function making it (see `Passing`): from its argument `expressions`,
    /// each `offset` from the parameter it fills, to a callee with parameters
    /// of types `parameters`
    pub(super) fn passing(&self, passed: &Values, parameters: &[Type], self_value: Option<(Type, Option<Expression>)>, returns: &Values, expressions: &[Expression], offset: isize) -> Passing {
        let named = parameters.len();
        let caller = self.function_ranges.last().map(|&(_, function)| function).filter(|_| self.function != MAIN_CHUNK);
        let expression = |position: usize| usize::try_from(position as isize - offset).ok().and_then(|i| expressions.get(i));
        let positions = passed.values.len().max(named);
        let origins = (0..positions)
            .map(|i| expression(i).and_then(|e| self.parameter_read(e)).filter(|&(function, _)| Some(function) == caller).map(|(_, index)| index))
            .collect();
        // Its `...` passed as the last argument, filling those past the named
        let rest_origin = caller.is_some() && usize::try_from(named as isize - offset).ok()
            .is_some_and(|i| i + 1 == expressions.len() && matches!(expressions.get(i), Some(Expression::VarArgs(_))));
        let self_origin = caller.and(self_value.as_ref())
            .and_then(|(ty, e)| e.as_ref().and_then(|e| self.write_target(e, *ty)));
        Passing { passed: passed.clone(), origins, rest_origin, named, parameters: parameters.to_vec(), self_value: self_value.map(|(ty, _)| ty), self_origin, returns: returns.clone() }
    }

    /// Applies what a callee's summaries say a call to it does, for the
    /// call `call` describes: the fields it gives (`field_writes`) and the
    /// methods it calls on what it's passed (`forwarded_calls`). `method` the
    /// method called, if it's
    /// a method call; `within` the functions whose summaries these come from,
    /// stopping a method calling itself
    pub(super) fn apply_summaries(&mut self, field_writes: &[FieldWrite], forwarded_calls: &[ForwardedCall], method: Option<(&str, FunctionRef)>, call: &Passing, within: &mut Vec<FunctionRef>) {
        for write in field_writes {
            self.give_field(write, method, call);
        }
        for forwarded in forwarded_calls {
            self.forward_call(forwarded, call, within);
        }
    }

    /// What `on` is in the calling function's terms, for passing a summary on
    pub(super) fn origin_of(&self, on: On, call: &Passing) -> Option<Result<On, TableId>> {
        let made = |ty: Type| ty.named.identity().filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at))).map(Err);
        match on {
            On::SelfValue => call.self_origin,
            On::Parameter(k) => call.origin(k).map(|j| Ok(On::Parameter(j))).or_else(|| made(call.passed.get(k))),
            On::Returned(slot) => made(call.returns.get(slot)),
        }
    }

    /// The field `write` stands for. Given to the table it's on (of several
    /// `self` may be, those whose method is `method`), and passed on as the
    /// calling function's own when that's its `self`, a parameter or a table
    /// it makes. Not a write to the calling function's own `self` by a method
    /// it calls on it, as that call is passed on itself (a `ForwardedCall` on
    /// `self`), to call whichever method `self` has at each call
    pub(super) fn give_field(&mut self, write: &FieldWrite, method: Option<(&str, FunctionRef)>, call: &Passing) {
        let on = match write.on {
            On::SelfValue => match call.self_value {
                Some(ty) => ty,
                None => return,
            },
            On::Parameter(k) => call.passed.get(k),
            On::Returned(slot) => call.returns.get(slot),
        };
        let origin = self.origin_of(write.on, call);
        let passed_on_by_call = origin == Some(Ok(On::SelfValue)) && write.on == On::SelfValue;
        let target = origin.filter(|_| !passed_on_by_call);
        let argument = |i: usize| call.argument(i);
        // Given here, but for what the caller passes on of what it was passed
        // when the write is passed on too: its type here isn't what each call
        // to the caller passes, which the caller's own write gives, as a
        // parameter passed straight on is settled by its calls (see
        // `pending_call`)
        let passed_on = |part: Forwarded| match part {
            Forwarded::Parameter(i) | Forwarded::Truthy(i) => call.origin(i).is_some(),
            Forwarded::Rest => call.rest_origin,
            Forwarded::Value(_) => false,
        };
        let value = write.value.iter()
            .filter(|&&part| origin.is_none() || !passed_on(part))
            .map(|&part| match part {
                Forwarded::Parameter(i) => argument(i),
                Forwarded::Truthy(i) => argument(i).truthy(),
                Forwarded::Value(ty) => ty,
                Forwarded::Rest => call.passed.rest,
            })
            // Nothing known of it, which says nothing of what the table holds:
            // the function's own write gives it what its body knows
            .filter(|&ty| !says_nothing(ty))
            .reduce(Type::union);
        // Only a table that's known, not a field name of every table
        if let Some(value) = value.filter(|_| on.named.identity().is_some() || matches!(on.named, NamedRef::Several(_))) {
            let objects: Vec<TableId> = match (write.on, method, on.named) {
                (On::SelfValue, Some((name, function)), NamedRef::Several(_)) => on.named.alternatives().into_iter()
                    .filter(|&object| self.field_read(Type { named: object, ..on }, name).function == function)
                    .filter_map(|object| object.identity())
                    .collect(),
                _ => on.named.identity().into_iter().chain(self.several_objects(on)).collect(),
            };
            for at in objects {
                self.apply(Write::Field(at, write.field.clone(), value));
            }
        }
        let Some(target) = target else { return };
        let parts: Vec<Forwarded> = write.value.iter()
            .flat_map(|&part| call.in_caller_terms(part, &argument))
            .collect();
        self.add_field_write(target, write.field.clone(), parts);
    }

    /// For `receiver:m()` in a function, with `receiver` of type `base` and
    /// `m` of type `method`: what the method may be on, for a `ForwardedCall`,
    /// if it's the function's `self`, one of its parameters or a table it
    /// made with parameters mixed in. The method only one of the files' own,
    /// or one it can't tell, or one of the definitions' with summaries of its
    /// own: not a WoW object's method, which does nothing more on each call
    pub(super) fn forwardable_receiver(&self, receiver: Option<&Expression>, base: Type, method: Type) -> Option<Vec<On>> {
        if self.function == MAIN_CHUNK {
            return None
        }
        let summarised = match method.function {
            FunctionRef::One(_) | FunctionRef::Several(_) | FunctionRef::Unknown => true,
            FunctionRef::Builtin(_) => self.known_function(method.function).is_some_and(|info| !info.field_writes.is_empty() || !info.forwarded_calls.is_empty()),
            FunctionRef::None => false,
        };
        if !summarised {
            return None
        }
        match self.write_target(receiver?, base)? {
            Ok(on @ (On::SelfValue | On::Parameter(_))) => Some(vec![on]),
            Ok(On::Returned(_)) => None,
            Err(at) => {
                let mixed = self.parameter_mixins.get(&at).filter(|mixed| !mixed.is_empty())?;
                Some(mixed.iter().map(|&p| On::Parameter(p)).collect())
            }
        }
    }

    /// Notes that the function being walked makes `call` for each call to it,
    /// once: a recursive function passing its parameter on to itself makes
    /// the call it already makes, which would otherwise be added again each
    /// walk, never settling
    pub(super) fn add_forwarded(&mut self, call: ForwardedCall) {
        self.deps_to_signature();
        if let Some(calls) = self.forwarded.last_mut()
            && !calls.contains(&call) {
            calls.push(call);
        }
    }

    /// The arguments of a call a `ForwardedCall` stands for: the function's
    /// parameters passed on, its `...`, and other values
    pub(super) fn forwarded_arguments(&self, expressions: &[Expression], values: &Values) -> Vec<Forwarded> {
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        let mut forwarded = Vec::new();
        for (i, expression) in expressions.iter().enumerate() {
            if i + 1 == expressions.len() && matches!(expression, Expression::VarArgs(_)) {
                forwarded.push(Forwarded::Rest);
                break
            }
            match self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                Some((_, index)) => forwarded.push(Forwarded::Parameter(index)),
                None => forwarded.push(Forwarded::Value(values.get(i))),
            }
        }
        forwarded
    }

    /// The call `forwarded` stands for, made by the call `call` describes. It
    /// calls the method of the first of what it may be on (the last first)
    /// that has it, as known there, with its own summaries, and is passed on
    /// in turn when what it's on is the calling function's `self` or a
    /// parameter, which then makes the call for each of its own calls
    pub(super) fn forward_call(&mut self, forwarded: &ForwardedCall, call: &Passing, within: &mut Vec<FunctionRef>) {
        // A method called on the calling function's own `self` that calls
        // this on its `self`: the caller's own summary has the call on `self`
        // (see `forwardable_receiver`), which each call to it resolves on
        // what `self` is there, this with it. Copying it up too would make
        // every method's summary every method it can reach
        if call.self_origin == Some(Ok(On::SelfValue)) && forwarded.on.contains(&On::SelfValue) {
            return
        }
        // Passed on by the function calling, when it's on its `self` or a
        // parameter: then each call to it makes the call, on what that call
        // passes, so there's nothing to do here
        if self.function != MAIN_CHUNK {
            let on: Vec<On> = forwarded.on.iter()
                .filter_map(|&on| match self.origin_of(on, call) {
                    Some(Ok(on @ (On::SelfValue | On::Parameter(_)))) => Some(on),
                    _ => None,
                })
                .collect();
            if !on.is_empty() {
                let arguments = forwarded.arguments.iter().flat_map(|&part| call.in_caller_terms(part, &|i| call.argument(i))).collect();
                self.add_forwarded(ForwardedCall { on, method: forwarded.method.clone(), arguments });
                return
            }
        }
        let Some((receiver, on, function)) = forwarded.on.iter().rev().find_map(|&on| {
            let receiver = match on {
                On::SelfValue => call.self_value?,
                On::Parameter(k) => call.passed.get(k),
                On::Returned(_) => return None,
            };
            match self.field_read(receiver, &forwarded.method).function {
                function @ (FunctionRef::One(_) | FunctionRef::Builtin(_)) => Some((receiver, on, function)),
                _ => None,
            }
        }) else { return };
        // What it passes the method
        let mut values = Vec::new();
        let mut rest = Type::NIL;
        for &part in &forwarded.arguments {
            match part {
                Forwarded::Parameter(i) => values.push(call.argument(i)),
                Forwarded::Truthy(i) => values.push(call.argument(i).truthy()),
                Forwarded::Value(ty) => values.push(ty),
                Forwarded::Rest => {
                    values.extend(call.passed.values.iter().skip(call.named).copied());
                    rest = call.passed.rest;
                }
            }
        }
        let Some(info) = self.known_function(function) else { return };
        let (method_takes_self, named) = (info.method, info.parameters.len());
        let (field_writes, forwarded_calls) = (info.field_writes.clone(), info.forwarded_calls.clone());
        let parameters: Vec<Type> = info.parameters.iter().map(|p| p.types).collect();
        if let FunctionRef::One(id) = function {
            // Only what's known: this adds to what the body's own call passes
            // (or calls the method's file sees), and what says nothing would
            // swamp it
            let mut passed: Vec<Type> = values.iter().map(|&ty| if says_nothing(ty) { Type::default() } else { ty }).collect();
            // Called with `:`, so `self` is separate unless the function takes it
            if !method_takes_self {
                passed.insert(0, receiver);
            } else {
                self.apply(Write::SelfPassed(id, receiver, true));
            }
            let rest = if says_nothing(rest) { Type::default() } else { rest };
            self.apply(Write::CallArguments(id, Values { values: passed, rest }));
        }
        // What the method does, in turn: deep enough for real chains of
        // methods, and never one already being applied
        if within.len() >= 6 || within.contains(&function) || (field_writes.is_empty() && forwarded_calls.is_empty()) {
            return
        }
        let mut origins: Vec<Option<usize>> = forwarded.arguments.iter()
            .take_while(|part| !matches!(part, Forwarded::Rest))
            .map(|&part| match part {
                Forwarded::Parameter(i) => call.origin(i),
                _ => None,
            })
            .collect();
        let rest_origin = forwarded.arguments.last() == Some(&Forwarded::Rest) && call.rest_origin;
        origins.resize(origins.len().max(values.len()), None);
        let self_origin = match on {
            On::SelfValue => call.self_origin,
            on => self.origin_of(on, call),
        };
        let inner = Passing {
            passed: Values { values, rest },
            origins,
            rest_origin: rest_origin && forwarded.arguments.len() - 1 == named,
            named,
            parameters,
            self_value: Some(receiver),
            self_origin,
            returns: Values::none(),
        };
        within.push(function);
        self.apply_summaries(&field_writes, &forwarded_calls, Some((&forwarded.method, function)), &inner, within);
        within.pop();
    }
}

/// `a.b` as (`a`, `b`), for an expression `a` and a field name `b`. None for
/// indexing (`a[b]`) and plain names
/// The tables a field on a value of `named` is on, for `LoopEdge`s: the
/// table, each of several, or each object's of several
pub(super) fn loop_tables(named: NamedRef) -> Vec<TableId> {
    match named {
        NamedRef::Several(_) => named.alternatives().into_iter().filter_map(NamedRef::identity).collect(),
        named => named.identities(),
    }
}

/// The values `expression` may be, for `LoopEdge`s: those of `a or b` and
/// `a and b` are either's, and `(a)`'s are `a`'s
pub(super) fn loop_operands(expression: &Expression) -> Vec<Expression> {
    match expression {
        Expression::GroupedExpression(grouped) => grouped.get_expression().map(|e| loop_operands(&e)).unwrap_or_default(),
        Expression::BinaryExpression(binary) => {
            let parts = binary.flatten();
            let logical = parts.iter().all(|part| !matches!(part, BinaryPart::Operator(op) if !matches!(op, Operator::Or | Operator::And)));
            if !logical {
                return Expression::cast(expression.syntax().clone()).into_iter().collect()
            }
            parts.into_iter()
                .filter_map(|part| match part { BinaryPart::Operand(operand) => Some(operand), BinaryPart::Operator(_) => None })
                .flat_map(|operand| loop_operands(&operand))
                .collect()
        }
        _ => Expression::cast(expression.syntax().clone()).into_iter().collect(),
    }
}

/// A pending call's arguments, with what's passed on as settled so far
pub(super) fn resolve_pending(call: &PendingCall, parameters: &HashMap<FunctionId, Vec<Type>>, varargs: &HashMap<FunctionId, Values>) -> Values {
    let mut values = call.known.clone();
    for &(position, function, index, narrowed) in &call.parameters {
        let ty = parameters.get(&function).and_then(|p| p.get(index)).copied().unwrap_or(Type::UNKNOWN);
        let ty = match narrowed {
            Some(set) => ty.without(TypeSet::UNKNOWN.without(set)),
            None => ty,
        };
        if let Some(value) = values.values.get_mut(position) {
            *value = ty;
        }
    }
    if let Some((position, function)) = call.varargs {
        let passed = varargs.get(&function).cloned().unwrap_or_else(Values::unknown);
        values.values.truncate(position);
        if call.recursive {
            // Passing its own `...` back with more in front, as in
            // `f(x, ...)` in `f`, would make `...` one longer each time round.
            // What's in front keeps its place, what's passed on is joined
            values.rest = passed.values.into_iter().fold(passed.rest, |rest, value| rest | value);
        } else {
            values.values.extend(passed.values);
            values.rest = passed.rest;
        }
    }
    values
}
