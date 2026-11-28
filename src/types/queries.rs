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

//! What the editor asks of a finished analysis: hovers, definitions and
//! references

use super::*;

impl Analysis {
    /// The hover text for whatever name is at `offset`, and the name's range
    pub fn hover(&self, file: FileId, offset: TextSize) -> Option<(String, TextRange)> {
        if let Some(r) = self.reference_at(file, offset) {
            let binding = self.binding(r);
            return Some((format!("{} {}: {}", binding.kind, binding.name, self.describe_callee(r.types, r.call_returns.as_ref())), r.range))
        }
        let field = self.field_reference_at(file, offset)?;
        Some((format!("field {}: {}", self.field_path(field), self.describe_callee(field.types, field.call_returns.as_ref())), field.range))
    }

    /// Where the variable or field named at `offset` is given its value: a
    /// local's or parameter's declaration, every assignment to a global, or
    /// every assignment to the field on the table it's reached through (or the
    /// mixin or `__index` table it comes from). Failing that, the start of the
    /// function the field holds, if it's one known function
    pub fn definition(&self, file: FileId, offset: TextSize) -> Vec<Site> {
        if let Some(r) = self.reference_at(file, offset) {
            return self.variable_sites(self.binding(r))
        }
        let Some(field) = self.field_reference_at(file, offset) else { return Vec::new() };
        let mut sites = self.field_sites(field.table, field.name());
        // On an object made from an XML template: where the template's Lua
        // mixins (the addon's tables) have it
        if sites.is_empty() {
            let objects = match field.base {
                NamedRef::Several(_) => field.base.alternatives(),
                base => vec![base],
            };
            for id in objects.iter().filter_map(|object| object.named_id()) {
                for mixin in self.definitions.lua_mixins(id) {
                    if let Some(at) = self.lua_mixins.get(mixin).and_then(|ty| ty.named.identity()) {
                        add_sites(&mut sites, &self.field_sites(Some(at), field.name()));
                    }
                }
                if !sites.is_empty() {
                    break
                }
            }
        }
        // Else where the function it holds (or each of several) is defined
        if sites.is_empty() {
            for function in field.types.function.alternatives() {
                if let FunctionRef::One(id) = function {
                    sites.push(Site { file: id.file, range: TextRange::empty(id.offset) });
                }
            }
        }
        sites
    }

    /// Where what's at `offset` is defined outside the files and those they
    /// depend on: in Blizzard's code, or an addon's XML. Only when they don't
    /// define it, as a file and line
    pub fn external_definition(&self, file: FileId, offset: TextSize) -> Option<(std::path::PathBuf, u32)> {
        if !self.definition(file, offset).is_empty() {
            return None
        }
        if let Some(r) = self.reference_at(file, offset) {
            let binding = self.binding(r);
            return (binding.kind == BindingKind::Global).then(|| self.definitions.global_site(&binding.name)).flatten()
        }
        let field = self.field_reference_at(file, offset)?;
        let objects = match field.base {
            NamedRef::Several(_) => field.base.alternatives(),
            base => vec![base],
        };
        objects.into_iter().find_map(|object| self.external_member(object, field.name(), 0))
    }

    /// Where member `name` of `object` is defined in Blizzard's code or an
    /// addon's XML: on its type, or a mixin or the original of a copy, for a
    /// table made in the files
    pub(super) fn external_member(&self, object: NamedRef, name: &str, depth: usize) -> Option<(std::path::PathBuf, u32)> {
        if depth > 16 {
            return None
        }
        if let Some(site) = object.named_id().and_then(|id| self.definitions.member_site(id, name)) {
            return Some(site)
        }
        let at = object.identity()?;
        let mixins = self.mixins.get(&at).or_else(|| self.layer.mixins.get(&at));
        let from_mixins = mixins.into_iter().flatten().rev().find_map(|mixin| self.external_member(mixin.named, name, depth + 1));
        from_mixins.or_else(|| {
            let source = self.copies.get(&at).or_else(|| self.layer.copies.get(&at))?;
            self.external_member(NamedRef::Table(*source), name, depth + 1)
        })
    }

