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

//! WoW objects: frames, pools, callback registries and script handlers

use super::*;

impl<'a> Walker<'a> {
    /// `CreateFrame("Button", ...)` gives a `Button`: the frame type named by a
    /// literal first argument, if it's a kind of `Frame`, or the type of the
    /// template it's made from (the fourth argument; see `templated`)
    pub(super) fn create_frame(&mut self, call: &FunctionCall, callee: Type, arguments: &[Expression], argument_types: &Values) -> Option<Values> {
        if self.definitions.global("CreateFrame") != Some(callee) {
            return None
        }
        let kind = self.frame_type(arguments.first())?;
        let (named, extended) = self.templated(kind, arguments.get(3));
        let at = self.call_location(call);
        // A kind the function making it is passed: each call's own
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        if let Some((_, index)) = arguments.first().and_then(|kind| self.parameter_read(kind)).filter(|&(function, _)| Some(function) == caller) {
            self.kind_parameters.insert(at, index);
        }
        // The third argument, or nil without one
        self.apply(Write::Parent(at, argument_types.get(2)));
        self.add_lua_mixins(at, named, argument_types.get(2));
        let named = NamedRef::Instance { named, at, extended };
        Some(Values { values: vec![Type { named, ..Type::of(TypeSet::TABLE) }], rest: Type::NIL })
    }

    /// `table.create(n)`: a new empty table, as `{}` written at the call is
    pub(super) fn create_table(&self, call: &FunctionCall, callee: Type) -> Option<Values> {
        if self.builtin("table.create") != Some(callee) {
            return None
        }
        let at = self.call_location(call);
        Some(Values { values: vec![Type::table(at)], rest: Type::NIL })
    }

    /// The type of an object of type `kind` made from the template `template`
    /// names, and whether it may have members beyond that type's. With no
    /// template, it's `kind`. With one known template for that kind of object
    /// (`CreateFrame("Button", nil, nil, "UIPanelButtonTemplate")`), it's the
    /// template's type. Otherwise, as with several templates, one for another
    /// kind, or one wow_ls doesn't know, it's `kind`, and the template can add
    /// members
    pub(super) fn templated(&self, kind: NamedId, template: Option<&Expression>) -> (NamedId, bool) {
        let text = match template {
            None => return (kind, false),
            Some(Expression::Literal(literal)) if literal.is_nil() => return (kind, false),
            Some(Expression::Literal(literal)) => literal.get_string(),
            Some(_) => None,
        };
        let name = text.as_deref().map(|t| t.trim().trim_matches(['"', '\'']).trim());
        match name.and_then(|name| self.definitions.template(name)) {
            Some((named, element)) if element == kind => (named, false),
            _ => (kind, true),
        }
    }

    /// The kind of `Frame` a literal frame type names, else `Frame`
    pub(super) fn frame_type(&self, argument: Option<&Expression>) -> Option<NamedId> {
        let frame = self.definitions.named_type_id("Frame")?;
        Some(self.kind_named(argument, frame))
    }

    /// The kind of `base` a literal argument names (`"Button"`, `"Flipbook"`),
    /// ignoring case, else `base`
    pub(super) fn kind_named(&self, argument: Option<&Expression>, base: NamedId) -> NamedId {
        match argument {
            Some(Expression::Literal(literal)) => literal.get_string()
                .and_then(|text| self.definitions.named_type_id_ignoring_case(text.trim().trim_matches(['"', '\''])))
                .filter(|&id| self.definitions.is_a(id, base))
                .unwrap_or(base),
            _ => base,
        }
    }

    /// What a pool made in the files holds, from this walk if it's been made
    /// yet, else the previous one
    pub(super) fn pool_object(&self, pool: TableId) -> Option<Type> {
        self.note(Read::PoolObject(pool));
        self.pool_objects.get(&pool)
            .or_else(|| self.layer.pool_objects.get(&pool))
            .or_else(|| self.prior?.pool_objects.get(&pool))
            .copied()
    }

