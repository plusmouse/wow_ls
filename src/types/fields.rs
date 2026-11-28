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

//! Reading and writing table fields, and the writes a body records

use super::*;

impl<'a> Walker<'a> {
    /// The type of field `name` of a value of type `base`
    pub(super) fn field_read(&self, base: Type, name: &str) -> Type {
        // Nothing known of it yet, like a parameter no call is seen passing
        // yet: so nothing known of its field. Unknown would stay in what it
        // flows into, as for a call of nothing
        if base.set == TypeSet::NEVER {
            return Type::default()
        }
        // May be a string or something else, like an unknown value: a string
        // has the string library's fields whatever the rest has, so knowing
        // more of what it may be only adds to what's read. What can't be
        // indexed (a number, a boolean, a function, nil) adds nothing, as
        // indexing it fails. With a table, only what the string library
        // declares: the table's read already falls back on fields of that
        // name anywhere (as a string's does, for functions added to
        // `string`), and it mustn't hide what the table's own field holds
        // (`ns.Version` of an addon's namespace)
        let other = base.without(TypeSet::STRING | TypeSet::NIL);
        if base.set.contains(TypeSet::STRING) && other.set != TypeSet::NEVER {
            if !(other.set.contains(TypeSet::TABLE) || other.set.contains(TypeSet::OTHER)) {
                return self.field_read(Type::of(TypeSet::STRING), name)
            }
            let declared = self.definitions.string_library()
                .and_then(|library| self.definitions.field(library, name))
                .unwrap_or_default();
            return declared | self.field_read(other, name)
        }
        // Any of several objects: what each has, which is only known for a
        // field all of them have
        if let NamedRef::Several(_) = base.named {
            return base.named.alternatives().into_iter()
                .map(|object| self.field_read(Type { named: object, ..base }, name))
                .reduce(Type::union)
                .unwrap_or(Type::UNKNOWN)
        }
        // Any of several tables: what any of them has. Those without it don't
        // make it unknown, as a field reset from `{}` to a table of constants
        // has only the second's fields
        if let NamedRef::Tables(_) = base.named {
            if let Some(prior) = self.prior {
                let held = base.named.identities().into_iter()
                    .filter_map(|at| self.own_field(prior, at, name, 0))
                    .reduce(Type::union);
                if let Some(held) = held {
                    return held
                }
            }
            return base.named.identities().into_iter()
                .map(|at| self.field_read(Type { named: NamedRef::Table(at), ..base }, name))
                .reduce(Type::union)
                .unwrap_or(Type::UNKNOWN)
        }
        let base = self.definitions.member_source(base);
        if let Some(declared) = self.definitions.field(base, name) {
            // A value the definitions don't know the type of: what this table
            // was given, if anything
            if declared.set == TypeSet::UNKNOWN
                && let (Some(prior), Some(at)) = (self.prior, base.named.identity())
                && let Some(own) = self.own_field(prior, at, name, 0) {
                return own
            }
            return declared
        }
        // Writes can come later in the file, so only a previous walk knows them all
        let Some(prior) = self.prior else { return Type::default() };
        let own = match base.named {
            NamedRef::Table(id) => self.own_field(prior, id, name, 0),
            // In a mixin's methods, `self` is an object with the fields of
            // the tables the mixin was mixed into on its way to it
            // A parameter's stand-in for an object of a named type, which may
            // be any object of it: what's written through it, with what any
            // object's field of that name is given, as for the type itself
            NamedRef::Instance { at: id, named, .. } if id.slot & PSEUDO != 0 => {
                let own = self.own_field(prior, id, name, 0).or_else(|| self.lua_mixin_field(prior, named, name));
                self.note(Read::AnyTableField(name_key(name)));
                let any = prior.any_table_fields.get(name).copied().into_iter().chain(self.layer.any_table_fields.get(name).copied()).reduce(Type::union);
                match (own, any) {
                    (Some(own), Some(any)) => Some(own | Type { named: any.named.loosened(), ..any }.guessed()),
                    (own, None) => own,
                    (None, Some(_)) => None,
                }
            }
            NamedRef::Instance { at: id, .. } => self.own_field(prior, id, name, 0).or_else(|| self.copied_into_field(prior, id, name)),
            // A frame XML makes from a template has its Lua mixins' fields,
            // as one `CreateFrame` makes does
            NamedRef::One(id) | NamedRef::Extended(id) => self.lua_mixin_field(prior, id, name),
            NamedRef::Several(_) | NamedRef::Tables(_) => unreachable!("read from each object"),
            // Not a table we know, so nothing to go on
            NamedRef::None | NamedRef::Unknown => return Type::UNKNOWN,
        };
        // What the table's own code (or a mixin's) sets wins. Writes to tables of
        // unknown identity are only a fallback: one field name is reused by many
        // unrelated tables (`entry.texts.cooldown` and `pools.cooldown`), so
        // merging them in would lose what's known about this one
        // Not one of its named fields, but a table stored to under string
        // keys (`t[key] = v`) may hold it: what it holds, as `t[name]` reads.
        // Not a variable's stand-in for a table it doesn't know, which holds
        // anything, as fields of that name elsewhere say more
        let entry = match (own, base.named) {
            (None, NamedRef::Table(id)) if id.slot & PSEUDO == 0 => {
                self.note(Read::Entries(id));
                self.entries_within(prior, id, 0, true)
                    .filter(|entries| entries.keys.set.contains(TypeSet::STRING) && entries.values.set != TypeSet::NEVER)
                    .map(|entries| (entries.values | Type::NIL).guessed())
            }
            _ => None,
        };
        match own.or(entry) {
            Some(own) => own,
            None => {
                self.note(Read::AnyTableField(name_key(name)));
                let own = prior.any_table_fields.get(name).copied();
                let found = match (own, self.layer.any_table_fields.get(name).copied()) {
                    (Some(a), Some(b)) => a | b,
                    (Some(found), None) | (None, Some(found)) => found,
                    // Written nowhere seen: nothing yet until walks settle,
                    // as unknown would stay in what it flows round (see
                    // `Walker::settled`), then unknown, as it may be set
                    // where that can't be seen
                    (None, None) if !self.settled => {
                        self.provisional.set(true);
                        return Type::default()
                    }
                    (None, None) => Type::UNKNOWN,
                };
                // Some other table's field, perhaps, so not a type to report
                // missing methods by
                Type { named: found.named.loosened(), ..found }.guessed()
            }
        }
    }