    pub(super) fn variable_sites(&self, binding: &Binding) -> Vec<Site> {
        if binding.kind != BindingKind::Global {
            return binding.declaration.map(|range| Site { file: binding.file, range }).into_iter().collect()
        }
        let mut sites: Vec<Site> = self.references.iter()
            .filter(|r| r.write && self.binding(r).kind == BindingKind::Global && self.binding(r).name == binding.name)
            .map(|r| Site { file: r.file, range: r.range })
            .collect();
        add_sites(&mut sites, self.layer.global_sites.get(&*binding.name).map_or(&[], Vec::as_slice));
        sites
    }

    /// The fields of tables that the files, or an addon depended on, assign
    /// somewhere: unlike `fields`, not those only Blizzard's functions give
    /// them, as `r` is set by `CreateColor`
    pub fn assigned_fields(&self) -> HashSet<(TableId, &str)> {
        let own = self.field_references.iter()
            .filter(|f| f.write)
            .filter_map(|f| Some((f.table?, f.name())));
        let layered = self.layer.field_sites.iter()
            .flat_map(|(&at, fields)| fields.iter().filter(|(_, sites)| !sites.is_empty()).map(move |(name, _)| (at, name.as_str())));
        own.chain(layered).collect()
    }

    /// Where field `name` of `table` is assigned. As for reading it, the
    /// writes to tables of unknown identity are the fallback
    pub(super) fn field_sites(&self, table: Option<TableId>, name: &str) -> Vec<Site> {
        let holder = table.and_then(|table| self.field_holder(table, name, 0));
        let mut sites: Vec<Site> = self.field_references.iter()
            .filter(|f| f.write && f.table == holder && f.name() == name)
            .map(|f| Site { file: f.file, range: f.range })
            .collect();
        let layered = match holder {
            Some(at) => self.layer.field_sites.get(&at).and_then(|fields| fields.get(name)),
            None => self.layer.any_field_sites.get(name),
        };
        add_sites(&mut sites, layered.map_or(&[], Vec::as_slice));
        // A field `CopyTable` gave a copy is where the original's is set
        if sites.is_empty()
            && let Some(source) = holder.and_then(|at| self.copies.get(&at).or_else(|| self.layer.copies.get(&at)))
            && Some(*source) != table {
            return self.field_sites(Some(*source), name)
        }
        sites
    }

    /// The table that field `name` of `table` comes from: its own, else the
    /// one a mixin (the last first) or its metatable's `__index` gives it
    pub(super) fn field_holder(&self, table: TableId, name: &str, depth: usize) -> Option<TableId> {
        let field = |at: TableId, name: &str| self.fields.get(&at).and_then(|f| f.get(name)).copied()
            .or_else(|| self.layer.fields.get(&at).and_then(|f| f.get(name)).copied());
        if field(table, name).is_some() {
            return Some(table)
        }
        if depth > 16 {
            return None
        }
        let mixins: Vec<Type> = self.mixins.get(&table).into_iter().chain(self.layer.mixins.get(&table)).flatten().copied().collect();
        if let Some(found) = mixins.iter().rev().find_map(|mixin| self.field_holder(mixin.named.identity()?, name, depth + 1)) {
            return Some(found)
        }
        let metatable = self.metatables.get(&table).or_else(|| self.layer.metatables.get(&table))?.named.identity()?;
        let index_holder = self.field_holder(metatable, "__index", depth + 1)?;
        let fallback = field(index_holder, "__index")?.named.identity()?;
        self.field_holder(fallback, name, depth + 1)
    }

    pub fn field_reference_at(&self, file: FileId, offset: TextSize) -> Option<&FieldReference> {
        let after = self.field_references.partition_point(|r| (r.file, r.range.start()) <= (file, offset));
        let candidate = self.field_references.get(after.checked_sub(1)?)?;
        (candidate.file == file && candidate.range.contains_inclusive(offset)).then_some(candidate)
    }

