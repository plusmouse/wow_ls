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

//! Walking calls: finding their callees and what they return

use super::*;

/// What a call to one function gives (see `Walker::call_one`)
pub(super) struct Called {
    pub(super) returns: Values,
    /// What the function's own record says it returns, before the call's
    /// arguments refine it
    pub(super) documented: Values,
    /// What it requires of each argument
    pub(super) required: Vec<Option<Type>>,
    /// Where it's passed a template, if it takes one
    pub(super) template: Option<usize>,
}

impl Called {
    /// Calling either: what either returns, and what both require
    pub(super) fn join(self, other: Called) -> Called {
        let required = (0..self.required.len().max(other.required.len()))
            .map(|i| match (self.required.get(i).copied().flatten(), other.required.get(i).copied().flatten()) {
                (Some(a), Some(b)) if a == b => Some(a),
                _ => None,
            })
            .collect();
        Called {
            returns: self.returns.union(&other.returns),
            documented: self.documented.union(&other.documented),
            required,
            template: self.template.or(other.template),
        }
    }
}

/// Where a call's callee was read
#[derive(Clone, Copy)]
pub(super) enum CalleeRead {
    /// An index into `references`
    Variable(usize),
    /// An index into `field_references`
    Field(usize),
}

impl<'a> Walker<'a> {
    /// A call: what it passes depends on its callee and arguments, not on
    /// what the callee returns, which the rest of the statement uses
    pub(super) fn call(&mut self, call: &FunctionCall) -> Values {
        let scope = self.deps.get_mut().last().and_then(|body| {
            let level = body.levels.last()?;
            Some(CallScope {
                level: body.levels.len() - 1,
                reads: level.reads.len(),
                locals: level.locals.len(),
                writes: level.writes.len(),
                pending: level.pending.len(),
                callee: None,
                ended: false,
            })
        });
        let tracked = scope.is_some();
        if let Some(scope) = scope {
            self.call_scopes.push(scope);
        }
        let values = self.call_walked(call);
        if tracked {
            self.end_call_scope(None);
            self.call_scopes.pop();
        }
        values
    }

    /// The call being walked has written what it passes, if it hadn't
    /// already: what it wrote depends on what it read. `callee`'s signature
    /// was read only for whether it's a method, so that's all what's passed
    /// depends on of it; the rest of the statement reads all of it
    pub(super) fn end_call_scope(&mut self, callee: Option<FunctionId>) {
        let Some(scope) = self.call_scopes.last_mut() else { return };
        let Some(body) = self.deps.get_mut().last_mut() else { return };
        if scope.ended || scope.level + 1 != body.levels.len() {
            return
        }
        scope.ended = true;
        let level = body.levels.last_mut().expect("checked above");
        let writes = level.writes.split_off(scope.writes.min(level.writes.len()));
        let pending = level.pending.split_off(scope.pending.min(level.pending.len()));
        if writes.is_empty() && pending.is_empty() {
            return
        }
        let mut dep = Dep::of(&level.reads[scope.reads..], &level.locals[scope.locals..]);
        if let Some(callee) = callee
            && let Some(&function) = body.key_numbers.get(&Read::Function(callee))
            && dep.reads.remove(function) {
            let method = body.key(&Read::Method(callee));
            dep.reads.insert(method);
        }
        let dep = Arc::new(dep);
        for write in writes {
            body.write_deps[write] = Some(dep.clone());
        }
        for pending in pending {
            body.pending_deps.push((pending, dep.clone()));
        }
    }

    /// What finding the callee of the call being walked read
    pub(super) fn callee_found(&mut self) -> Option<(usize, std::ops::Range<usize>, std::ops::Range<usize>)> {
        let scope = self.call_scopes.last()?;
        let (reads, locals) = scope.callee?;
        Some((scope.level, scope.reads..reads, scope.locals..locals))
    }

