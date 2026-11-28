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

//! Narrowing types by conditions: `if x then`, `type(x) == "string"`, `x == "a"`

use super::*;

/// What a condition proves about the locals it tests: the type each has
/// where it's true, and where it's false
#[derive(Default)]
pub(super) struct Narrowing {
    pub(super) when_true: Vec<(Target, Type)>,
    pub(super) when_false: Vec<(Target, Type)>,
}

/// What a condition narrows: a local of the function being walked, or a
/// field reached from one by names (`self.margin`, `info.a.b`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    Local(BindingId),
    Field(BindingId, Vec<String>),
}

impl Narrowing {
    /// Without the fields it narrows, for when something may change them
    /// before the narrowing would apply
    pub(super) fn locals_only(self) -> Narrowing {
        let locals = |narrowed: Vec<(Target, Type)>| narrowed.into_iter().filter(|(t, _)| matches!(t, Target::Local(_))).collect();
        Narrowing { when_true: locals(self.when_true), when_false: locals(self.when_false) }
    }
}

impl Narrowing {
    pub(super) fn negated(self) -> Narrowing {
        Narrowing { when_true: self.when_false, when_false: self.when_true }
    }
}

/// Both of two sets of narrowed types holding: a local in both is the
/// intersection
pub(super) fn both(mut a: Vec<(Target, Type)>, b: Vec<(Target, Type)>) -> Vec<(Target, Type)> {
    for (target, ty) in b {
        match a.iter_mut().find(|(other, _)| *other == target) {
            Some((_, existing)) => *existing = intersect(*existing, ty.set),
            None => a.push((target, ty)),
        }
    }
    a
}

/// Whether evaluating `node` may call a function, which could change a
/// table's fields. `type(x)`, the usual test, can't
pub(super) fn may_call(walker: &Walker, node: &SyntaxNode) -> bool {
    node.descendants()
        .filter_map(FunctionCall::cast)
        .any(|call| !walker.calls_library_global(&call, "type"))
}

/// The part of `ty` that's in `set`. If none of it is, the analysis must have
/// missed something, so `set` itself
pub(super) fn intersect(ty: Type, set: TypeSet) -> Type {
    // Nothing known yet stays nothing, so types only grow walk by walk
    if ty.set == TypeSet::NEVER {
        return ty
    }
    let narrowed = ty.without(TypeSet::UNKNOWN.without(set));
    if narrowed.set == TypeSet::NEVER { Type::of(set) } else { narrowed }
}

/// The type set `type(x)` names
pub(super) fn type_name_set(name: &str) -> Option<TypeSet> {
    Some(match name {
        "nil" => TypeSet::NIL,
        "boolean" => TypeSet::BOOLEAN,
        "number" => TypeSet::NUMBER,
        "string" => TypeSet::STRING,
        "function" => TypeSet::FUNCTION,
        "table" => TypeSet::TABLE,
        "userdata" | "thread" => TypeSet::OTHER,
        _ => return None,
    })
}

impl<'a> Walker<'a> {
    /// Runs `f` with locals narrowed to the types given. Afterwards each is
    /// what it was before, with anything `f` assigned it, as if never narrowed
    pub(super) fn with_narrowing<R>(&mut self, narrowed: &[(Target, Type)], f: impl FnOnce(&mut Self) -> R) -> R {
        let saved: Vec<(BindingId, Type)> = narrowed.iter()
            .filter_map(|(target, _)| match target { Target::Local(id) => Some((*id, self.current[*id])), Target::Field(..) => None })
            .collect();
        let (saved_fields, epoch) = (self.narrowed_fields.clone(), self.field_epoch);
        for (target, ty) in narrowed {
            self.narrow(target, *ty);
        }
        let result = f(self);
        for (id, before) in saved {
            self.set_current(id, self.current[id] | before);
        }
        // As before, unless `f` may have changed fields
        if self.field_epoch == epoch {
            self.narrowed_fields = saved_fields;
        } else {
            self.narrowed_fields.clear();
        }
        result
    }

    /// Narrows the locals among `narrowed` from here. Inside an `if`, which
    /// joins what its branches leave, so they aren't put back after
    pub(super) fn narrow_locals(&mut self, narrowed: &[(Target, Type)]) {
        for (target, ty) in narrowed {
            if let Target::Local(_) = target {
                self.narrow(target, *ty);
            }
        }
    }

    /// The condition of a branch of the `if` being walked has been: what it
    /// did happens whichever branch is taken after it
    pub(super) fn conditions_walked(&mut self) {
        let Some(join) = self.joins.last() else { return };
        let changed: Vec<BindingId> = join.before.keys().copied().collect();
        let held: Vec<(BindingId, Type)> = changed.into_iter().map(|id| (id, self.current[id])).collect();
        if let Some(join) = self.joins.last_mut() {
            join.after_conditions.extend(held);
        }
    }