    /// Field `name` of the Lua mixins of named type `id`, the last given first,
    /// as later ones overwrite earlier ones
    pub(super) fn lua_mixin_field(&self, prior: &Analysis, id: NamedId, name: &str) -> Option<Type> {
        let names = self.definitions.lua_mixins(id);
        if names.is_empty() {
            return None
        }
        self.note(Read::LuaMixins);
        names.iter().rev()
            .filter_map(|mixin| prior.lua_mixins.get(mixin)?.named.identity())
            .find_map(|at| self.own_field(prior, at, name, 0))
    }

    /// For mixin `mixin`, field `name` of the tables it was mixed into, and
    /// those they were, as `CreateFromMixins(mixin)` makes: what an object it
    /// reaches through any of them has. None if none has it
    pub(super) fn copied_into_field(&self, prior: &Analysis, mixin: TableId, name: &str) -> Option<Type> {
        let mut found: Option<Type> = None;
        let mut seen: HashSet<TableId> = HashSet::default();
        let mut pending = vec![mixin];
        while let Some(at) = pending.pop() {
            // Deep enough for any real chain of mixins
            if !seen.insert(at) || seen.len() > 64 {
                continue
            }
            self.note(Read::MixedInto(at));
            let copies = prior.mixin_copies.get(&at).into_iter().chain(self.layer.mixin_copies.get(&at)).flatten();
            for &table in copies {
                if let Some(ty) = self.own_field(prior, table, name, 0) {
                    *found.get_or_insert_default() |= ty;
                }
                pending.push(table);
            }
        }
        found
    }