    pub(super) fn call_walked(&mut self, call: &FunctionCall) -> Values {
        let is_method = call.method_name().is_some();
        // Where the callee's read was recorded, to note what this call returns
        let mut callee_read = None;
        let callee = call.callee().map(|callee| {
            let types = self.expression(&callee);
            match &callee {
                // The read `expression` just recorded
                Expression::Identifier(i) if !is_method && (i.simple_name().is_some() || self.global_table_access(i.syntax()).is_some()) => {
                    if let Some(reference) = self.references.last_mut() {
                        reference.callee = true;
                        callee_read = Some(CalleeRead::Variable(self.references.len() - 1));
                    }
                    self.check_called_variable(types);
                }
                Expression::Identifier(i) if !is_method && field_access(i.syntax()).is_some() => {
                    self.last_field_is_callee();
                    callee_read = self.field_references.len().checked_sub(1).map(CalleeRead::Field);
                    if let Some((_, name)) = field_access(i.syntax()) {
                        let base = self.field_references.last().map_or(NamedRef::None, |field| field.base);
                        self.check_called_field(base, name.text(), name.text_range(), types);
                    }
                }
                // `t["f"]()`: the field reference `expression` just recorded
                // for the key, if it's one naming a field
                Expression::Identifier(i) if !is_method && index_access(i.syntax()).is_some() => {
                    let Some((_, Expression::Literal(key))) = index_access(i.syntax()) else { return types };
                    let name = key.get_string().as_deref().and_then(string_field_name).map(str::to_string);
                    let range = key.syntax().text_range();
                    let base = self.field_references.last()
                        .filter(|field| range.contains_range(field.range))
                        .map(|field| field.base);
                    if let (Some(name), Some(base)) = (name, base) {
                        self.check_called_field(base, &name, range, types);
                    }
                }
                _ => {}
            }
            types
        });
        let receiver = if is_method { callee } else { None };
        // `a:m()` calls field `m` of `a`
        // `p:m()` on a parameter, or on a table made here with parameters
        // mixed in: the parameters that may hold `m`, for a `ForwardedCall`
        let mut forward_on = None;
        let callee = match (callee, call.method_name()) {
            (Some(base), Some(method)) => {
                let types = self.field_read(base, method.text());
                forward_on = self.forwardable_receiver(call.callee().as_ref(), base, types);
                self.field_reference(call.syntax(), &method, base, types, false);
                self.last_field_is_callee();
                if forward_on.is_some() {
                    let function = self.function_ids.last().copied();
                    if let Some(field) = self.field_references.last_mut() {
                        field.forwarded = function;
                    }
                }
                callee_read = Some(CalleeRead::Field(self.field_references.len() - 1));
                self.check_called_field(base.named, method.text(), method.text_range(), types);
                self.check_method(base, &method);
                Some(types)
            }
            (callee, _) => callee,
        };
        // Which function it calls depends only on what was read so far
        if let (Some(scope), Some(body)) = (self.call_scopes.last_mut(), self.deps.get_mut().last())
            && let Some(level) = body.levels.get(scope.level).filter(|_| body.levels.len() == scope.level + 1) {
            scope.callee = Some((level.reads.len(), level.locals.len()));
        }
        let arguments = call.arguments();
        let argument_types = self.expression_values(&arguments);
        if let (Some(on), Some(method)) = (forward_on, call.method_name()) {
            let forwarded = self.forwarded_arguments(&arguments, &argument_types);
            self.add_forwarded(ForwardedCall { on, method: method.text().to_string(), arguments: forwarded });
        }
        if let Some(callee) = callee {
            self.table_library_call(call, callee, arguments.len(), &argument_types);
        }
        // A method of one of several objects: which one it calls isn't known,
        // but each object's own method of that name gets what's passed
        if let (Some(base), Some(method), Some(FunctionRef::Unknown)) = (receiver, call.method_name(), callee.map(|c| c.function)) {
            self.pass_to_each_method(call, base, method.text(), &arguments, &argument_types);
        }
        // Calling a table whose metatable has `__call` calls that, with the table first
        let call_metamethod = callee
            .filter(|c| !c.set.contains(TypeSet::FUNCTION))
            .and_then(|c| Some((c, self.metamethod(self.prior?, c.named.identity()?, "__call", 0)?)))
            .filter(|(_, f)| f.set.contains(TypeSet::FUNCTION));
        let (callee, receiver, is_method) = match call_metamethod {
            Some((table, function)) => (Some(function), Some(table), true),
            None => (callee, receiver, is_method),
        };
        let Some(callee) = callee.filter(|c| c.set.contains(TypeSet::FUNCTION)) else {
            // Nothing known of it yet, like a function read as an upvalue on the
            // first walk, so nothing known of what it returns. Unknown would
            // stay in what it flows into, even a function's own returns
            return match callee {
                Some(callee) if callee.set == TypeSet::NEVER => Values::nothing(),
                _ => Values::unknown(),
            }
        };
        let called = match callee.function {
            // Any of several functions: it calls one of them, so it returns
            // what any of them does, and each gets what's passed
            FunctionRef::Several(_) => {
                let mut joined: Option<Called> = None;
                let mut bailed: Option<Values> = None;
                for function in callee.function.alternatives() {
                    let one = Type { function, ..callee };
                    match self.call_one(call, one, receiver, is_method, &arguments, &argument_types) {
                        Ok(called) => joined = Some(match joined {
                            None => called,
                            Some(joined) => joined.join(called),
                        }),
                        Err(returns) => bailed = Some(bailed.map_or_else(|| returns.clone(), |b| b.union(&returns))),
                    }
                }
                match (joined, bailed) {
                    (None, bailed) => return bailed.unwrap_or_else(Values::unknown),
                    // One it can't call yet: none of what's required of
                    // the arguments is certain
                    (Some(joined), Some(bailed)) => Called {
                        returns: joined.returns.union(&bailed),
                        documented: joined.documented.union(&bailed),
                        required: Vec::new(),
                        template: joined.template,
                    },
                    (Some(joined), None) => joined,
                }
            }
            _ => match self.call_one(call, callee, receiver, is_method, &arguments, &argument_types) {
                Ok(called) => called,
                Err(returns) => return returns,
            },
        };
        let Called { mut returns, documented, required, template } = called;
        if let Some(receiver) = receiver.filter(|_| is_method) {
            self.callback_registry(receiver, callee, &arguments, &argument_types);
            if let Some(method) = call.method_name() {
                self.script_handler(receiver, callee, method.text(), &arguments, &argument_types);
            }
        }
        if let Some(receiver) = receiver {
            returns = self.object_method(call, receiver, &arguments, template, &argument_types, returns);
        }
        self.require_arguments(&arguments, &required);
        if returns != documented {
            let call_returns = Some(returns.clone());
            match callee_read {
                Some(CalleeRead::Variable(i)) => if let Some(r) = self.references.get_mut(i) { r.call_returns = call_returns },
                Some(CalleeRead::Field(i)) => if let Some(f) = self.field_references.get_mut(i) { f.call_returns = call_returns },
                None => {}
            }
        }
        returns
    }