    pub fn reference_at(&self, file: FileId, offset: TextSize) -> Option<&Reference> {
        let after = self.references.partition_point(|r| (r.file, r.range.start()) <= (file, offset));
        let candidate = self.references.get(after.checked_sub(1)?)?;
        (candidate.file == file && candidate.range.contains_inclusive(offset)).then_some(candidate)
    }

    /// Every key and value a table made in the files holds, as `pairs` would
    /// give them: its entries and its named fields. None if nothing's known
    pub fn contents(&self, at: TableId) -> Option<Entries> {
        let mut contents = self.entries.get(&at).copied().unwrap_or_default();
        if let Some(layered) = self.layer.entries.get(&at) {
            contents.keys |= layered.keys;
            contents.values |= layered.values;
        }
        for &value in self.fields.get(&at).into_iter().chain(self.layer.fields.get(&at)).flat_map(|fields| fields.values()) {
            contents.keys |= Type::of(TypeSet::STRING);
            contents.values |= value.without(TypeSet::NIL);
        }
        (contents.values.set != TypeSet::NEVER).then_some(contents)
    }

    /// A table with entries (not just named fields) shown by what it holds:
    /// `string[]` when its keys are all numbers, otherwise `table<K, V>`
    pub(super) fn describe_contents(&self, ty: Type) -> Option<String> {
        // Any of several tables: what any of them holds
        let tables = match ty.named {
            NamedRef::Table(_) | NamedRef::Tables(_) => ty.named.identities(),
            _ => return None,
        };
        if !tables.iter().any(|at| self.entries.contains_key(at) || self.layer.entries.contains_key(at)) {
            return None
        }
        let contents = tables.iter().filter_map(|&at| self.contents(at))
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })?;
        // Nothing stored is nil, so `unknown (not nil)` would only be noise
        let shown = |ty: Type| match ty.set == TypeSet::UNKNOWN.without(TypeSet::NIL) {
            true => self.describe_shallow(Type::UNKNOWN),
            false => self.describe_shallow(ty),
        };
        let values = shown(contents.values);
        if contents.keys.set == TypeSet::NUMBER {
            let wrap = values.contains(['|', ' ', '?']);
            return Some(if wrap { format!("({values})[]") } else { format!("{values}[]") })
        }
        Some(format!("table<{}, {}>", shown(contents.keys), values))
    }

    /// How a field was reached, without whitespace or comments, like
    /// `addonTable.hiddenTable`
    pub fn field_path(&self, field: &FieldReference) -> String {
        let root = self.roots.get(field.file.0 as usize).map(|green| SyntaxNode::new_root(green.clone()));
        field.path(root.as_ref())
    }

    /// Each file's tree, to find nodes in
    pub(super) fn syntax_roots(&self) -> Vec<SyntaxNode> {
        self.roots.iter().map(|green| SyntaxNode::new_root(green.clone())).collect()
    }

    /// What each file assigns that other files can see: globals, and fields
    /// of tables when it's known which table. In no particular order, with
    /// repeats
    pub fn writes(&self) -> Vec<(FileId, Written)> {
        let mut writes = Vec::new();
        for r in self.references.iter().filter(|r| r.write) {
            let binding = self.binding(r);
            if binding.kind == BindingKind::Global {
                writes.push((r.file, Written::Global(binding.name.to_string())));
            }
        }
        for f in self.field_references.iter().filter(|f| f.write) {
            if let Some(at) = f.table {
                writes.push((f.file, Written::Field(at, f.name().to_string())));
            }
        }
        writes
    }

    /// The function a value is, if it's exactly one known function
    pub fn function_info(&self, function: FunctionRef) -> Option<&FunctionInfo> {
        match function {
            FunctionRef::One(id) => self.functions.get(&id).or_else(|| self.layer.functions.get(&id)),
            FunctionRef::Builtin(id) => self.definitions.function(id),
            _ => None,
        }
    }

    pub fn binding(&self, reference: &Reference) -> &Binding {
        &self.bindings[reference.binding]
    }
}