    /// A branch of an `if`
    pub(super) fn branch_block(&mut self, block: Option<Block>) {
        if let Some(block) = block {
            self.push_scope(true, block.syntax().text_range().end());
            if let Some(scope) = self.scopes.last_mut() {
                scope.joined = true;
            }
            self.statements(&block);
            self.pop_scope();
        }
    }

    /// The end of a branch of the `if` being walked: what it leaves, if it
    /// goes on past the `if` (`goes_on`), joins `ends`, and the next branch
    /// starts from what the `if` started with
    pub(super) fn end_branch(&mut self, ends: &mut HashMap<BindingId, Type>, goes_on: bool) {
        let Some(join) = self.joins.last() else { return };
        let before: Vec<(BindingId, Type)> = join.before.iter()
            .map(|(&id, &(ty, _))| (id, join.after_conditions.get(&id).copied().unwrap_or(ty)))
            .collect();
        if goes_on {
            for &(id, _) in &before {
                *ends.entry(id).or_default() |= self.current[id];
            }
            if let Some(join) = self.joins.last_mut() {
                join.fell_through += 1;
            }
        }
        for (id, ty) in before {
            self.set_current(id, ty);
        }
    }

    /// Takes `target` to be of type `ty` from here
    pub(super) fn narrow(&mut self, target: &Target, ty: Type) {
        match target {
            Target::Local(id) => self.set_current(*id, ty),
            Target::Field(root, path) => {
                match self.narrowed_fields.iter_mut().find(|(r, p, _)| r == root && p == path) {
                    Some((_, _, existing)) => *existing = ty,
                    None => self.narrowed_fields.push((*root, path.clone(), ty)),
                }
            }
        }
    }

    /// Something may have changed any table's fields: none stay narrowed
    pub(super) fn forget_fields(&mut self) {
        self.narrowed_fields.clear();
        self.field_epoch += 1;
    }

    /// `e` as a local of the current function and the field names from it,
    /// for `x` (no names) or `x.a.b`
    pub(super) fn field_path(&self, e: &Expression) -> Option<(BindingId, Vec<String>)> {
        let Expression::Identifier(identifier) = e else { return None };
        if let Some(name) = identifier.simple_name() {
            // In the main chunk, a global is followed as it's assigned, as
            // `if G == nil then G = {} end` needs, so it narrows like a local
            let id = match self.local_binding(name.text()) {
                Some(id) => id,
                None if self.function == MAIN_CHUNK => *self.globals.get(name.text())?,
                None => return None,
            };
            return (self.bindings[id].function == self.function).then(|| (id, Vec::new()))
        }
        let (base, field) = field_access(identifier.syntax())?;
        let (id, mut path) = self.field_path(&base)?;
        path.push(field.text().to_string());
        Some((id, path))
    }

    /// The narrowed type of the field `e` reads, if it's narrowed here
    pub(super) fn narrowed_field(&self, e: &Expression) -> Option<Type> {
        if self.narrowed_fields.is_empty() {
            return None
        }
        let (root, path) = self.field_path(e)?;
        self.narrowed_fields.iter().find(|(r, p, _)| *r == root && *p == path).map(|(_, _, ty)| *ty)
    }

    /// A local of the current function named by `expression`, and its type
    /// here. Upvalues aren't narrowed, as a closure can run at any time
    /// Or a field reached from one by names, while nothing changes it (see
    /// `narrowed_fields`)
    pub(super) fn narrowable(&self, expression: &Expression) -> Option<(Target, Type)> {
        let (root, path) = self.field_path(expression)?;
        // A global the files don't assign is its preset, as a read finds
        let held = self.builtin_global(root).unwrap_or(self.current[root]);
        if path.is_empty() {
            return Some((Target::Local(root), held))
        }
        // Its type here: as narrowed, or read from what it's on
        let mut ty = held;
        for (i, name) in path.iter().enumerate() {
            ty = match self.narrowed_fields.iter().find(|(r, p, _)| *r == root && p[..] == path[..=i]) {
                Some((_, _, narrowed)) => *narrowed,
                None => self.field_read(ty, name),
            };
        }
        Some((Target::Field(root, path), ty))
    }

