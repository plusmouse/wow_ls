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

//! Checking that what's called, indexed or operated on can be

use super::*;

/// A binary expression as the tree its precedences make
pub(super) enum Operation {
    Operand(Expression),
    Binary(Operator, Box<Operation>, Box<Operation>),
}

impl Operation {
    /// The expressions it's made of
    pub(super) fn syntax_nodes(&self) -> Vec<SyntaxNode> {
        match self {
            Operation::Operand(e) => vec![e.syntax().clone()],
            Operation::Binary(_, left, right) => {
                let mut nodes = left.syntax_nodes();
                nodes.extend(right.syntax_nodes());
                nodes
            }
        }
    }

    /// The code it covers
    pub(super) fn range(&self) -> TextRange {
        match self {
            Operation::Operand(e) => e.syntax().text_range(),
            Operation::Binary(_, left, right) => left.range().cover(right.range()),
        }
    }
}

/// Whether calling a value of type `ty` raises an error whatever it is: it's
/// a number, string or boolean (or nil), and nothing that can be called (a
/// function, or a table or userdata, which may have `__call`). Nil alone isn't
/// sure enough, as a field or variable not seen set reads as nil
pub(super) fn uncallable(ty: Type) -> bool {
    ty.set.intersection(TypeSet::FUNCTION | OBJECTS) == TypeSet::NEVER
        && ty.set.intersection(TypeSet::BOOLEAN | TypeSet::NUMBER | TypeSet::STRING) != TypeSet::NEVER
}

