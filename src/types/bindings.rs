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

//! Resolving variable names to bindings, and reading and writing them

use super::*;

impl<'a> Walker<'a> {
    pub(super) fn new_binding(&mut self, name: &str, kind: BindingKind, declaration: Option<TextRange>, types: Type, function: usize, scope_depth: usize) -> BindingId {
        let id = self.bindings.len();
        // Bindings are made in the same order each walk, so the previous
        // walk's usually has the name already
        let name = match self.prior.and_then(|prior| prior.bindings.get(id)) {
            Some(prior) if &*prior.name == name => prior.name.clone(),
            _ => name.into(),
        };
        self.bindings.push(Binding {
            name,
            kind,
            declaration,
            types,
            file: self.file,
            visible: None,
            function,
            scope_depth,
            assigned: false,
            reassigned: false,
            loop_depth: self.loop_depth,
            last_write: declaration.map_or(TextSize::from(0), |range| range.start()),
            closure_written: false,
        });
        self.current.push(types);
        // A local starts with what its declaring statement read
        if kind != BindingKind::Global
            && let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.bindings.push(id);
        }
        id
    }

    pub(super) fn declare(&mut self, token: &SyntaxToken, kind: BindingKind, types: Type) -> BindingId {
        let place = Place::Binding(self.location(token.text_range().start()));
        let mut types = self.stored(place, types, true);
        let pseudo = Location { slot: PSEUDO, ..self.location(token.text_range().start()) };
        match types.named {
            // A table we can't identify: track its fields under this variable
            NamedRef::Unknown => types.named = NamedRef::Table(pseudo),
            // A parameter required to be an object of a named type, which
            // has no identity of its own: still track what's written through
            // it under this variable, as before it was known to be one, so
            // what the body reads back is the same either way
            NamedRef::One(named) | NamedRef::Extended(named) if kind == BindingKind::Parameter => {
                let extended = matches!(types.named, NamedRef::Extended(_));
                types.named = NamedRef::Instance { named, at: pseudo, extended };
            }
            _ => {}
        }
        let depth = self.scopes.len() - 1;
        let id = self.new_binding(token.text(), kind, Some(token.text_range()), types, self.function, depth);
        self.bindings[id].assigned = true;
        let end = self.scopes[depth].end.max(token.text_range().end());
        self.bindings[id].visible = Some(TextRange::new(token.text_range().end(), end));
        self.scopes[depth].names.insert(token.text().to_string(), id);
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: true, callee: false, call_returns: None });
        id
    }

    pub(super) fn resolve(&mut self, name: &str) -> BindingId {
        for scope in self.scopes.iter().rev() {
            if let Some(&id) = scope.names.get(name) {
                return id;
            }
        }
        self.resolve_global(name)
    }

    /// The global `name`, even where a local hides it (as `_G.name` reaches it)
    pub(super) fn resolve_global(&mut self, name: &str) -> BindingId {
        if let Some(&id) = self.globals.get(name) {
            return id;
        }
        let id = self.new_binding(name, BindingKind::Global, None, Type::default(), MAIN_CHUNK, 0);
        self.set_current(id, self.global_before_assigned(id));
        self.globals.insert(name.to_string(), id);
        self.globals_created.push(name.to_string());
        id
    }

    /// What global `id` holds before the files first assign it. One they
    /// assign is theirs, so until they do it's nil, or what a dependency or
    /// the game gives it, as `T = T or {}` and `if G == nil then G = 1 end`
    /// expect. One they never assign is unknown (unless a preset applies,
    /// see `builtin_global`). On the first walk which it is isn't known yet,
    /// so it's nothing
    pub(super) fn global_before_assigned(&self, id: BindingId) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::Binding(id));
        let name = &self.bindings[id].name;
        match prior.bindings.get(id).filter(|p| &p.name == name) {
            Some(p) if p.assigned => self.preset_global(name).unwrap_or(Type::NIL),
            _ => Type::UNKNOWN,
        }
    }

    pub(super) fn is_global(&self, name: &str) -> bool {
        !self.scopes.iter().any(|s| s.names.contains_key(name))
    }

    pub(super) fn read(&mut self, token: &SyntaxToken) -> Type {
        let id = self.resolve(token.text());
        self.read_binding(token, id)
    }

    /// A read of binding `id` at `token`
    pub(super) fn read_binding(&mut self, token: &SyntaxToken, id: BindingId) -> Type {
        let index = self.references.len();
        let types = if self.bindings[id].function == self.function {
            self.current[id]
        } else if let Some(types) = self.upvalue_here(id) {
            types
        } else {
            // An upvalue: the closure could run after any assignment
            self.deferred.push(index);
            self.prior_binding_types(id)
        };
        let types = self.builtin_global(id).unwrap_or(types);
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: false, callee: false, call_returns: None });
        types
    }

    /// An upvalue as it is where the closure reading it is made, when no
    /// assignment can come after: every write to it is in the function
    /// declaring it, before the closure, with no loop around the closure that
    /// isn't around the declaration too (which would run the writes again).
    /// What's narrowed there holds then, as a check that it's a table does
    /// for a callback made after it. Known from the previous walk, which saw
    /// every write
    pub(super) fn upvalue_here(&mut self, id: BindingId) -> Option<Type> {
        let prior = self.prior?;
        let binding = &self.bindings[id];
        if binding.kind == BindingKind::Global {
            return None
        }
        // The closure made in the binding's function, which this is inside
        let declaring = self.function_ranges.iter().position(|&(_, function)| function == binding.function);
        let closure = declaring.map_or(0, |at| at + 1);
        let (&(range, _), &loop_depth) = self.function_ranges.get(closure).zip(self.closure_loop_depths.get(closure))?;
        self.note(Read::Binding(id));
        let seen = prior.bindings.get(id).filter(|p| p.name == binding.name)?;
        if seen.closure_written || seen.last_write >= range.start() || loop_depth != binding.loop_depth || binding.file != self.file {
            return None
        }
        let types = self.current[id];
        // Only ever narrower: a parameter's type there may not be settled yet,
        // and a loop may have widened it
        if (types | seen.types) != seen.types {
            return None
        }
        for frame in self.frames.iter_mut().filter(|frame| id < frame.bindings_start) {
            frame.upvalues.push((id, types));
        }
        Some(types)
    }

    /// Every value the previous walk saw assigned to this binding. Bindings are
    /// created in the same order on every walk, so the ids line up. On the
    /// first walk nothing is known yet, so types grow from nothing rather
    /// than shrinking from unknown: a cycle (a field written with what's read
    /// from it) then settles on what really flows round it, where starting
    /// from unknown would keep it unknown forever
    pub(super) fn prior_binding_types(&self, id: BindingId) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::Binding(id));
        prior.bindings.get(id)
            .filter(|prior| prior.name == self.bindings[id].name)
            .map_or(Type::UNKNOWN, |prior| prior.types)
    }

    /// True for a global the file never assigns (as far as this walk has seen
    /// on the first walk, anywhere on the second), so presets can apply
    pub(super) fn is_unassigned_global(&self, id: BindingId) -> bool {
        self.note(Read::Binding(id));
        let binding = &self.bindings[id];
        let prior = self.prior.and_then(|p| p.bindings.get(id)).filter(|p| p.name == binding.name);
        binding.kind == BindingKind::Global && !binding.assigned && prior.is_none_or(|p| !p.assigned)
    }

    pub(super) fn builtin_global(&self, id: BindingId) -> Option<Type> {
        if !self.is_unassigned_global(id) {
            return None
        }
        self.preset_global(&self.bindings[id].name)
    }

    pub(super) fn write(&mut self, token: &SyntaxToken, types: Type) {
        let id = self.resolve(token.text());
        self.write_binding(token, id, types)
    }

    /// A write of binding `id` at `token`
    pub(super) fn write_binding(&mut self, token: &SyntaxToken, id: BindingId, types: Type) {
        // Fields narrowed through it are another table's now
        if self.narrowed_fields.iter().any(|(root, _, _)| *root == id) {
            self.forget_fields();
        }
        // Kept where another call can return it again
        if self.bindings[id].function != self.function || self.bindings[id].kind == BindingKind::Global {
            let kept: Vec<TableId> = types.named.identities().into_iter().chain(self.several_objects(types)).collect();
            for function in &mut self.kept_outside {
                function.extend(kept.iter().copied());
            }
        }
        let in_closure = self.bindings[id].function != self.function;
        let joined = self.joined_at(self.binding_place(id), self.bindings[id].types, types, true);
        let types = self.stored(self.binding_place(id), types, true);
        let binding = &mut self.bindings[id];
        binding.types = joined;
        binding.assigned = true;
        binding.reassigned = true;
        if in_closure {
            binding.closure_written = true;
        } else {
            binding.last_write = binding.last_write.max(token.text_range().start());
        }
        // An `if`'s branches don't count, as the `if` joins what each leaves
        let innermost_conditional = self.scopes.iter().rposition(|s| s.conditional && !s.joined);
        let replaces = binding.function == self.function
            && innermost_conditional.is_none_or(|depth| binding.scope_depth >= depth);
        if replaces {
            self.set_current(id, types);
        } else {
            self.set_current(id, self.current[id] | types);
        }
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: true, callee: false, call_returns: None });
        self.record_write(Write::Binding(id, types));
    }
}