    /// What `condition` being true, or false, proves about the locals it tests
    pub(super) fn narrowing(&self, condition: &Expression) -> Narrowing {
        match condition {
            Expression::GroupedExpression(g) => g.get_expression().map(|e| self.narrowing(&e)).unwrap_or_default(),
            Expression::UnaryExpression(u) if u.kind() == Operator::Not => {
                // `not not x` nests, so count them
                let nots = u.syntax().descendants_with_tokens().filter(|t| t.kind() == SyntaxKind::NotKeyword).count();
                let narrowing = u.operand().map(|e| self.narrowing(&e)).unwrap_or_default();
                if nots % 2 == 1 { narrowing.negated() } else { narrowing }
            }
            Expression::BinaryExpression(b) => self.binary_tree(b).map(|tree| self.operation_narrowing(&tree)).unwrap_or_default(),
            _ => match self.narrowable(condition) {
                Some((target, ty)) => Narrowing { when_true: vec![(target.clone(), ty.truthy())], when_false: vec![(target, ty.falsy())] },
                None => Narrowing::default(),
            },
        }
    }

    pub(super) fn operation_narrowing(&self, operation: &Operation) -> Narrowing {
        match operation {
            Operation::Operand(e) => self.narrowing(e),
            // Both true; false says nothing about which wasn't
            // What the left proves of fields doesn't hold past a call on the right
            Operation::Binary(Operator::And, left, right) => Narrowing {
                when_true: both(self.left_narrowing(left, right).when_true, self.operation_narrowing(right).when_true),
                when_false: Vec::new(),
            },
            Operation::Binary(Operator::Or, left, right) => Narrowing {
                when_true: Vec::new(),
                when_false: both(self.left_narrowing(left, right).when_false, self.operation_narrowing(right).when_false),
            },
            Operation::Binary(op @ (Operator::Equals | Operator::NotEquals), left, right) => {
                let (Operation::Operand(left), Operation::Operand(right)) = (&**left, &**right) else { return Narrowing::default() };
                let narrowing = self.comparison(left, right).or_else(|| self.comparison(right, left)).unwrap_or_default();
                if *op == Operator::NotEquals { narrowing.negated() } else { narrowing }
            }
            _ => Narrowing::default(),
        }
    }

    /// What the left of an `and`/`or` proves once its right has run: nothing of
    /// fields when the right may call a function, which could change them
    pub(super) fn left_narrowing(&self, left: &Operation, right: &Operation) -> Narrowing {
        let narrowing = self.operation_narrowing(left);
        match right.syntax_nodes().iter().any(|node| may_call(self, node)) {
            true => narrowing.locals_only(),
            false => narrowing,
        }
    }

    /// Whether `call` calls the preset global function `name` (not a method,
    /// and not while a local or the files' own global hides it)
    pub(super) fn calls_library_global(&self, call: &FunctionCall, name: &str) -> bool {
        let Some(Expression::Identifier(callee)) = call.callee() else { return false };
        call.method_name().is_none()
            && callee.simple_name().is_some_and(|n| n.text() == name)
            && self.is_global(name)
            && self.definitions.global(name).is_some()
            // A dependency's global of that name isn't the library's
            && !self.layer.globals.contains_key(name)
            && self.globals.get(name).is_none_or(|&id| self.is_unassigned_global(id))
    }

    /// `x == nil` or `type(x) == "string"`, with the tested side first
    pub(super) fn comparison(&self, tested: &Expression, against: &Expression) -> Option<Narrowing> {
        let Expression::Literal(literal) = against else { return None };
        if literal.is_nil() {
            let (target, ty) = self.narrowable(tested)?;
            return Some(Narrowing { when_true: vec![(target.clone(), Type::NIL)], when_false: vec![(target, ty.without(TypeSet::NIL))] })
        }
        // `x == "a"`: then it's that string, else not it
        if let (Some(source), Some((target, ty))) = (literal.get_string(), self.narrowable(tested)) {
            let equal = Type::string_literal(&source);
            // Nothing known yet stays nothing, so types only grow walk by walk
            let when_true = if ty.set == TypeSet::NEVER { ty } else { equal };
            let when_false = match equal.literal.strings() {
                Some(&[text]) if ty.literal.strings().is_some() => match ty.literal.without(text) {
                    Literals::None => ty.without(TypeSet::STRING),
                    literal => Type { literal, ..ty },
                },
                _ => ty,
            };
            return Some(Narrowing { when_true: vec![(target.clone(), when_true)], when_false: vec![(target, when_false)] })
        }
        // `type(x)`, while `type` is the library's
        let Expression::FunctionCall(call) = tested else { return None };
        if !self.calls_library_global(call, "type") {
            return None
        }
        let [argument] = call.arguments().try_into().ok()?;
        let (target, ty) = self.narrowable(&argument)?;
        let set = type_name_set(string_field_name(&literal.get_string()?)?)?;
        Some(Narrowing { when_true: vec![(target.clone(), intersect(ty, set))], when_false: vec![(target, ty.without(set))] })
    }
}
