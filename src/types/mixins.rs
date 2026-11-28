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

//! Tables made from mixins, mixing in, and copying tables

use super::*;

/// What a mixin was mixed into, as far as its methods' `self` goes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MixinTarget {
    /// Nothing seen
    Nothing,
    /// Only WoW objects, all of this type (and whether they may have members
    /// beyond it)
    Object(NamedId, bool),
    /// A plain table, or objects with no type in common
    Other,
}

/// A call that returns fresh objects (see `fresh_object`)
#[derive(Clone, Copy)]
pub(super) struct Making<'c> {
    pub(super) call: &'c FunctionCall,
    /// The types of the call's arguments
    pub(super) arguments: &'c Values,
    /// The call's arguments, each `offset` from the parameter it fills (see
    /// `pending_call`)
    pub(super) expressions: &'c [Expression],
    pub(super) offset: isize,
    /// The definitions' function called, if it's one, whose tables are of a
    /// family (see `family`)
    pub(super) function: Option<BuiltinId>,
}

impl<'a> Walker<'a> {
    pub(super) fn fresh_object(&mut self, making: &Making, slot: usize, value: Type, fresh: &FreshObject) -> Type {
        let Making { call, arguments, expressions, offset, function } = *making;
        let here = self.call_location(call);
        let at = function.and_then(|function| family_member(here, function, slot))
            .unwrap_or(Location { slot: slot as u32 + 1, ..here });
        // Passing on a parameter of the function calling, so each call to that
        // mixes in what it passes, as for `CreateFromMixins(parameter)`
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        for &position in &fresh.mixed_parameters {
            let Some(expression) = usize::try_from(position as isize - offset).ok().and_then(|i| expressions.get(i)) else { continue };
            if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                self.apply(Write::ParameterMixin(at, index));
            }
        }
        // A frame of the kind the call passes, if the function makes one of
        // the kind it's passed: a literal kind names it, and a parameter of
        // the function calling is passed on, for each call to that
        let kind_expression = fresh.kind_parameter
            .and_then(|position| usize::try_from(position as isize - offset).ok())
            .and_then(|i| expressions.get(i));
        let passed_kind = kind_expression.and_then(|expression| {
            if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                self.kind_parameters.insert(at, index);
            }
            let frame = self.definitions.named_type_id("Frame")?;
            Some(self.kind_named(Some(expression), frame)).filter(|&kind| kind != frame)
        });
        let named = match value.named {
            NamedRef::Table(_) => NamedRef::Table(at),
            NamedRef::Instance { named, extended, .. } => NamedRef::Instance { named: passed_kind.unwrap_or(named), at, extended },
            // A definition's `table`
            NamedRef::None | NamedRef::Unknown if value.set.contains(TypeSet::TABLE) && fresh.made.is_none() => NamedRef::Table(at),
            // A definition's table of the type its mixins make (see
            // `Definitions::add_mixin_types`): one of that type
            NamedRef::One(named) if fresh.made.is_none() => {
                let extended = !self.definitions.named_type(named).is_some_and(|t| t.has_methods);
                NamedRef::Instance { named, at, extended }
            }
            other => return Type { named: other, ..value },
        };
        // What the function made, for the fields it set there
        let mut mixins: Vec<Type> = value.named.identity().map(|_| value).into_iter().collect();
        // Unless the type it is has them (see `Definitions::add_mixin_types`)
        if !matches!(named, NamedRef::Instance { .. }) {
            mixins.extend(fresh.mixin_globals.iter().filter_map(|name| self.definitions.global(name)));
        }
        mixins.extend(fresh.mixed_parameters.iter().map(|&i| arguments.get(i)));
        for mixin in &mixins {
            self.record_mixed_into(*mixin, named);
        }
        self.apply(Write::MixinsReplaced(at, mixins));
        for (name, ty) in &fresh.fields {
            self.apply(Write::Field(at, name.clone(), *ty));
        }
        if let Some(parent) = fresh.made.and_then(|made| self.known_parent(made)) {
            self.apply(Write::Parent(at, parent));
        }
        // A pool: its objects are new for the call too, with the call's
        // arguments for the parameters its init or reset function mixed in
        if let (Some(mixed), Some(inner)) = (&fresh.pool_mixed_parameters, fresh.made.and_then(|made| self.pool_object(made))) {
            let object_at = Location { slot: at.slot | POOL_OBJECT, ..at };
            let object = match inner.named {
                NamedRef::Instance { named, extended, .. } => Type { named: NamedRef::Instance { named, at: object_at, extended }, ..inner },
                NamedRef::Table(_) => Type { named: NamedRef::Table(object_at), ..inner },
                _ => inner,
            };
            if object.named.identity() == Some(object_at) {
                let mut mixins = vec![inner];
                mixins.extend(mixed.iter().map(|&i| arguments.get(i)));
                for mixin in &mixins {
                    self.record_mixed_into(*mixin, object.named);
                }
                self.apply(Write::MixinsReplaced(object_at, mixins));
                if let Some(parent) = inner.named.identity().and_then(|inner| self.known_parent(inner)) {
                    self.apply(Write::Parent(object_at, parent));
                }
            }
            self.apply(Write::PoolObject(at, object));
        }
        Type { named, ..value }
    }

    /// `CreateFromMixins(a, b)` makes a table with the fields of `a` and `b`,
    /// and `Mixin(object, a, b)` adds them to `object` and returns it
    pub(super) fn mix(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let (object, mixins, mixin_expressions) = if self.definitions.global("CreateFromMixins") == Some(callee) {
            (Type::table(self.call_location(call)), &arguments.values[..], expressions)
        } else if self.definitions.global("Mixin") == Some(callee) {
            let (object, mixins) = arguments.values.split_first()?;
            (*object, mixins, expressions.get(1..).unwrap_or_default())
        } else {
            return None
        };
        // Mixing a parameter into something the function makes, so each call
        // mixes in what it passes
        // (or an enclosing function makes, like a pool's object in its init function)
        if let Some((at, maker)) = object.named.identity().and_then(|at| Some((at, self.maker(at)?))) {
            let parameters: Vec<usize> = mixin_expressions.iter()
                .filter_map(|e| self.parameter_read(e))
                .filter(|&(function, _)| function == maker)
                .map(|(_, index)| index)
                .collect();
            for parameter in parameters {
                self.apply(Write::ParameterMixin(at, parameter));
            }
        }
        for mixin in mixins {
            self.record_mixed_into(*mixin, object.named);
        }
        if !mixins.is_empty() {
            self.open(object);
        }
        // Only a table or object made in the files can keep track of its
        // mixins: it, or each of several it may be
        let targets: Vec<TableId> = object.named.identity().into_iter().chain(self.several_objects(object)).collect();
        for at in targets {
            for mixin in mixins {
                self.apply(Write::MixinAdded(at, *mixin));
            }
        }
        Some(Values { values: vec![object], rest: Type::NIL })
    }

    /// `mixin` was mixed into a table or object of type `object`
    pub(super) fn record_mixed_into(&mut self, mixin: Type, object: NamedRef) {
        let Some(mixin) = mixin.named.identity() else { return };
        if matches!(object, NamedRef::None | NamedRef::Unknown) {
            return
        }
        self.apply(Write::MixedInto(mixin, object));
    }

    /// `CopyTable(t)` (and `CopyTableSafe`): a new table, made at the call,
    /// with what `t` (or any of the tables it may be) holds: its fields, contents and mixins, and a copy of each
    /// table among them, as the copy is deep. `CopyTable(t, true)` copies
    /// only `t`, sharing the tables it holds
    pub(super) fn copy_table(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let safe = self.definitions.global("CopyTableSafe") == Some(callee);
        if !safe && self.definitions.global("CopyTable") != Some(callee) {
            return None
        }
        let source = arguments.get(0);
        // Any of several tables: a copy of whichever it is, so what any holds
        let sources = match source.named {
            NamedRef::Table(_) | NamedRef::Tables(_) => source.named.identities(),
            _ => return None,
        };
        let shallow = matches!(expressions.get(1), Some(Expression::Literal(literal)) if literal.get_bool().as_deref() == Some("true"));
        let at = self.call_location(call);
        for from in sources {
            self.copy_into(from, at, shallow, &mut vec![from]);
        }
        let mut copy = Type::table(at);
        // `CopyTableSafe(nil)` gives nil
        if safe && source.set.intersection(TypeSet::NIL | TypeSet::FALSE) != TypeSet::NEVER {
            copy |= Type::NIL;
        }
        Some(Values { values: vec![copy], rest: Type::NIL })
    }

    /// Gives table `to` what table `from` holds, as the previous walk (or a
    /// dependency) knows it, each table in it copied too unless `shallow`.
    /// `within` are the tables being copied, which a table holding one of
    /// them shares rather than copying forever
    pub(super) fn copy_into(&mut self, from: TableId, to: TableId, shallow: bool, within: &mut Vec<TableId>) {
        self.apply(Write::CopiedFrom(to, from));
        let Some(prior) = self.prior else { return };
        self.note(Read::Fields(from));
        let mut fields: Vec<(String, Type)> = Vec::new();
        for (name, &ty) in [prior.fields.get(&from), self.layer.fields.get(&from)].into_iter().flatten().flatten() {
            match fields.iter_mut().find(|(n, _)| n == name) {
                Some((_, found)) => *found |= ty,
                None => fields.push((name.clone(), ty)),
            }
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        // A copy into a saved table leaves out values of unknown shape, as a
        // write does (see `stored_in`). A profile copied into itself would
        // otherwise keep one forever once it got in
        for (name, ty) in fields {
            let ty = self.copied(ty, to, &name, shallow, within);
            if let Some(ty) = self.stored_in(to, ty) {
                self.apply(Write::Field(to, name, ty));
            }
        }
        self.note(Read::Entries(from));
        if let Some(entries) = prior.entries.get(&from).or_else(|| self.layer.entries.get(&from)).cloned() {
            let values = self.copied(entries.values, to, "[]", shallow, within);
            if let Some(values) = self.stored_in(to, values) {
                self.apply(Write::Entry(to, entries.keys, values));
            }
        }
        // Mixins' fields were copied into it, so they're the copy's too
        self.note(Read::Mixins(from));
        let mixins = prior.mixins.get(&from).or_else(|| self.layer.mixins.get(&from)).cloned().unwrap_or_default();
        for mixin in mixins {
            self.apply(Write::MixinAdded(to, mixin));
        }
    }

    /// What a copy into table `into` holds under `key` for a value of type
    /// `ty`: a copy of the table it is, made in `into`, unless `shallow`
    pub(super) fn copied(&mut self, ty: Type, into: TableId, key: &str, shallow: bool, within: &mut Vec<TableId>) -> Type {
        let NamedRef::Table(nested) = ty.named else { return ty };
        // Deep enough for any real table, and a table holding itself is shared
        if shallow || nested.slot & PSEUDO != 0 || within.contains(&nested) || within.len() > 8 {
            return ty
        }
        let place = name_key(&format!("{}/{key}", into.slot)) as u32 & (COPY - 1);
        let copy = Location { slot: COPY | place, ..into };
        within.push(nested);
        self.copy_into(nested, copy, false, within);
        within.pop();
        Type { named: NamedRef::Table(copy), ..ty }
    }

    /// What `mixin` was mixed into on this walk, following tables made from it
    /// to what they were mixed into
    pub(super) fn mixin_target(&self, mixin: TableId, depth: usize) -> MixinTarget {
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return MixinTarget::Nothing
        }
        let Some(objects) = self.mixed_into.get(&mixin) else { return MixinTarget::Nothing };
        // Each of several objects it may have been mixed into
        let objects = objects.iter().flat_map(|&object| match object {
            NamedRef::Several(_) => object.alternatives(),
            object => vec![object],
        });
        objects.fold(MixinTarget::Nothing, |target, object| {
            let next = match object.object_type() {
                Some((named, extended)) => MixinTarget::Object(named, extended),
                // A table, which is plain unless it's mixed into something too
                None => match object.identity().map(|at| self.mixin_target(at, depth + 1)) {
                    Some(MixinTarget::Nothing) | None => MixinTarget::Other,
                    Some(target) => target,
                },
            };
            match (target, next) {
                (MixinTarget::Nothing, x) | (x, MixinTarget::Nothing) => x,
                (MixinTarget::Object(a, e), MixinTarget::Object(b, f)) => match self.definitions.common_ancestor(a, b) {
                    Some(common) => MixinTarget::Object(common, e || f),
                    None => MixinTarget::Other,
                },
                _ => MixinTarget::Other,
            }
        })
    }
}