    /// A field of a table made in the files: its own, else one it got from its
    /// mixins (the last mixin first, as later ones overwrite earlier ones)
    pub(super) fn own_field(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
        // A stand-in: what's written through it, and what any table it
        // stands for has
        if is_stand_in(table) && depth <= 16 {
            self.note(Read::StandIn(table));
            self.note(Read::Field(table, name_key(name)));
            let written = prior.fields.get(&table).and_then(|fields| fields.get(name)).copied();
            let found = union_all(self.stand_in_members(table).into_iter()
                .filter_map(|member| self.own_field(prior, member, name, depth + 1))
                .chain(written))?;
            return Some(self.read_through(Place::Field(table, name_key(name)), found))
        }
        self.note(Read::Field(table, name_key(name)));
        // A dependency's table: what it gave the table, and what's written to it here
        let layered = (table.file.0 >= LAYER_FILES).then(|| self.layer.field(table, name, 0)).flatten();
        let own = prior.fields.get(&table).and_then(|fields| fields.get(name)).copied();
        match (layered, own) {
            (Some(a), Some(b)) => return Some(a | b),
            (Some(found), None) | (None, Some(found)) => return Some(found),
            (None, None) => {}
        }
        // One of a constructor's elements: what's written through any of them
        if let Some(through) = elements_of(table).and_then(through_elements) {
            self.note(Read::Field(through, name_key(name)));
            if let Some(found) = prior.fields.get(&through).and_then(|fields| fields.get(name)) {
                return Some(*found)
            }
        }
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return None
        }
        self.note(Read::Mixins(table));
        // Its own mixins, or a dependency's table's
        let mixins = prior.mixins.get(&table).or_else(|| self.layer.mixins.get(&table));
        let from_mixins = mixins.into_iter().flatten().rev().find_map(|&mixin| {
            match mixin.named.identity() {
                Some(at) => self.own_field(prior, at, name, depth + 1),
                // One the definitions give, like Blizzard's `CallbackRegistryMixin`
                None => self.definitions.field(mixin, name),
            }
        });
        from_mixins.or_else(|| {
            // Missing fields come from the metatable's `__index`: another table
            // to look in, or a function giving them
            let index = self.metamethod(prior, table, "__index", depth)?;
            match index.named.identity() {
                Some(fallback) => self.own_field(prior, fallback, name, depth + 1),
                None => self.known_function(index.function).map(|f| f.returns.first()),
            }
        })
    }

    /// Field `name` (`__index`, `__call`, ...) of table `table`'s metatable
    pub(super) fn metamethod(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
        self.note(Read::Metatable(table));
        if let Some(metatable) = self.layer.metatables.get(&table).and_then(|m| m.named.identity())
            && !prior.metatables.contains_key(&table) {
            return self.own_field(prior, metatable, name, depth + 1)
        }
        let metatable = prior.metatables.get(&table)?.named.identity()?;
        self.own_field(prior, metatable, name, depth + 1)
    }

    /// Makes a change to what the walk is gathering, recording it for each
    /// function body being walked
    pub(super) fn apply(&mut self, write: Write) {
        self.effect(&write);
        self.record_write(write);
    }

    /// What a write changes, without noting it for the bodies being walked.
    /// What a table of a family is given, the family is too
    pub(super) fn effect(&mut self, write: &Write) {
        // What a table of a family, or one of a constructor's elements, is
        // given, the family or the elements are too
        // and what's written through the elements' identity, each may have
        let group = |at: &TableId| family(*at).filter(|f| f != at).or_else(|| elements_of(*at)).or_else(|| through_elements(*at));
        let family_write = match write {
            Write::Field(at, name, value) => group(at).map(|f| vec![Write::Field(f, name.clone(), *value)]),
            Write::Entry(at, key, value) => group(at).map(|f| vec![Write::Entry(f, *key, *value)]),
            Write::MixinAdded(at, mixin) => group(at).map(|f| vec![Write::MixinAdded(f, *mixin)]),
            Write::MixinsReplaced(at, mixins) => group(at).map(|f| mixins.iter().map(|&m| Write::MixinAdded(f, m)).collect()),
            _ => None,
        };
        for write in family_write.into_iter().flatten() {
            self.effect_here(&write);
        }
        // What's written through a stand-in, each table it stands for is
        // given too, so it's there however the table is reached
        let through = match write {
            Write::Field(at, name, value) if is_stand_in(*at) => self.stood_for(*at).into_iter()
                .map(|m| Write::Field(m, name.clone(), *value)).collect(),
            Write::Entry(at, key, value) if is_stand_in(*at) => self.stood_for(*at).into_iter()
                .map(|m| Write::Entry(m, *key, *value)).collect(),
            Write::MixinAdded(at, mixin) if is_stand_in(*at) => self.stood_for(*at).into_iter()
                .map(|m| Write::MixinAdded(m, *mixin)).collect(),
            _ => Vec::new(),
        };
        for write in through {
            self.effect(&write);
        }
        self.effect_here(write);
    }

    /// The tables stand-in `at` stands for: as the walk before found, and as
    /// this walk has so far
    pub(super) fn stand_in_members(&self, at: TableId) -> Vec<TableId> {
        let mut members: Vec<TableId> = self.prior.and_then(|prior| prior.stand_ins.get(&at)).into_iter()
            .chain(self.layer.stand_ins.get(&at))
            .flatten()
            .copied()
            .chain(self.stand_ins.borrow().get(&at).into_iter().flatten().copied())
            .collect();
        members.sort();
        members.dedup();
        members
    }

    /// The tables made in the files stand-in `at` stands for, through any
    /// stand-ins it stands for: each once, however they stand for each other
    pub(super) fn stood_for(&self, at: TableId) -> Vec<TableId> {
        let mut seen: HashSet<TableId> = HashSet::from_iter([at]);
        let mut pending = vec![at];
        let mut tables = Vec::new();
        while let Some(at) = pending.pop() {
            for member in self.stand_in_members(at) {
                if !seen.insert(member) {
                    continue
                }
                if is_stand_in(member) {
                    pending.push(member);
                } else {
                    tables.push(member);
                }
            }
        }
        tables
    }

    /// `ty`, to be kept at `place`: if it's more tables than are kept apart,
    /// the place's stand-in instead, which stands for them from now on. So
    /// what's kept there only grows. `record` notes it for replaying the
    /// body walking it, when it's not part of a write already replayed
    pub(super) fn stored(&mut self, place: Place, ty: Type, record: bool) -> Type {
        let (ty, added) = kept_at(self.stand_ins.get_mut(), place, ty);
        if let (true, Some((at, added))) = (record, added) {
            self.record_write(Write::StandIn(at, added));
        }
        ty
    }

    /// `before`, kept at `place`, joined with `value`. Once it's the place's
    /// stand-in, `value`'s tables go straight into what it stands for, rather
    /// than through a set of them
    pub(super) fn joined_at(&mut self, place: Place, before: Type, value: Type, record: bool) -> Type {
        if let NamedRef::Table(at) = before.named
            && is_stand_in(at) && matches!(value.named, NamedRef::Table(_) | NamedRef::Tables(_)) && at == stand_in(place) {
            let tables: Vec<TableId> = value.named.identities().into_iter().filter(|&t| t != at).collect();
            let known = self.stand_ins.get_mut().entry(at).or_default();
            let mut added: Vec<TableId> = tables.into_iter().filter(|t| known.binary_search(t).is_err()).collect();
            added.sort();
            added.dedup();
            if !added.is_empty() {
                known.extend(added.iter().copied());
                known.sort();
                if record {
                    self.record_write(Write::StandIn(at, added));
                }
            }
            return before | Type { named: NamedRef::None, ..value }
        }
        self.stored(place, before | value, record)
    }

    /// What's read through a stand-in, at `place`: if it's more tables than
    /// are kept apart, a stand-in for them, as for what's kept there, so a
    /// read doesn't make a set that size
    pub(super) fn read_through(&self, place: Place, found: Type) -> Type {
        let (ty, _) = kept_at(&mut self.stand_ins.borrow_mut(), place, found);
        if let NamedRef::Table(at) = ty.named
            && is_stand_in(at) {
            self.note(Read::StandIn(at));
        }
        ty
    }

    /// The place a binding's value is kept
    pub(super) fn binding_place(&self, id: BindingId) -> Place {
        let binding = &self.bindings[id];
        match binding.declaration {
            Some(range) => Place::Binding(Location { file: binding.file, offset: range.start(), slot: 0 }),
            None => Place::Global(name_key(&binding.name)),
        }
    }

    pub(super) fn effect_here(&mut self, write: &Write) {
        /// The entry for `name`, copying the name only when it's new
        fn named<'m, V: Default>(map: &'m mut HashMap<String, V>, name: &str) -> &'m mut V {
            if !map.contains_key(name) {
                map.insert(name.to_string(), V::default());
            }
            map.get_mut(name).expect("inserted above")
        }
        match write {
            Write::Field(at, name, value) => {
                let before = self.fields.get(at).and_then(|fields| fields.get(name)).copied().unwrap_or_default();
                let joined = self.joined_at(Place::Field(*at, name_key(name)), before, *value, false);
                *named(self.fields.entry(*at).or_default(), name) = joined;
            }
            Write::AnyTableField(name, value) => {
                let before = self.any_table_fields.get(name).copied().unwrap_or_default();
                let joined = self.joined_at(Place::AnyField(name_key(name)), before, *value, false);
                *named(&mut self.any_table_fields, name) = joined;
            }
            Write::StandIn(at, members) => {
                let known = self.stand_ins.get_mut().entry(*at).or_default();
                known.extend(members.iter().copied());
                known.sort();
                known.dedup();
            }
            Write::Opened(id) => {
                self.opened_types.insert(*id);
            }
            Write::StoredInField(function, name) => {
                let names = self.stored_in_fields.entry(*function).or_default();
                if !names.contains(name) {
                    names.insert(name.clone());
                }
            }
            Write::Entry(at, key, value) => {
                let before = self.entries.get(at).copied().unwrap_or_default();
                let keys = self.joined_at(Place::EntryKeys(*at), before.keys, *key, false);
                let values = self.joined_at(Place::Entries(*at), before.values, *value, false);
                let entries = self.entries.entry(*at).or_default();
                entries.keys = keys;
                entries.values = values;
            }
            Write::Metatable(at, value) => *self.metatables.entry(*at).or_default() |= *value,
            Write::CallArguments(function, values) => {
                self.call_arguments.entry(*function)
                    .and_modify(|all| *all = all.union(values))
                    .or_insert_with(|| values.clone());
            }
            Write::MixinsReplaced(at, mixins) => { self.mixins.insert(*at, mixins.clone()); }
            Write::MixinAdded(at, mixin) => {
                let recorded = self.mixins.entry(*at).or_default();
                if !recorded.contains(mixin) {
                    recorded.push(*mixin);
                }
            }
            Write::CopiedFrom(copy, source) => {
                self.copies.insert(*copy, *source);
            }
            Write::MixedInto(mixin, object) => {
                let recorded = self.mixed_into.entry(*mixin).or_default();
                if !recorded.contains(object) {
                    recorded.push(*object);
                }
            }
            Write::PoolCallback(function) => { self.pool_callbacks.insert(*function); }
            Write::Escaped(function) => { self.escaped_functions.insert(*function); }
            Write::SelfPassed(function, value, by_call) => {
                let passed = self.self_passed.entry(*function).or_default();
                match by_call {
                    true => passed.0 |= *value,
                    false => passed.1 |= *value,
                }
            }
            Write::AssignedTo(function, binding) => { self.assigned_to.insert(*function, *binding); }
            Write::ParameterMixin(at, parameter) => {
                let recorded = self.parameter_mixins.entry(*at).or_default();
                if !recorded.contains(parameter) {
                    recorded.push(*parameter);
                }
            }
            Write::Parent(at, parent) => *self.parents.entry(*at).or_default() |= *parent,
            Write::Triggered(at, event, values) => {
                self.triggered.entry((*at, *event))
                    .and_modify(|all| *all = all.union(values))
                    .or_insert_with(|| values.clone());
            }
            Write::PoolObject(at, object) => { self.pool_objects.insert(*at, *object); }
            Write::Function(id, info) => { self.functions.insert(*id, info.clone()); }
            Write::Binding(id, types) => {
                let joined = self.joined_at(self.binding_place(*id), self.bindings[*id].types, *types, false);
                let binding = &mut self.bindings[*id];
                binding.types = joined;
                binding.assigned = true;
                binding.reassigned = true;
                // Only a body declared inside the binding's function records it
                binding.closure_written = true;
                self.set_current(*id, self.current[*id] | *types);
            }
        }
    }

    /// Notes a change for each function body being walked. A variable's
    /// assignment only matters to bodies it was declared outside of
    pub(super) fn record_write(&mut self, write: Write) {
        // Only the innermost: each body's writes join its parent's when it ends
        let Some(innermost) = self.frames.last_mut() else { return };
        if let Write::Binding(id, _) = write
            && id >= innermost.bindings_start {
            return
        }
        let write = Arc::new(write);
        innermost.own_writes.push(write.clone());
        innermost.writes.push(write);
        if let Some(body) = self.deps.get_mut().last_mut() {
            let index = body.write_deps.len();
            body.write_deps.push(None);
            if let Some(level) = body.levels.last_mut() {
                level.writes.push(index);
            }
        }
    }

    /// Starts a level of its own for a declaration at the body's top, like
    /// a parameter's, so it depends only on what it reads, not on all the
    /// top reads
    pub(super) fn begin_declaration(&mut self) {
        if let Some(body) = self.deps.get_mut().last_mut() {
            body.levels.push(DepLevel::default());
        }
    }

    /// Ends it: what it declared depends on what it read, and on `also`
    pub(super) fn end_declaration(&mut self, also: Option<Read>) {
        if let Some(body) = self.deps.get_mut().last_mut()
            && body.levels.len() > 1
            && let Some(mut level) = body.levels.pop() {
            if let Some(also) = also {
                let number = body.key(&also);
                level.reads.push(number);
            }
            body.resolve(level);
        }
    }

    /// The statement being walked gives the body's signature something
    pub(super) fn deps_to_signature(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.signature = true;
        }
    }

    /// The statement being walked requires a type of one of the body's parameters
    pub(super) fn deps_to_requirements(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.requirement = true;
            level.signature = true;
        }
    }

    /// The statement being walked calls a function
    pub(super) fn deps_to_calls(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.calls = true;
        }
    }

    /// Upvalues a body read as they are where it is: the parent's too, for
    /// those declared outside it
    pub(super) fn upvalues_to_parent(&mut self, upvalues: &[(BindingId, Type)]) {
        if let Some(parent) = self.frames.last_mut() {
            parent.upvalues.extend(upvalues.iter().filter(|&&(id, _)| id < parent.bindings_start).copied());
        }
    }

    /// A body's reads and writes become its parent's too, as the parent
    /// replayed does all the body did
    pub(super) fn pass_to_parent(&mut self, reads: &HashSet<Read>, writes: &[Arc<Write>]) {
        if let Some(parent) = self.reads.borrow_mut().last_mut() {
            parent.extend(reads.iter().cloned());
        }
        if let Some(parent) = self.frames.last_mut() {
            for write in writes {
                // An assignment to a variable the parent declared is the parent's own
                if let Write::Binding(id, _) = **write
                    && id >= parent.bindings_start {
                    continue
                }
                parent.writes.push(write.clone());
            }
        }
    }

    /// Each function of the files `value` may be, as escaping (see
    /// `Write::Escaped`)
    pub(super) fn escape(&mut self, value: Type) {
        for function in value.function.alternatives() {
            if let FunctionRef::One(function) = function {
                self.apply(Write::Escaped(function));
            }
        }
    }

    pub(super) fn field_write(&mut self, base: Type, name: &str, value: Type) {
        for function in value.function.alternatives() {
            if let FunctionRef::One(function) = function {
                self.apply(Write::StoredInField(function, name.to_string()));
            }
        }
        match base.named {
            NamedRef::Table(id) => {
                if let Some(value) = self.stored_in(id, value) {
                    self.apply(Write::Field(id, name.to_string(), value));
                }
            }
            NamedRef::Instance { at: id, .. } => {
                self.apply(Write::Field(id, name.to_string(), value));
                // A parameter's stand-in for an object of unknown identity:
                // as for any object of its type, which it may be
                if id.slot & PSEUDO != 0 {
                    self.apply(Write::AnyTableField(name.to_string(), value));
                }
            }
            // Could be any table, or shared by every object of a WoW type
            NamedRef::One(_) | NamedRef::Extended(_) | NamedRef::Unknown => self.apply(Write::AnyTableField(name.to_string(), value)),
            // Whichever object it is gets it, so each may have it. Also kept
            // for tables whose identity isn't known, as the objects' mixins'
            // `self` (the mixin table) reads what's set on the objects there
            NamedRef::Several(_) => {
                for object in base.named.alternatives() {
                    match object {
                        // A table made in the files, as for one alone
                        NamedRef::Table(at) => if let Some(value) = self.stored_in(at, value) {
                            self.apply(Write::Field(at, name.to_string(), value));
                        }
                        _ => if let Some(at) = object.identity() {
                            self.apply(Write::Field(at, name.to_string(), value));
                        }
                    }
                }
                self.apply(Write::AnyTableField(name.to_string(), value));
            }
            // Whichever table it is gets it
            NamedRef::Tables(_) => {
                for at in base.named.identities() {
                    if let Some(value) = self.stored_in(at, value) {
                        self.apply(Write::Field(at, name.to_string(), value));
                    }
                }
            }
            // Not a table, as far as is known, but written to as one: one
            // it can't identify. Most likely read as a field of that name
            // other tables have (`entry.resource` of an unknown `entry`), so
            // the write only goes missing once that's known, and comes back
            // when its own write is then gone, each walk
            NamedRef::None if base.set != TypeSet::NEVER => self.apply(Write::AnyTableField(name.to_string(), value)),
            NamedRef::None => {}
        }
    }
}