    /// `CreateFramePool`, `CreateTexturePool`, `CreateFontStringPool` and
    /// `CreateObjectPool` make a pool, whose objects are all one object here:
    /// of the type the pool makes (or its create function returns), made with
    /// the pool's parent. Its init and reset functions are called with it, so
    /// the fields and mixins they give it are what `Acquire` returns
    pub(super) fn create_pool(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let is = |name: &str| self.builtin(name) == Some(callee);
        let at = self.call_location(call);
        let object_at = Location { slot: POOL_OBJECT, ..at };
        // Made from a template, as with `CreateFrame`
        let region = |kind: Option<NamedId>, template: usize| -> Option<Type> {
            let (named, extended) = self.templated(kind?, expressions.get(template));
            let named = NamedRef::Instance { named, at: object_at, extended };
            Some(Type { named, ..Type::of(TypeSet::TABLE) })
        };
        // The object, its parent, then each callback's argument position and
        // whether it's given the pool before the object
        let (object, parent, callbacks): (Type, Option<Type>, &[(usize, bool)]) = if is("CreateFramePool") {
            (region(self.frame_type(expressions.first()), 2)?, Some(arguments.get(1)), &[(3, true), (5, false)])
        } else if is("CreateTexturePool") {
            (region(self.definitions.named_type_id("Texture"), 3)?, Some(arguments.get(0)), &[(4, true)])
        } else if is("CreateFontStringPool") {
            (region(self.definitions.named_type_id("FontString"), 3)?, Some(arguments.get(0)), &[(4, true)])
        } else if is("CreateObjectPool") {
            // Whatever the create function returns, which it's given the pool for
            let pool = self.pool_type(at);
            let create = arguments.get(0);
            let object = match create.function {
                FunctionRef::One(id) => self.returns_of(id).first(),
                FunctionRef::Builtin(id) => self.definitions.function(id).map_or(Type::UNKNOWN, |f| f.returns.first()),
                _ => Type::UNKNOWN,
            };
            self.pass_to_callback(expressions.first(), create, Values { values: vec![pool], rest: Type::NIL });
            (object, None, &[(1, true)])
        } else {
            return None
        };
        if let Some(parent) = parent {
            self.apply(Write::Parent(object_at, parent));
        }
        let pool = self.pool_type(at);
        self.apply(Write::PoolObject(at, object));
        for &(index, with_pool) in callbacks {
            let values = if with_pool { vec![pool, object] } else { vec![object] };
            self.pass_to_callback(expressions.get(index), arguments.get(index), Values { values, rest: Type::NIL });
        }
        Some(Values { values: vec![pool], rest: Type::NIL })
    }

    /// A pool made at `at`
    pub(super) fn pool_type(&self, at: TableId) -> Type {
        match self.definitions.named_type_id("ObjectPool") {
            // Only the main methods are listed, so others aren't reported
            Some(named) => Type { named: NamedRef::Instance { named, at, extended: true }, ..Type::of(TypeSet::TABLE) },
            None => Type::table(at),
        }
    }

    /// `registry:TriggerEvent("Name", ...)` and `registry:RegisterCallback("Name",
    /// callback, owner, ...)` on a table made in the files with Blizzard's
    /// `CallbackRegistryMixin`: each callback is called with its owner (a
    /// number made for it when there's none), what was registered after the
    /// owner, then what each `TriggerEvent` of that event passes
    pub(super) fn callback_registry(&mut self, receiver: Type, callee: Type, arguments: &[Expression], argument_types: &Values) {
        let is = |w: &Walker, path: &str| callee.function != FunctionRef::None && w.builtin(path).is_some_and(|b| b.function == callee.function);
        let (trigger, register) = (is(self, "CallbackRegistryMixin.TriggerEvent"), is(self, "CallbackRegistryMixin.RegisterCallback"));
        if !trigger && !register {
            return
        }
        let Some(at) = receiver.named.identity() else { return };
        let Some(Expression::Literal(literal)) = arguments.first() else { return };
        let Some(event) = literal.get_string() else { return };
        let event = name_key(event.trim_matches(['"', '\'']));
        let after = |from: usize| Values { values: argument_types.values.iter().skip(from).copied().collect(), rest: argument_types.rest };
        if trigger {
            self.apply(Write::Triggered(at, event, after(1)));
            return
        }
        self.note(Read::Triggered(at, event));
        let prior = self.prior.and_then(|prior| prior.triggered.get(&(at, event)));
        let triggered = match (prior, self.layer.triggered.get(&(at, event))) {
            (Some(a), Some(b)) => a.union(b),
            (Some(found), None) | (None, Some(found)) => found.clone(),
            // Never triggered, so never called
            (None, None) => return,
        };
        let owner = match arguments.len() {
            0..=2 => Type::of(TypeSet::NUMBER),
            _ => {
                let owner = argument_types.get(2);
                match owner.set.contains(TypeSet::NIL) {
                    true => owner.without(TypeSet::NIL) | Type::of(TypeSet::NUMBER),
                    false => owner,
                }
            }
        };
        let mut values = vec![owner];
        let extras = after(3);
        // What was registered after the owner comes first, when how many
        // there are is known
        let rest = match arguments.len() > 3 && matches!(arguments.last(), Some(Expression::VarArgs(_) | Expression::FunctionCall(_))) {
            true => {
                values.extend(extras.values);
                Type::UNKNOWN
            }
            false => {
                values.extend(extras.values.iter().take(arguments.len().saturating_sub(3)).copied());
                values.extend(triggered.values);
                triggered.rest
            }
        };
        // A method's parameters don't count `self`, which the owner is
        let callback = argument_types.get(1);
        if self.known_function(callback.function).is_some_and(|f| f.method) {
            values.remove(0);
        }
        self.pass_to_callback(arguments.get(1), callback, Values { values, rest });
    }

