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

//! Calls of the Lua standard library that need more than their definitions

use super::*;

impl<'a> Walker<'a> {
    /// `select(n, ...)` with a literal `n`, which gives the arguments from the
    /// `n`th on, or `select("#", ...)`, a count. This is how many addons get
    /// their namespace: `local ns = select(2, ...)`
    /// The preset function at a path like `table.insert`, as it's typed
    pub(super) fn builtin(&self, path: &str) -> Option<Type> {
        match self.builtins.iter().find(|(known, _)| *known == path) {
            Some(&(_, ty)) => ty,
            None => self.definitions.at_path(path),
        }
    }

    /// Standard library functions whose returns depend on their arguments
    pub(super) fn library_returns(&mut self, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let is = |path: &str| self.builtin(path) == Some(callee);
        let one = |ty: Type| Some(Values { values: vec![ty], rest: Type::NIL });
        // Gives back its arguments, the first sure to be true, or raises an error
        if is("assert") {
            let mut values = arguments.clone();
            match values.values.first_mut() {
                Some(first) => *first = first.truthy(),
                None => return None,
            }
            return Some(values)
        }
        // Whether `f` ran without error, then its returns, or the error
        let (pcall, xpcall) = (is("pcall"), is("xpcall"));
        if pcall || xpcall {
            let mut returns: Option<Values> = None;
            for function in arguments.get(0).function.alternatives() {
                let known = self.known_function(function).map(|f| f.returns.clone())?;
                let one = match function {
                    FunctionRef::One(id) => self.returns_of(id),
                    _ => known,
                };
                returns = Some(returns.map_or_else(|| one.clone(), |r| r.union(&one)));
            }
            let returns = returns?;
            // The error is anything `error` was given, though nearly always a message
            let error = match xpcall {
                true => self.known_function(arguments.get(1).function).map_or(Type::UNKNOWN, |f| f.returns.first()),
                false => Type::of(TypeSet::STRING),
            };
            let mut values = vec![Type::of(TypeSet::BOOLEAN), returns.get(0) | error];
            values.extend(returns.values.iter().skip(1).map(|&v| v | Type::NIL));
            return Some(Values { values, rest: returns.rest | Type::NIL })
        }
        let table = arguments.get(0).named.identity();
        // Whichever table it is, of several
        let tables = arguments.get(0).named.identities();
        for &at in &tables {
            self.note(Read::Entries(at));
            self.note(Read::Fields(at));
        }
        // What a table's array holds, any of which may be missing. For a
        // table made in the files that nothing's been seen stored in, nothing
        // yet (see `settled`)
        let array = |prior: &Analysis| match self.entries_of_any(prior, &tables).filter(|e| e.keys.set.contains(TypeSet::NUMBER)) {
            Some(entries) => Some(entries.values | Type::NIL),
            None => (!tables.is_empty() && !self.settled).then(|| {
                self.provisional.set(true);
                Type::default()
            }),
        };
        // On the first walk, nothing yet, as a field read gives, rather than
        // the documented unknown, which a table filled from itself (as a
        // pool's list of free objects is) would never be rid of. Even when
        // the table isn't known: it may be one read from a field, which only
        // the previous walk knows
        let unwalked = self.prior.is_none();
        if is("unpack") {
            if unwalked {
                return Some(Values { values: Vec::new(), rest: Type::default() })
            }
            return Some(Values { values: Vec::new(), rest: array(self.prior?)? })
        }
        if is("table.remove") || is("tremove") {
            if unwalked {
                return one(Type::default())
            }
            return one(array(self.prior?)?)
        }
        // A field without the metatable: the table's own
        if is("rawget") {
            let key = expressions.get(1)?;
            return match key {
                Expression::Literal(l) if l.get_string().as_deref().and_then(string_field_name).is_some() => {
                    let name = l.get_string()?;
                    let name = string_field_name(&name)?;
                    let here = self.prior?.fields.get(&table?).and_then(|fields| fields.get(name)).copied();
                    let layered = self.layer.fields.get(&table?).and_then(|fields| fields.get(name)).copied();
                    one(match (here, layered) {
                        (Some(a), Some(b)) => a | b,
                        (a, b) => a.or(b)?,
                    })
                }
                _ => one(self.index_read(arguments.get(0), arguments.get(1))),
            }
        }
        // A key and value from the table, or nil when it's empty
        if is("next") {
            if unwalked {
                return Some(Values { values: vec![Type::default(), Type::default()], rest: Type::NIL })
            }
            // Nothing yet from a table made in the files that nothing's been
            // seen stored in (see `settled`)
            let contents = match self.contents_of_any(self.prior?, &tables) {
                Some(contents) => contents,
                None if !tables.is_empty() && !self.settled => {
                    self.provisional.set(true);
                    Entries { keys: Type::default(), values: Type::default() }
                }
                None => return None,
            };
            return Some(Values { values: vec![contents.keys | Type::NIL, contents.values | Type::NIL], rest: Type::NIL })
        }
        // Fields set this way can't be named, or come from where the
        // analysis doesn't look
        if is("rawset") {
            self.open(arguments.get(0));
            return None
        }
        if is("setmetatable") {
            let (table, metatable) = (arguments.get(0), arguments.get(1));
            self.open(table);
            let at = table.named.identity()?;
            // One that isn't known is kept too, so the table isn't taken to have
            // none
            if metatable.named != NamedRef::None {
                self.apply(Write::Metatable(at, metatable));
            }
            return one(table)
        }
        if is("getmetatable") {
            let at = arguments.get(0).named.identity()?;
            self.note(Read::Metatable(at));
            let here = self.prior?.metatables.get(&at).copied();
            return one(match (here, self.layer.metatables.get(&at).copied()) {
                (Some(a), Some(b)) => a | b,
                (a, b) => a.or(b)?,
            })
        }
        None
    }