pub(super) fn field_access(node: &SyntaxNode) -> Option<(Expression, SyntaxToken)> {
    let mut significant = node.children_with_tokens()
        .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment));
    let (Some(NodeOrToken::Node(base)), Some(NodeOrToken::Token(dot)), Some(NodeOrToken::Token(field)), None) =
        (significant.next(), significant.next(), significant.next(), significant.next()) else { return None };
    if dot.kind() != SyntaxKind::Dot || field.kind() != SyntaxKind::Name {
        return None
    }
    Some((Expression::cast(base)?, field))
}

/// `t[k]`: the table and the key
pub(super) fn index_access(node: &SyntaxNode) -> Option<(Expression, Expression)> {
    let mut significant = node.children_with_tokens()
        .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment));
    let (Some(NodeOrToken::Node(base)), Some(NodeOrToken::Token(open)), Some(NodeOrToken::Node(key)), Some(NodeOrToken::Token(close)), None) =
        (significant.next(), significant.next(), significant.next(), significant.next(), significant.next()) else { return None };
    if open.kind() != SyntaxKind::LeftSquareBracket || close.kind() != SyntaxKind::RightSquareBracket {
        return None
    }
    Some((Expression::cast(base)?, Expression::cast(key)?))
}

/// The field a quoted string literal names, if it could be written as one:
/// `"name"` or `'name'`
pub(super) fn string_field_name(literal: &str) -> Option<&str> {
    let literal = literal.trim();
    let quote = literal.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let name = literal.strip_prefix(quote)?.strip_suffix(quote)?;
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    (starts_well && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(name)
}

/// Whether what `node` gives is only tested (as an `if`'s or `while`'s
/// condition is) or worked out from (by an operator other than `and` and
/// `or`, which may give it itself), so it goes nowhere else
pub(super) fn only_tested(node: &SyntaxNode) -> bool {
    let mut node = node.clone();
    while let Some(parent) = node.parent() {
        match parent.kind() {
            SyntaxKind::Expression | SyntaxKind::GroupedExpression => {}
            SyntaxKind::BinaryExpression => {
                let logical = parent.children_with_tokens().filter_map(|t| t.into_token()).all(|t| {
                    matches!(t.kind(), SyntaxKind::AndKeyword | SyntaxKind::OrKeyword | SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment)
                });
                if !logical {
                    return true
                }
            }
            SyntaxKind::UnaryExpression | SyntaxKind::Condition => return true,
            _ => return false,
        }
        node = parent;
    }
    false
}