    /// `frame:SetScript("OnEvent", handler)` or `HookScript`, with the game's
    /// method: the game calls the handler with the frame, then the arguments
    /// that kind of handler gets (see `xml::handler_parameters`)
    pub(super) fn script_handler(&mut self, receiver: Type, callee: Type, method: &str, arguments: &[Expression], argument_types: &Values) {
        if !matches!(method, "SetScript" | "HookScript") || !matches!(callee.function, FunctionRef::Builtin(_)) {
            return
        }
        let Some(Expression::Literal(literal)) = arguments.first() else { return };
        let Some(script) = literal.get_string() else { return };
        let script = script.trim_matches(['"', '\'']);
        let mut values = vec![receiver];
        values.extend(crate::xml::handler_parameters(script).iter().map(|&(_, types)| Type::of(types)));
        // An event's payload, which isn't known
        let rest = if script == "OnEvent" { Type::UNKNOWN } else { Type::NIL };
        // A method's parameters don't count `self`, which the frame is
        let handler = argument_types.get(1);
        if self.known_function(handler.function).is_some_and(|f| f.method) {
            values.remove(0);
            // The frame, which the game calls it with (not nil, as setting
            // it would have failed)
            for function in handler.function.alternatives() {
                if let FunctionRef::One(id) = function {
                    self.apply(Write::SelfPassed(id, receiver.without(TypeSet::NIL), false));
                }
            }
        }
        self.pass_to_callback(arguments.get(1), handler, Values { values, rest });
    }

    /// `a:m(...)` where `a` is one of several objects: what's passed goes to
    /// each object's own `m` that's known to be one function
    pub(super) fn pass_to_each_method(&mut self, call: &FunctionCall, base: Type, method: &str, expressions: &[Expression], arguments: &Values) {
        if !matches!(base.named, NamedRef::Several(_) | NamedRef::Tables(_)) {
            return
        }
        for object in base.named.alternatives() {
            let receiver = Type { named: object, ..base };
            let FunctionRef::One(id) = self.field_read(receiver, method).function else { continue };
            let Some(takes_self) = self.known_function(FunctionRef::One(id)).map(|f| f.method) else { continue };
            // A step of a loop through a field, as for a call to it alone
            self.note_loop_call(call, id, expressions, if takes_self { 0 } else { 1 });
            if takes_self {
                self.apply(Write::SelfPassed(id, receiver, true));
            }
            let values = match takes_self {
                true => arguments.clone(),
                false => {
                    let mut values = vec![receiver];
                    values.extend(arguments.values.iter().copied());
                    Values { values, rest: arguments.rest }
                }
            };
            self.apply(Write::CallArguments(id, values));
        }
    }