    /// What calling `callee`, one function (not `Several`), does: what it
    /// returns, and what it requires of the arguments. Or, if it isn't known
    /// yet, what the call returns then
    pub(super) fn call_one(&mut self, call: &FunctionCall, callee: Type, receiver: Option<Type>, is_method: bool, arguments: &[Expression], argument_types: &Values) -> Result<Called, Values> {
        let Some(function) = self.known_function(callee.function) else {
            // A function the files make but this first walk hasn't reached yet
            return Err(match (callee.function, self.prior) {
                (FunctionRef::One(_), None) => Values::nothing(),
                _ => Values::unknown(),
            })
        };
        // The definitions' functions don't list `self`, as `:` passes it, but
        // `s:format(x)` is `string.format(s, x)`: the string is its first
        // parameter, so the arguments come after it
        let string_receiver = is_method && !matches!(callee.function, FunctionRef::One(_))
            && receiver.is_some_and(|r| self.definitions.member_source(r) != r);
        let skipped = usize::from(string_receiver);
        let required: Vec<Option<Type>> = (0..arguments.len()).map(|i| function.mandated(i + skipped)).collect();
        // Where the call passes a template, if it takes one
        let template = function.parameters.iter()
            .position(|p| p.name.to_ascii_lowercase().contains("template"));
        let documented = function.returns.clone();
        let fresh_returns = function.fresh_returns.clone();
        let forwarded_calls = function.forwarded_calls.clone();
        let field_writes = function.field_writes.clone();
        let returned_fields = function.returned_fields.clone();
        // The callee's parameters as its body sees them, for its summaries
        let settled_parameters: Vec<Type> = match function.field_writes.is_empty() && function.forwarded_calls.is_empty() {
            true => Vec::new(),
            false => function.parameters.iter().map(|p| p.types).collect(),
        };
        let callee_method = function.method;
        // How far each argument is from the parameter it fills (see `pending_call`)
        let offset: isize = match (callee_method, is_method) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        // What the function's own parameters get: `self` is separate for a
        // method, whether it's passed with `:` or explicitly
        let parameter_types = match (function.method, is_method) {
            (true, false) => Values { values: argument_types.values.iter().skip(1).copied().collect(), rest: argument_types.rest },
            (false, true) => {
                let mut values = vec![receiver.unwrap_or(Type::UNKNOWN)];
                values.extend(argument_types.values.iter().copied());
                Values { values, rest: argument_types.rest }
            }
            _ => argument_types.clone(),
        };
        let mut returns = match callee.function {
            FunctionRef::One(id) => {
                self.note_loop_call(call, id, arguments, offset);
                // A method is given a `self`: what it's called on, or the first argument
                if callee_method {
                    let passed = if is_method { receiver.unwrap_or(Type::UNKNOWN) } else { argument_types.get(0) };
                    self.apply(Write::SelfPassed(id, passed, true));
                }
                match self.pending_call(id, arguments, &parameter_types, callee_method, is_method) {
                    Some(pending) => {
                        self.pending_calls.push(pending);
                        let index = self.pending_calls.len() - 1;
                        if let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
                            level.pending.push(index);
                        }
                    }
                    None => self.apply(Write::CallArguments(id, parameter_types.clone())),
                }
                self.end_call_scope(Some(id));
                self.callee_found = self.callee_found();
                let mut returns = self.returns_of(id);
                for (slot, fresh) in fresh_returns.iter().enumerate() {
                    if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                        returns.values[slot] = self.fresh_object(&Making { call, arguments: &parameter_types, expressions: arguments, offset, function: None }, slot, value, fresh);
                    }
                }
                // A loop over this call gets what one over the function's
                // own return does
                if let Some(iterated) = self.known_function(FunctionRef::One(id)).and_then(|f| f.iterated.clone()) {
                    self.iterated.insert(self.call_location(call), iterated);
                }
                returns
            }
            _ => self.select(callee, arguments, argument_types)
                .or_else(|| self.library_returns(callee, arguments, argument_types))
                .or_else(|| self.create_frame(call, callee, arguments, argument_types))
                .or_else(|| self.create_table(call, callee))
                .or_else(|| self.create_pool(call, callee, arguments, argument_types))
                .or_else(|| self.mix(call, callee, arguments, argument_types))
                .or_else(|| self.copy_table(call, callee, arguments, argument_types))
                .unwrap_or_else(|| {
                    // A definition's function returning a new table at each
                    // call, with arguments mixed in
                    let builtin = match callee.function { FunctionRef::Builtin(id) => Some(id), _ => None };
                    let mut returns = documented.clone();
                    for (slot, fresh) in fresh_returns.iter().enumerate() {
                        if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                            returns.values[slot] = self.fresh_object(&Making { call, arguments: argument_types, expressions: arguments, offset: 0, function: builtin }, slot, value, fresh);
                        }
                    }
                    returns
                }),
        };
        // What the function calls on what this call passes it. A definition's
        // function has its arguments by position
        let passed = if matches!(callee.function, FunctionRef::One(_)) { (&parameter_types, offset) } else { (argument_types, 0) };
        // `self`, and the code giving it
        let self_value = match (is_method, callee_method) {
            (true, _) => receiver.map(|r| (r, call.callee())),
            (false, true) => Some((argument_types.get(0), arguments.first().and_then(copy_expression))),
            (false, false) => None,
        };
        if !field_writes.is_empty() || !forwarded_calls.is_empty() {
            let passing = self.passing(passed.0, &settled_parameters, self_value, &returns, arguments, passed.1);
            let method = call.method_name().map(|name| name.text().to_string());
            self.apply_summaries(&field_writes, &forwarded_calls, method.as_deref().map(|name| (name, callee.function)), &passing, &mut vec![callee.function]);
        }
        let self_value = match (is_method, callee_method) {
            (true, _) => receiver,
            (false, true) => Some(argument_types.get(0)),
            (false, false) => None,
        };
        for (slot, field) in returned_fields.iter().enumerate() {
            let Some(field) = field else { continue };
            let on = match field.on {
                On::SelfValue => self_value,
                On::Parameter(p) => Some(passed.0.get(p)),
                On::Returned(_) => None,
            };
            let (Some(on), Some(&value)) = (on, returns.values.get(slot)) else { continue };
            // A table made in the files, or one the definitions give the field
            // (a global table like `NORMAL_FONT_COLOR`, with its `r`): not
            // what any table's field of that name may be
            let declared = self.definitions.field(self.definitions.member_source(on), &field.field).is_some();
            if on.named.identity().is_none() && !matches!(on.named, NamedRef::Several(_)) && !declared {
                continue
            }
            // What it returns isn't known yet: nothing it says narrows the field
            let held = self.field_read(on, &field.field);
            returns.values[slot] = if value.set == TypeSet::NEVER { held } else { narrowed_to(held, value) };
        }
        Ok(Called { returns, documented, required, template })
    }

    /// Methods that make or change WoW objects' parents. `returns` is what the
    /// method's documentation says it returns, and `template` which argument
    /// names the template it makes an object from
    pub(super) fn object_method(&mut self, call: &FunctionCall, receiver: Type, expressions: &[Expression], template: Option<usize>, arguments: &Values, mut returns: Values) -> Values {
        let Some(method) = call.method_name() else { return returns };
        let name = method.text();
        let kind = expressions.first();
        let template = template.and_then(|i| expressions.get(i));
        let parent = receiver.named.identity();
        if let Some(object) = parent.and_then(|at| self.pool_object(at)) {
            match name {
                "Acquire" => return Values { values: vec![object, Type::of(TypeSet::BOOLEAN)], rest: Type::NIL },
                "GetNextActive" => return Values { values: vec![object | Type::NIL], rest: Type::NIL },
                "EnumerateActive" => {
                    self.enumerated.insert(self.call_location(call), object);
                }
                _ => {}
            }
        }
        match name {
            "SetParent" => {
                if let Some(at) = parent {
                    self.apply(Write::Parent(at, arguments.first()));
                }
                // Whichever of several objects it is
                for object in self.several_objects(receiver) {
                    self.apply(Write::Parent(object, arguments.first()));
                }
            }
            "GetParent" => {
                // What it was made with or given, if it was made in the files
                if let Some(parent) = parent.and_then(|at| self.known_parent(at)) {
                    returns = Values { values: vec![parent], rest: Type::NIL };
                }
                // Any of several objects made in the files, each with a known parent
                if let NamedRef::Several(_) = receiver.named {
                    let parents: Option<Vec<Type>> = receiver.named.alternatives().into_iter()
                        .map(|object| self.known_parent(object.identity()?))
                        .collect();
                    if let Some(parent) = parents.and_then(|parents| parents.into_iter().reduce(Type::union)) {
                        returns = Values { values: vec![parent], rest: Type::NIL };
                    }
                }
                // `self` in a mixin's method: any object it's mixed into,
                // some perhaps made where the analysis can't see (from a
                // template named by a variable), so not a parent to report
                // missing methods by
                if parent.is_some_and(|at| self.is_mixin(at)) {
                    for value in &mut returns.values {
                        value.named = value.named.loosened();
                    }
                }
            }
            // `parent:CreateTexture()` and the like: a new object of the type the
            // method returns, made with the receiver as its parent
            name if name.starts_with("Create") => {
                let Some(first) = returns.values.first_mut() else { return returns };
                let NamedRef::One(child) = first.named else { return returns };
                if !self.definitions.named_type(child).is_some_and(|t| t.has_methods) {
                    return returns
                }
                // `CreateAnimation("Alpha")` makes the kind of animation it names
                let child = match name {
                    "CreateAnimation" => self.kind_named(kind, child),
                    _ => child,
                };
                let at = self.call_location(call);
                let (named, extended) = self.templated(child, template);
                first.named = NamedRef::Instance { named, at, extended };
                self.apply(Write::Parent(at, receiver));
                self.add_lua_mixins(at, named, receiver);
            }
            _ => {}
        }
        returns
    }
}