    pub(super) fn select(&self, callee: Type, arguments: &[Expression], argument_types: &Values) -> Option<Values> {
        if self.definitions.global("select") != Some(callee) {
            return None
        }
        let Some(Expression::Literal(index)) = arguments.first() else { return None };
        if index.get_string().is_some_and(|s| s.trim().trim_matches(['"', '\'']) == "#") {
            return Some(Values { values: vec![Type::of(TypeSet::NUMBER)], rest: Type::NIL })
        }
        let index: usize = index.get_number()?.trim().parse().ok().filter(|&n| n >= 1)?;
        // The index itself is the first argument
        Some(Values {
            values: argument_types.values.iter().skip(index).copied().collect(),
            rest: argument_types.rest,
        })
    }
}

impl<'a> Walker<'a> {
    /// `table.insert` (or `tinsert`) adds its last argument to the table, and
    /// `pairs` and `ipairs` note what their loop gives
    pub(super) fn table_library_call(&mut self, call: &FunctionCall, callee: Type, count: usize, arguments: &Values) {
        let is = |path: &str| self.builtin(path) == Some(callee);
        // Every capture (or the whole match) is a string
        if is("string.gmatch") || is("gmatch") {
            let strings = Values { values: Vec::new(), rest: Type::of(TypeSet::STRING) };
            self.iterated.insert(self.call_location(call), strings);
            return
        }
        if (is("table.insert") || is("tinsert")) && count >= 2 {
            self.entry_write(arguments.get(0), Type::of(TypeSet::NUMBER), arguments.get(count - 1));
            return
        }
        let (ipairs, pairs) = (is("ipairs"), is("pairs"));
        if !ipairs && !pairs {
            return
        }
        // On the first walk, nothing yet: what the loop gives grows from
        // there, as a field read's does
        let Some(prior) = self.prior else {
            let nothing = Values { values: vec![Type::default(), Type::default()], rest: Type::NIL };
            self.iterated.insert(self.call_location(call), nothing);
            return
        };
        // Whichever table it is, of several
        let tables = arguments.get(0).named.identities();
        for &at in &tables {
            self.note(Read::Entries(at));
            self.note(Read::Fields(at));
        }
        let contents = if ipairs {
            // Only the entries under number keys are visited
            self.entries_of_any(prior, &tables)
                .filter(|e| e.keys.set.contains(TypeSet::NUMBER))
                .map(|e| (Type::of(TypeSet::NUMBER), e.values))
        } else {
            self.contents_of_any(prior, &tables).map(|e| (e.keys, e.values))
        };
        // Over a table made in the files that nothing's been seen stored in,
        // nothing yet (see `settled`)
        let contents = contents.or_else(|| (!tables.is_empty() && !self.settled).then(|| {
            self.provisional.set(true);
            (Type::default(), Type::default())
        }));
        if let Some((key, value)) = contents {
            let values = Values { values: vec![key, value], rest: Type::NIL };
            self.iterated.insert(self.call_location(call), values);
        }
    }
}