    /// A function given to a pool, which calls it with `arguments`. That call
    /// is seen like any other, and passing it there doesn't let it escape
    pub(super) fn pass_to_callback(&mut self, expression: Option<&Expression>, function: Type, arguments: Values) {
        let FunctionRef::One(id) = function.function else { return };
        self.apply(Write::CallArguments(id, arguments));
        let Some(Expression::Identifier(identifier)) = expression else {
            // A function written right there, which nothing else can call
            if matches!(expression, Some(Expression::Function(_))) {
                self.apply(Write::PoolCallback(id));
            }
            return
        };
        // The read of the variable or field holding it counts as a call
        let range = identifier.syntax().text_range();
        if identifier.simple_name().is_some() {
            if let Some(reference) = self.references.iter_mut().rev().find(|r| r.file == self.file && range.contains_range(r.range)) {
                reference.callee = true;
            }
        } else if field_access(identifier.syntax()).is_some() {
            if let Some(field) = self.field_references.iter_mut().rev().find(|f| f.file == self.file && !f.write && range.contains_range(f.range)) {
                field.callee = true;
            }
        }
    }

    /// The parents an object made in the files is known to have
    pub(super) fn known_parent(&self, at: TableId) -> Option<Type> {
        self.note(Read::Parent(at));
        // From the previous walk, which saw every `SetParent`, and this one so
        // far. Parents only gain types, so the two together are never wrong
        let prior = self.prior.and_then(|prior| prior.parents.get(&at)).copied();
        let current = self.parents.get(&at).copied();
        let layered = self.layer.parents.get(&at).copied();
        match (prior, current, layered) {
            (None, None, None) => None,
            (a, b, c) => Some(a.unwrap_or_default() | b.unwrap_or_default() | c.unwrap_or_default()),
        }
    }

    /// Where each of several objects `ty` may be was made, when it's that
    pub(super) fn several_objects(&self, ty: Type) -> Vec<TableId> {
        match ty.named {
            NamedRef::Several(_) => ty.named.alternatives().into_iter().filter_map(NamedRef::identity).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether the table made at `at` is a mixin mixed into WoW objects, as
    /// the previous walk found
    pub(super) fn is_mixin(&self, at: TableId) -> bool {
        self.note(Read::MixedInto(at));
        self.prior.is_some_and(|prior| prior.mixed_into.contains_key(&at)) || self.layer.mixed_into.contains_key(&at)
    }

    /// Where a call is, for what it makes and what loops over it get: where it
    /// starts, unless a call it makes its callee starts there too, as
    /// `a:b():c()`'s `a:b()` does, whose table would be the same one. Then its
    /// method's name, or its arguments, where no other call starts
    pub(super) fn call_location(&self, call: &FunctionCall) -> Location {
        let start = call.syntax().text_range().start();
        let shared = call.callee().is_some_and(|callee| callee.syntax().descendants().any(|n| n.kind() == SyntaxKind::FunctionCall));
        let at = match shared {
            false => start,
            true => call.method_name().map(|name| name.text_range().start())
                .or_else(|| call.syntax().children().find(|n| n.kind() == SyntaxKind::ArgumentList).map(|n| n.text_range().start()))
                .unwrap_or(start),
        };
        self.location(at)
    }

    /// Whether a table or object was made by the function being walked: there,
    /// or new from a call there. Not a pool's object or a copy's table
    pub(super) fn made_here(&self, at: TableId) -> bool {
        at.slot & (POOL_OBJECT | COPY) == 0 && at.file == self.file && self.function_ranges.last().is_some_and(|(range, _)| range.contains(at.offset))
    }

    /// The innermost function being walked that made a table or object (there,
    /// or new from a call there), or a pool whose object it is, as its number
    /// in the walk
    pub(super) fn maker(&self, at: TableId) -> Option<usize> {
        if at.file != self.file || at.slot & COPY != 0 {
            return None
        }
        self.function_ranges.iter().rev().find(|(range, _)| range.contains(at.offset)).map(|&(_, function)| function)
    }

    /// The function (by its number in the walk) and index of the parameter
    /// that `expression` reads, if it reads one that still holds its argument.
    /// It can be an enclosing function's
    /// The parameter of the function being walked `expression` reads, if it
    /// reads one as it was passed
    pub(super) fn own_parameter_read(&self, expression: &Expression) -> Option<usize> {
        let caller = self.function_ranges.last().map(|&(_, function)| function).filter(|_| self.function != MAIN_CHUNK)?;
        self.parameter_read(expression).filter(|&(function, _)| function == caller).map(|(_, index)| index)
    }
}
