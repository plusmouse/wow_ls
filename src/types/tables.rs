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

//! Table constructors and indexing by computed keys

use super::*;

/// A key a table is indexed with
pub(super) enum Key {
    /// A string literal that could be a field name: `t["a"]` is `t.a`. With
    /// the literal's token, to record the field's use at
    Name(String, Option<SyntaxToken>),
    Other(Type),
}

impl<'a> Walker<'a> {
    /// Visits the variable reads in `a.b`, `a[k]`, `a:m(x).y` and similar: the
    /// base, index expressions and call arguments, but not field names.
    /// Expressions nest to the left (Identifier[Identifier[a], Dot, Name b]); function
    /// definition names are flat (Identifier[Name a, Dot, Name b, Colon, Name c])
    pub(super) fn identifier_parts(&mut self, node: &SyntaxNode) {
        let mut first = true;
        let mut in_brackets = false;
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Token(t) => match t.kind() {
                    SyntaxKind::Name if first => { self.read(&t); }
                    SyntaxKind::LeftSquareBracket => in_brackets = true,
                    SyntaxKind::RightSquareBracket => in_brackets = false,
                    _ => {}
                },
                NodeOrToken::Node(n) => {
                    if n.kind() == SyntaxKind::ArgumentList {
                        for argument in n.children().filter_map(Expression::cast) {
                            self.expression(&argument);
                        }
                    } else if first || in_brackets || n.kind() != SyntaxKind::Identifier {
                        if let Some(e) = Expression::cast(n) {
                            self.expression(&e);
                        }
                    }
                }
            }
            first = false;
        }
    }

    /// A new table, with the types of its `name = value` fields recorded
    pub(super) fn table_constructor(&mut self, node: &SyntaxNode) -> Type {
        let mut at = self.location(node.text_range().start());
        if let Some(outer) = self.element_of.take().filter(|&o| u32::from(o) < ELEMENT - 2) {
            at.slot = ELEMENT | u32::from(outer);
        }
        let table = Type::table(at);
        let fields: Vec<SyntaxNode> = node.children().filter(|n| n.kind() == SyntaxKind::Field).collect();
        let last_field = fields.last().cloned();
        for field in fields {
            // `{ x }` is an array item, whose identifier is a value, not a key
            let keyed = field.children_with_tokens().any(|c| c.kind() == SyntaxKind::Assign);
            // A last item of `...` or a call is every value it gives
            if !keyed && Some(&field) == last_field.as_ref() {
                if let Some(e) = field.children().find_map(Expression::cast).filter(|e| matches!(e, Expression::VarArgs(_) | Expression::FunctionCall(_))) {
                    let values = self.expression_values(std::slice::from_ref(&e));
                    let all = values.values.iter().fold(values.rest, |all, &v| all | v);
                    self.entry_write(table, Type::of(TypeSet::NUMBER), all);
                    continue
                }
            }
            let mut in_brackets = false;
            let mut after_assign = false;
            let mut name = None;
            let mut key = None;
            for child in field.children_with_tokens() {
                match child {
                    NodeOrToken::Token(t) => match t.kind() {
                        SyntaxKind::LeftSquareBracket => in_brackets = true,
                        SyntaxKind::RightSquareBracket => in_brackets = false,
                        SyntaxKind::Assign => after_assign = true,
                        _ => {}
                    },
                    // `{name = value}`: the name is a key, not a variable
                    NodeOrToken::Node(n) if n.kind() == SyntaxKind::Identifier && keyed && !in_brackets && !after_assign => {
                        name = Identifier::cast(n).and_then(|i| i.simple_name());
                    }
                    NodeOrToken::Node(n) => {
                        let Some(e) = Expression::cast(n) else { continue };
                        // `{[key] = value}`
                        if in_brackets {
                            key = Some(self.index_key(&e));
                            continue
                        }
                        // A table written directly in this one is one of its elements
                        if let Expression::TableConstructor(_) = e {
                            self.element_of = Some(node.text_range().start());
                        }
                        let value = self.expression(&e);
                        match (keyed, &name, key.take()) {
                            (false, ..) => self.entry_write(table, Type::of(TypeSet::NUMBER), value),
                            (true, Some(name), _) => {
                                self.field_write(table, name.text(), value);
                                self.note_loop_write(table, name.text(), Some(&e));
                                self.field_reference(&name.parent().unwrap_or_else(|| field.clone()), name, table, value, true);
                            }
                            (true, None, Some(Key::Name(name, token))) => {
                                self.field_write(table, &name, value);
                                if let Some(token) = token {
                                    self.field_reference(&field, &token, table, value, true);
                                }
                            }
                            (true, None, Some(Key::Other(key))) => self.entry_write(table, key, value),
                            (true, None, None) => {}
                        }
                    }
                }
            }
        }
        table
    }

    /// The key in `t[key]`: a string literal that could be a field name is
    /// that field, and anything else is read for its type
    pub(super) fn index_key(&mut self, key: &Expression) -> Key {
        if let Expression::Literal(literal) = key {
            if let Some(name) = literal.get_string().as_deref().and_then(string_field_name) {
                let token = literal.syntax().descendants_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == SyntaxKind::String);
                return Key::Name(name.to_string(), token)
            }
        }
        Key::Other(self.expression(key))
    }

    /// `t[key] = value` (or an array item, or `table.insert`) for a table made
    /// in the files. Storing nil removes, so adds nothing
    pub(super) fn entry_write(&mut self, base: Type, key: Type, value: Type) {
        if key.set.contains(TypeSet::STRING) {
            self.open(base);
        }
        let value = value.without(TypeSet::NIL);
        if value.set == TypeSet::NEVER {
            return
        }
        // Whichever table it is gets it
        for at in base.named.identities() {
            if let Some(value) = self.stored_in(at, value) {
                self.apply(Write::Entry(at, key.without(TypeSet::NIL), value));
            }
        }
    }

    /// What of `value` a write into table `at` stores: all of it, unless `at`
    /// is one the game saves between sessions and `value` may be a table of
    /// unknown shape. That's a table the addon stores there from outside it,
    /// like a profile deserialized from an import string, which isn't taken
    /// as what the table holds: what saved data comes back as is what the
    /// addon writes into it, and a table of any shape would hide all of that.
    /// Such a table is one of no known identity, or only a variable's
    /// stand-in for one (as a parameter holding an import has). The rest of
    /// the value is kept, so more stored never leaves less, and walks settle.
    /// None when nothing's left
    pub(super) fn stored_in(&self, at: TableId, value: Type) -> Option<Type> {
        if !value.set.contains(TypeSet::TABLE) {
            return Some(value)
        }
        let identities = value.named.identities();
        let known: Vec<TableId> = identities.iter().copied().filter(|table| table.slot & PSEUDO == 0).collect();
        if !known.is_empty() && known.len() == identities.len() {
            return Some(value)
        }
        self.note(Read::Saved(at));
        if !self.prior.is_some_and(|prior| prior.saved_tables.contains(&at)) {
            return Some(value)
        }
        // Its tables made in the files, without the stand-ins
        let tables = known.into_iter().map(Type::table).reduce(Type::union);
        let rest = value.without(TypeSet::TABLE);
        match (rest.set != TypeSet::NEVER, tables) {
            (true, Some(tables)) => Some(rest | tables),
            (true, None) => Some(rest),
            (false, tables) => tables,
        }
    }

    /// The type of `t[key]`, from what the previous walk saw stored. It may
    /// not be there, so it can be nil
    pub(super) fn index_read(&self, base: Type, key: Type) -> Type {
        // Nothing yet on the first walk, even for a table that isn't known
        // yet, as it may be one read from a field, nor from what nothing's
        // known of yet
        let Some(prior) = self.prior else { return Type::default() };
        if base.set == TypeSet::NEVER {
            return Type::default()
        }
        let tables = base.named.identities();
        if tables.is_empty() {
            return Type::UNKNOWN
        }
        let mut found: Option<Type> = None;
        // Whichever table it is: what any of them holds
        for at in tables {
            self.note(Read::Entries(at));
            self.note(Read::Fields(at));
            if let Some(entries) = self.entries_of(prior, at) {
                *found.get_or_insert_default() |= entries.values;
            }
            // A string key could be any of the named fields
            if key.set.contains(TypeSet::STRING) {
                for value in self.field_values_of(prior, at) {
                    *found.get_or_insert_default() |= value;
                }
            }
        }
        // Nothing seen stored in it: nothing yet, and once walks settle,
        // unknown rather than nil (see `settled`)
        match found {
            Some(values) if values.set != TypeSet::NEVER => values | Type::NIL,
            _ if self.settled => Type::UNKNOWN,
            _ => {
                self.provisional.set(true);
                Type::default()
            }
        }
    }

    /// What any of `tables` holds beyond its named fields (see `entries_of`)
    pub(super) fn entries_of_any(&self, prior: &Analysis, tables: &[TableId]) -> Option<Entries> {
        tables.iter().filter_map(|&at| self.entries_of(prior, at))
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })
    }

    /// Every key and value any of `tables` holds (see `contents_of`)
    pub(super) fn contents_of_any(&self, prior: &Analysis, tables: &[TableId]) -> Option<Entries> {
        tables.iter().filter_map(|&at| self.contents_of(prior, at))
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })
    }

    /// What a table holds beyond its named fields: here, and from a dependency
    pub(super) fn entries_of(&self, prior: &Analysis, at: TableId) -> Option<Entries> {
        self.entries_within(prior, at, 0, false)
    }

    /// With `known`, only what's seen put in tables it knows: not that a
    /// variable's stand-in for a table it doesn't know may hold anything, as
    /// a field read through it falls back on fields of that name elsewhere
    pub(super) fn entries_within(&self, prior: &Analysis, at: TableId, depth: usize, known: bool) -> Option<Entries> {
        // What it holds is read, whether it's the table asked about or one
        // reached through it (a mixin, or a stand-in's member)
        self.note(Read::Entries(at));
        // A stand-in: what any table it stands for holds, and what's put
        // through it
        if is_stand_in(at) && depth <= 16 {
            self.note(Read::StandIn(at));
            let all: Vec<Entries> = self.stand_in_members(at).into_iter()
                .filter_map(|member| self.entries_within(prior, member, depth + 1, known))
                .chain(prior.entries.get(&at).copied())
                .collect();
            let keys = union_all(all.iter().map(|e| e.keys))?;
            let values = union_all(all.iter().map(|e| e.values))?;
            return Some(Entries {
                keys: self.read_through(Place::EntryKeys(at), keys),
                values: self.read_through(Place::Entries(at), values),
            })
        }
        // A variable's stand-in for a table it doesn't know: what it's seen
        // given is only some of what the table may hold. Nothing it knows,
        // with `known`, whether reached as a stand-in's member or a mixin
        if at.slot & PSEUDO != 0 {
            if known {
                return None
            }
            let anything = Type::UNKNOWN.without(TypeSet::NIL);
            return Some(Entries { keys: anything, values: anything })
        }
        let own = match (prior.entries.get(&at), self.layer.entries.get(&at)) {
            (Some(a), Some(b)) => Some(Entries { keys: a.keys | b.keys, values: a.values | b.values }),
            (a, b) => a.or(b).copied(),
        };
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return own
        }
        // What its mixins hold too, as `Mixin` copies every key: a new table
        // a call to a function returns has the one it made as its mixin
        self.note(Read::Mixins(at));
        let mixins = prior.mixins.get(&at).or_else(|| self.layer.mixins.get(&at));
        mixins.into_iter().flatten()
            .filter_map(|mixin| mixin.named.identity())
            .filter_map(|mixin| self.entries_within(prior, mixin, depth + 1, known))
            .chain(own)
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })
    }

    /// The types of a table's named fields: here, and from a dependency
    pub(super) fn field_values_of(&self, prior: &Analysis, at: TableId) -> Vec<Type> {
        prior.fields.get(&at).into_iter().chain(self.layer.fields.get(&at)).flat_map(|fields| fields.values().copied()).collect()
    }

    /// Every key and value a table holds, as `pairs` gives them, here and
    /// from a dependency
    pub(super) fn contents_of(&self, prior: &Analysis, at: TableId) -> Option<Entries> {
        let mut contents = self.entries_of(prior, at).unwrap_or_default();
        for value in self.field_values_of(prior, at) {
            contents.keys |= Type::of(TypeSet::STRING);
            contents.values |= value.without(TypeSet::NIL);
        }
        (contents.values.set != TypeSet::NEVER).then_some(contents)
    }
}