/// The kinds of Lua value `set` may be, as Lua names them: `boolean or function`
pub(super) fn kind_names(set: TypeSet) -> String {
    let kinds = [
        (TypeSet::NUMBER, "number"), (TypeSet::STRING, "string"), (TypeSet::BOOLEAN, "boolean"),
        (TypeSet::FUNCTION, "function"), (TypeSet::TABLE, "table"), (TypeSet::OTHER, "userdata"),
    ];
    kinds.iter()
        .filter(|(kind, _)| set.intersection(*kind) != TypeSet::NEVER)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The one kind of Lua value `set` is, if it's only one: number, string,
/// boolean, function, table, or userdata
pub(super) fn single_kind(set: TypeSet) -> Option<TypeSet> {
    if set == TypeSet::NEVER {
        return None
    }
    [TypeSet::NUMBER, TypeSet::STRING, TypeSet::BOOLEAN, TypeSet::FUNCTION, TypeSet::TABLE, TypeSet::OTHER]
        .into_iter()
        .find(|&kind| kind.contains(set))
}

/// Precedence climbing: operators bind tighter the higher their precedence.
/// Associativity doesn't change the result type for any operator, nor what a
/// condition proves, so everything is treated as left associative
pub(super) fn operation_tree(operands: &mut std::vec::IntoIter<Expression>, operators: &[Operator], position: &mut usize, min_precedence: u8) -> Option<Operation> {
    let mut left = Operation::Operand(operands.next()?);
    while let Some(&op) = operators.get(*position) {
        let p = precedence(op);
        if p <= min_precedence {
            break;
        }
        *position += 1;
        let right = operation_tree(operands, operators, position, p)?;
        left = Operation::Binary(op, Box::new(left), Box::new(right));
    }
    Some(left)
}

impl<'a> Walker<'a> {
    /// Reports `receiver:method()` when the receiver is one WoW type whose
    /// methods are known, and none of them (nor any function the addon puts in
    /// a table under that name, as a mixin might) is called `method`
    pub(super) fn check_method(&mut self, receiver: Type, method: &SyntaxToken) {
        // Only the previous walk knows every function the files define
        let Some(prior) = self.prior else { return };
        if receiver.set.without(OBJECTS | TypeSet::NIL) != TypeSet::NEVER {
            return
        }
        if let NamedRef::Table(table) = receiver.named {
            return self.check_table_method(prior, table, method)
        }
        // Any of several objects needs the method on all of them, so it's
        // reported for those whose methods are known and don't include it
        let objects = receiver.named.alternatives();
        let name = method.text();
        let mut lacking: Vec<String> = Vec::new();
        for object in &objects {
            let id = match *object {
                NamedRef::One(id) | NamedRef::Instance { named: id, extended: false, .. } => id,
                // May have it
                _ => return,
            };
            let Some(named) = self.definitions.named_type(id) else { return };
            // A data table's members are all known too, and none are methods,
            // unless it's given ones that can't be named
            if !named.has_methods && (!self.definitions.has_fixed_fields(id) || self.is_opened(prior, id)) {
                return
            }
            if !self.lua_mixins_known(prior, id) {
                return
            }
            self.note(Read::FunctionField(name_key(name)));
            if prior.function_fields.contains(name) || self.layer.function_fields.contains(name) {
                return
            }
            // Its own fields and mixins', for an object made in the files,
            // which may have more if a mixin may
            if object.identity().is_some_and(|at| self.may_gain_fields(prior, at, 0)) {
                return
            }
            let own = object.identity().and_then(|at| self.own_field(prior, at, name, 0).or_else(|| match object {
                NamedRef::Instance { .. } => self.copied_into_field(prior, at, name),
                _ => None,
            }));
            if own.is_none() && self.definitions.field(Type { named: *object, ..receiver }, name).is_none() && !lacking.contains(&named.name) {
                lacking.push(named.name.clone());
            }
        }
        if lacking.is_empty() {
            return
        }
        let all: Vec<String> = objects.iter()
            .filter_map(|object| self.definitions.named_type(object.named_id()?).map(|n| n.name.clone()))
            .fold(Vec::new(), |mut names, name| { if !names.contains(&name) { names.push(name) } names });
        let message = if lacking.len() == all.len() {
            match lacking.as_slice() {
                [one] => format!("{one} has no method `{name}`"),
                [a, b] => format!("Neither {a} nor {b} has a method `{name}`"),
                _ => format!("None of {} has a method `{name}`", lacking.join(", ")),
            }
        } else {
            match lacking.as_slice() {
                [one] => format!("This may be a {one}, which has no method `{name}`"),
                _ => format!("This may be a {}, which have no method `{name}`", lacking.join(" or ")),
            }
        };
        self.diagnostics.push(Diagnostic { file: self.file, range: method.text_range(), message });
    }

    /// `ty`, if it's a data table (or may be one), was given fields that can't
    /// be named, so it may have any
    pub(super) fn open(&mut self, ty: Type) {
        for object in ty.named.alternatives() {
            if let NamedRef::One(id) | NamedRef::Extended(id) = object
                && self.definitions.has_fixed_fields(id) {
                self.apply(Write::Opened(id));
            }
        }
    }

    /// Reports calling field `name` of a table (`t.name()` or `t:name()`) when
    /// it's known to hold a value that can't be called: a number, string or
    /// boolean, and nothing that can be (a function, or a table or userdata,
    /// which may have `__call`). Only for a field whose type is sure: one the
    /// definitions give, or one a table made in the files is given, not the
    /// guess from fields of that name on other tables. Nil alone isn't
    /// reported, as a field not yet seen set reads as nothing
    pub(super) fn check_called_field(&mut self, base: NamedRef, text: &str, range: TextRange, types: Type) {
        let Some(prior) = self.prior else { return };
        if !uncallable(types) {
            return
        }
        let sure = match base {
            NamedRef::One(id) | NamedRef::Extended(id) => {
                self.definitions.field(Type::named(id, TypeSet::TABLE), text).is_some()
                    && !(self.definitions.has_fixed_fields(id) && self.is_opened(prior, id))
            }
            NamedRef::Table(at) | NamedRef::Instance { at, .. } => self.own_field(prior, at, text, 0).is_some(),
            _ => false,
        };
        if !sure {
            return
        }
        // Some table given a function of that name might be this one
        self.note(Read::FunctionField(name_key(text)));
        if prior.function_fields.contains(text) || self.layer.function_fields.contains(text) {
            return
        }
        let message = format!("`{text}` is {}, not a function", self.definitions.type_text(types));
        self.diagnostics.push(Diagnostic { file: self.file, range, message });
    }

    /// Reports calling a variable (`n()`, or `_G.n()`) whose value here can
    /// only be one that can't be called, as for `check_called_field`. A
    /// parameter has a type only when every call to its function is seen (it's
    /// unknown once the function escapes), and a global's is every value the
    /// files give it. SavedVariables can't hold functions, and a global the
    /// files don't set has its preset type
    pub(super) fn check_called_variable(&mut self, types: Type) {
        if self.prior.is_none() || !uncallable(types) {
            return
        }
        let Some(reference) = self.references.last() else { return };
        let binding = &self.bindings[reference.binding];
        let message = format!("`{}` is {}, not a function", binding.name, self.definitions.type_text(types));
        let range = reference.range;
        self.diagnostics.push(Diagnostic { file: self.file, range, message });
    }

    /// Reports `operand` of unary `op` (or one side of a binary one) when it can
    /// only be a value `op` raises an error on. Nil is set aside: every other
    /// value it may be already raises one, and a value that can only be nil
    /// isn't sure enough to report, as a field not seen set reads as nil
    pub(super) fn check_operand(&mut self, op: Operator, operand: Type, range: TextRange) {
        if self.prior.is_none() {
            return
        }
        // What raises an error, beyond nil. Arithmetic and `..` take numbers
        // and strings (a string as a number if it is one), and tables and
        // userdata may have metamethods. `#` takes strings and tables
        let fails = match op {
            Operator::Add | Operator::Subtract | Operator::Multiply | Operator::Divide | Operator::Modulo
                | Operator::Hat | Operator::Concatenate => TypeSet::BOOLEAN | TypeSet::FUNCTION,
            Operator::ArrayLength => TypeSet::NUMBER | TypeSet::BOOLEAN | TypeSet::FUNCTION,
            _ => return,
        };
        let set = operand.set.without(TypeSet::NIL);
        if set == TypeSet::NEVER || !fails.contains(set) {
            return
        }
        let what = kind_names(set);
        let message = match op {
            Operator::Concatenate => format!("Can't concatenate a {what}"),
            Operator::ArrayLength => format!("Can't get the length of a {what}"),
            _ => format!("Can't do arithmetic on a {what}"),
        };
        self.diagnostics.push(Diagnostic { file: self.file, range, message });
    }

    /// Reports binary `op` on `left` and `right` when it raises an error
    /// whatever they are (nil aside, as for `check_operand`): arithmetic or
    /// `..` on a side it can't take, or an order comparison of two kinds of
    /// value, which Lua 5.1 never converts between (even with `__lt`, which
    /// needs both to be the same kind), or of booleans or functions
    pub(super) fn check_operands(&mut self, op: Operator, left: Type, right: Type, range: TextRange) {
        if self.prior.is_none() {
            return
        }
        if !matches!(op, Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals | Operator::GreaterThanOrEquals) {
            // One report for the expression, for the first side that can't be used
            let before = self.diagnostics.len();
            self.check_operand(op, left, range);
            if self.diagnostics.len() == before {
                self.check_operand(op, right, range);
            }
            return
        }
        let (Some(a), Some(b)) = (single_kind(left.set.without(TypeSet::NIL)), single_kind(right.set.without(TypeSet::NIL))) else { return };
        let unordered = TypeSet::BOOLEAN | TypeSet::FUNCTION;
        if a == b && !unordered.contains(a) {
            return
        }
        let message = format!("Can't compare a {} with a {}", kind_names(a), kind_names(b));
        self.diagnostics.push(Diagnostic { file: self.file, range, message });
    }

    /// Whether data type `id` was given fields that can't be named (see `open`)
    pub(super) fn is_opened(&self, prior: &Analysis, id: NamedId) -> bool {
        self.note(Read::Opened(id));
        prior.opened_types.contains(&id) || self.layer.opened_types.contains(&id)
    }

    /// Reports reading field `name` (at `token`) of a data table whose fields
    /// the definitions give all of (`values`), when it has none of that name
    /// and no code in the files gives one of that name to a table like it.
    /// The addon may add its own fields, as the table is its to change
    pub(super) fn check_field(&mut self, base: Type, token: &SyntaxToken, name: &str) {
        // Only the previous walk knows every field the files assign
        let Some(prior) = self.prior else { return };
        if base.set.without(TypeSet::TABLE | TypeSet::NIL) != TypeSet::NEVER {
            return
        }
        let NamedRef::One(id) = base.named else { return };
        if !self.definitions.has_fixed_fields(id) || self.definitions.field(base, name).is_some() {
            return
        }
        if self.is_opened(prior, id) {
            return
        }
        // Writes to it, as to any table whose identity isn't known, are by name
        self.note(Read::AnyTableField(name_key(name)));
        if prior.any_table_fields.contains_key(name) || self.layer.any_table_fields.contains_key(name) {
            return
        }
        let Some(named) = self.definitions.named_type(id) else { return };
        let message = format!("{} has no field `{name}`", named.name);
        self.diagnostics.push(Diagnostic { file: self.file, range: token.text_range(), message });
    }

    /// Whether every Lua mixin type `id` has (see `Definitions::lua_mixins`)
    /// is a table made in the files or those they depend on, whose fields
    /// the analysis sees
    pub(super) fn lua_mixins_known(&self, prior: &Analysis, id: NamedId) -> bool {
        let names = self.definitions.lua_mixins(id);
        if names.is_empty() {
            return true
        }
        self.note(Read::LuaMixins);
        names.iter().all(|name| match prior.lua_mixins.get(name).and_then(|ty| ty.named.identity()) {
            Some(at) => !self.may_gain_fields(prior, at, 0),
            None => false,
        })
    }

    /// An object of type `named` made at `at`, in `parent`, has its Lua
    /// mixins' fields, as a frame made from a template with `mixin="..."`
    /// does. `self:GetParent()` in their methods may be `parent`
    pub(super) fn add_lua_mixins(&mut self, at: TableId, named: NamedId, parent: Type) {
        let names = self.definitions.lua_mixins(named);
        if names.is_empty() {
            return
        }
        let Some(prior) = self.prior else { return };
        self.note(Read::LuaMixins);
        let mixins: Vec<Type> = names.iter().rev().filter_map(|name| prior.lua_mixins.get(name).copied()).collect();
        for mixin in mixins {
            self.apply(Write::MixinAdded(at, mixin));
            if let Some(mixin) = mixin.named.identity() {
                self.apply(Write::Parent(mixin, parent));
            }
        }
    }

    /// Reports `t:method()` for a table made in the files when no table in
    /// them (or the addons they depend on) is given a function called
    /// `method`, no WoW object has a method of that name (the table may be a
    /// mixin for one, in a way the analysis can't follow), the table has no
    /// field of that name, and nothing else the analysis can't follow could
    /// give it one
    pub(super) fn check_table_method(&mut self, prior: &Analysis, table: TableId, method: &SyntaxToken) {
        // A variable's stand-in for a table it doesn't know
        if table.slot & PSEUDO != 0 {
            return
        }
        let name = method.text();
        self.note(Read::FunctionField(name_key(name)));
        if prior.function_fields.contains(name) || self.layer.function_fields.contains(name) {
            return
        }
        if self.definitions.is_object_method(name) {
            return
        }
        if self.own_field(prior, table, name, 0).is_some() || self.may_gain_fields(prior, table, 0) {
            return
        }
        let message = format!("No function `{}` is defined for this table", name);
        self.diagnostics.push(Diagnostic { file: self.file, range: method.text_range(), message });
    }

    /// Whether a table made in the files could have fields the analysis
    /// doesn't see: from a mixin or `__index` that isn't a table made in
    /// them, or functions stored under keys that aren't known names
    pub(super) fn may_gain_fields(&self, prior: &Analysis, table: TableId, depth: usize) -> bool {
        // Deep enough for any real chain, and stops a loop
        if depth > 16 || table.slot & PSEUDO != 0 {
            return true
        }
        self.note(Read::Entries(table));
        let entries = [prior.entries.get(&table), self.layer.entries.get(&table)];
        if entries.into_iter().flatten().any(|e| e.keys.set.contains(TypeSet::STRING) && e.values.set.contains(TypeSet::FUNCTION)) {
            return true
        }
        self.note(Read::Mixins(table));
        let mixins = [prior.mixins.get(&table), self.layer.mixins.get(&table)];
        let open_mixin = mixins.into_iter().flatten().flatten().any(|mixin| match mixin.named.identity() {
            Some(mixin) => self.may_gain_fields(prior, mixin, depth + 1),
            // One the definitions give is known, unless it's open
            None => self.definitions.may_have_more(*mixin),
        });
        if open_mixin {
            return true
        }
        self.note(Read::Metatable(table));
        let Some(metatable) = prior.metatables.get(&table).or_else(|| self.layer.metatables.get(&table)) else { return false };
        let Some(metatable) = metatable.named.identity() else { return true };
        match self.own_field(prior, metatable, "__index", depth + 1) {
            None => self.may_gain_fields(prior, metatable, depth + 1),
            Some(index) => match index.named.identity() {
                Some(fallback) if index.set.without(OBJECTS | TypeSet::NIL) == TypeSet::NEVER => self.may_gain_fields(prior, fallback, depth + 1),
                // A function, or something not known
                _ => true,
            },
        }
    }
}
