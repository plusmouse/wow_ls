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

//! A [`Layer`]: what an analysis of some files leaves for analysing others
//! on top of them

use super::*;

/// What the addons an addon depends on make visible to it: their globals,
/// functions and tables, from their own analyses. A dependent reads them but
/// can't change them, so only an addon itself types its API, however many
/// addons use it. Their files are numbered in a range of their own, by path
/// (see `layer_file`), so one file is one file in every addon depending on it
#[derive(Debug, Default, Clone)]
pub struct Layer {
    pub globals: HashMap<String, Type>,
    pub functions: HashMap<FunctionId, FunctionInfo>,
    pub fields: HashMap<TableId, HashMap<String, Type>>,
    pub any_table_fields: HashMap<String, Type>,
    pub opened_types: HashSet<NamedId>,
    pub function_fields: HashSet<String>,
    pub entries: HashMap<TableId, Entries>,
    pub metatables: HashMap<TableId, Type>,
    pub mixins: HashMap<TableId, Vec<Type>>,
    pub mixed_into: HashMap<TableId, (NamedId, bool)>,
    /// The tables each mixin was mixed into (see `Analysis::mixin_copies`)
    pub mixin_copies: HashMap<TableId, Vec<TableId>>,
    /// The table each table `CopyTable` made is a copy of
    pub copies: HashMap<TableId, TableId>,
    pub parents: HashMap<TableId, Type>,
    /// What each callback registry's events pass (see `Analysis::triggered`)
    pub triggered: HashMap<(TableId, u64), Values>,
    pub pool_objects: HashMap<TableId, Type>,
    pub table_names: HashMap<TableId, String>,
    /// Where each global is assigned
    pub global_sites: HashMap<String, Vec<Site>>,
    /// Where each field of each table is assigned
    pub field_sites: HashMap<TableId, HashMap<String, Vec<Site>>>,
    /// Where fields of tables whose identity wasn't known are assigned, by name
    pub any_field_sites: HashMap<String, Vec<Site>>,
    /// The tables each stand-in stands for (see `Analysis::stand_ins`)
    pub stand_ins: HashMap<TableId, Vec<TableId>>,
}

/// Where the files of addons in a `Layer` are numbered from: far above any
/// addon's own files, and below the pseudo files of namespaces
pub(super) const LAYER_FILES: u32 = 1 << 30;

/// A dependency's file (or namespace, by a key of its own), numbered the
/// same in every analysis this run makes
pub fn layer_file(key: &str) -> FileId {
    let mut numbers = layer_file_numbers().lock().unwrap_or_else(|e| e.into_inner());
    let next = numbers.len() as u32;
    FileId(LAYER_FILES + *numbers.entry(key.to_string()).or_insert(next))
}

/// The key a dependency's file was numbered by (its path), if it's one
pub fn layer_file_key(file: FileId) -> Option<String> {
    let number = file.0.checked_sub(LAYER_FILES)?;
    let numbers = layer_file_numbers().lock().unwrap_or_else(|e| e.into_inner());
    numbers.iter().find(|&(_, &n)| n == number).map(|(key, _)| key.clone())
}

pub(super) fn layer_file_numbers() -> &'static std::sync::Mutex<std::collections::HashMap<String, u32>> {
    static NUMBERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u32>>> = std::sync::OnceLock::new();
    NUMBERS.get_or_init(Default::default)
}

impl Layer {
    /// What `analysis` (of an addon's files, `paths`, in that order) makes
    /// visible to addons loaded after it, with what it saw of its own
    /// dependencies. `namespace` names its `...` namespace table
    pub fn of(analysis: &Analysis, paths: &[std::path::PathBuf], namespace: &str) -> Layer {
        let files: Vec<FileId> = paths.iter().map(|p| layer_file(&p.to_string_lossy())).collect();
        let own_namespace = addon_namespace(0).file;
        let namespace = layer_file(&format!("namespace of {namespace}"));
        let file = |file: FileId| match files.get(file.0 as usize) {
            Some(&renumbered) => renumbered,
            None if file == own_namespace => namespace,
            // Already a dependency's, or a pseudo file of no file
            None => file,
        };
        let mut layer = (*analysis.layer).clone();
        let mut own = Layer::default();
        for binding in analysis.bindings.iter().filter(|b| b.kind == BindingKind::Global && b.assigned) {
            *own.globals.entry(binding.name.to_string()).or_default() |= renumber_type(binding.types, &file);
        }
        for (id, info) in &analysis.functions {
            own.functions.insert(renumber(*id, &file), renumber_function(info, &file));
        }
        for (at, fields) in &analysis.fields {
            let mine = own.fields.entry(renumber(*at, &file)).or_default();
            for (name, &ty) in fields {
                *mine.entry(name.clone()).or_default() |= renumber_type(ty, &file);
            }
        }
        for (name, &ty) in &analysis.any_table_fields {
            *own.any_table_fields.entry(name.clone()).or_default() |= renumber_type(ty, &file);
        }
        own.opened_types.extend(analysis.opened_types.iter().copied());
        own.function_fields = analysis.function_fields.clone();
        for (at, entries) in &analysis.entries {
            let mine = own.entries.entry(renumber(*at, &file)).or_default();
            mine.keys |= renumber_type(entries.keys, &file);
            mine.values |= renumber_type(entries.values, &file);
        }
        for (at, &ty) in &analysis.metatables {
            *own.metatables.entry(renumber(*at, &file)).or_default() |= renumber_type(ty, &file);
        }
        for (at, mixins) in &analysis.mixins {
            own.mixins.insert(renumber(*at, &file), mixins.iter().map(|&m| renumber_type(m, &file)).collect());
        }
        for (at, &target) in &analysis.mixed_into {
            own.mixed_into.insert(renumber(*at, &file), target);
        }
        for (at, tables) in &analysis.mixin_copies {
            own.mixin_copies.insert(renumber(*at, &file), tables.iter().map(|&t| renumber(t, &file)).collect());
        }
        for (copy, source) in &analysis.copies {
            own.copies.insert(renumber(*copy, &file), renumber(*source, &file));
        }
        for (at, &ty) in &analysis.parents {
            *own.parents.entry(renumber(*at, &file)).or_default() |= renumber_type(ty, &file);
        }
        for (&(at, event), values) in &analysis.triggered {
            let values = renumber_values(values, &file);
            own.triggered.entry((renumber(at, &file), event))
                .and_modify(|all| *all = all.union(&values))
                .or_insert(values);
        }
        for (at, &ty) in &analysis.pool_objects {
            *own.pool_objects.entry(renumber(*at, &file)).or_default() |= renumber_type(ty, &file);
        }
        for (at, name) in &analysis.table_names {
            own.table_names.insert(renumber(*at, &file), name.clone());
        }
        for (at, members) in &analysis.stand_ins {
            own.stand_ins.insert(*at, members.iter().map(|&m| renumber(m, &file)).collect());
        }
        let site = |file_id: FileId, range: TextRange| Site { file: file(file_id), range };
        for r in analysis.references.iter().filter(|r| r.write) {
            let binding = analysis.binding(r);
            if binding.kind == BindingKind::Global {
                own.global_sites.entry(binding.name.to_string()).or_default().push(site(r.file, r.range));
            }
        }
        for f in analysis.field_references.iter().filter(|f| f.write) {
            let name = f.name().to_string();
            let sites = match f.table {
                Some(at) => own.field_sites.entry(renumber(at, &file)).or_default().entry(name).or_default(),
                None => own.any_field_sites.entry(name).or_default(),
            };
            sites.push(site(f.file, f.range));
        }
        layer.add(&own);
        layer
    }

    /// Adds what `other` makes visible: both addons are loaded
    pub fn add(&mut self, other: &Layer) {
        for (at, members) in &other.stand_ins {
            let mine = self.stand_ins.entry(*at).or_default();
            for &member in members {
                if !mine.contains(&member) {
                    mine.push(member);
                }
            }
        }
        for (name, &ty) in &other.globals {
            *self.globals.entry(name.clone()).or_default() |= ty;
        }
        for (id, info) in &other.functions {
            self.functions.entry(*id).or_insert_with(|| info.clone());
        }
        for (at, fields) in &other.fields {
            let mine = self.fields.entry(*at).or_default();
            for (name, &ty) in fields {
                *mine.entry(name.clone()).or_default() |= ty;
            }
        }
        for (name, &ty) in &other.any_table_fields {
            *self.any_table_fields.entry(name.clone()).or_default() |= ty;
        }
        self.opened_types.extend(other.opened_types.iter().copied());
        self.function_fields.extend(other.function_fields.iter().cloned());
        for (at, entries) in &other.entries {
            let mine = self.entries.entry(*at).or_default();
            mine.keys |= entries.keys;
            mine.values |= entries.values;
        }
        for (at, &ty) in &other.metatables {
            *self.metatables.entry(*at).or_default() |= ty;
        }
        for (at, mixins) in &other.mixins {
            let mine = self.mixins.entry(*at).or_default();
            for mixin in mixins {
                if !mine.contains(mixin) {
                    mine.push(*mixin);
                }
            }
        }
        for (at, &target) in &other.mixed_into {
            self.mixed_into.entry(*at).or_insert(target);
        }
        for (at, tables) in &other.mixin_copies {
            self.mixin_copies.entry(*at).or_insert_with(|| tables.clone());
        }
        for (copy, source) in &other.copies {
            self.copies.entry(*copy).or_insert(*source);
        }
        for (at, &ty) in &other.parents {
            *self.parents.entry(*at).or_default() |= ty;
        }
        for (key, values) in &other.triggered {
            self.triggered.entry(*key)
                .and_modify(|all| *all = all.union(values))
                .or_insert_with(|| values.clone());
        }
        for (at, &ty) in &other.pool_objects {
            *self.pool_objects.entry(*at).or_default() |= ty;
        }
        for (at, name) in &other.table_names {
            self.table_names.entry(*at).or_insert_with(|| name.clone());
        }
        for (name, sites) in &other.global_sites {
            add_sites(self.global_sites.entry(name.clone()).or_default(), sites);
        }
        for (at, fields) in &other.field_sites {
            let mine = self.field_sites.entry(*at).or_default();
            for (name, sites) in fields {
                add_sites(mine.entry(name.clone()).or_default(), sites);
            }
        }
        for (name, sites) in &other.any_field_sites {
            add_sites(self.any_field_sites.entry(name.clone()).or_default(), sites);
        }
    }

    /// A field of a dependency's table, through its mixins and `__index`
    pub(super) fn field(&self, table: TableId, name: &str, depth: usize) -> Option<Type> {
        if let Some(&own) = self.fields.get(&table).and_then(|fields| fields.get(name)) {
            return Some(own)
        }
        if depth > 16 {
            return None
        }
        let from_mixins = self.mixins.get(&table).into_iter().flatten().rev()
            .find_map(|mixin| self.field(mixin.named.identity()?, name, depth + 1));
        from_mixins.or_else(|| {
            let metatable = self.metatables.get(&table)?.named.identity()?;
            let index = self.field(metatable, "__index", depth + 1)?;
            match index.named.identity() {
                Some(fallback) => self.field(fallback, name, depth + 1),
                None => match index.function {
                    FunctionRef::One(id) => self.functions.get(&id).map(|f| f.returns.first()),
                    _ => None,
                },
            }
        })
    }
}

/// `location` with its file renumbered by `file`
pub(super) fn renumber(location: Location, file: &impl Fn(FileId) -> FileId) -> Location {
    Location { file: file(location.file), ..location }
}

pub(super) fn renumber_named(named: NamedRef, file: &impl Fn(FileId) -> FileId) -> NamedRef {
    match named {
        NamedRef::Table(at) => NamedRef::Table(renumber(at, file)),
        NamedRef::Instance { named, at, extended } => NamedRef::Instance { named, at: renumber(at, file), extended },
        NamedRef::Several(_) => named.map_objects(|object| renumber_named(object, file)),
        NamedRef::Tables(_) => NamedRef::several_tables(named.identities().into_iter().map(|at| renumber(at, file)).collect()),
        other => other,
    }
}

pub(super) fn renumber_type(ty: Type, file: &impl Fn(FileId) -> FileId) -> Type {
    let function = ty.function.map_functions(|function| Some(match function {
        FunctionRef::One(id) => FunctionRef::One(renumber(id, file)),
        other => other,
    })).unwrap_or(FunctionRef::Unknown);
    Type { function, named: renumber_named(ty.named, file), numeric: renumber_named(ty.numeric, file), ..ty }
}

pub(super) fn renumber_values(values: &Values, file: &impl Fn(FileId) -> FileId) -> Values {
    Values { values: values.values.iter().map(|&v| renumber_type(v, file)).collect(), rest: renumber_type(values.rest, file) }
}

pub(super) fn renumber_function(info: &FunctionInfo, file: &impl Fn(FileId) -> FileId) -> FunctionInfo {
    let parameter = |p: &Parameter| Parameter { name: p.name.clone(), types: renumber_type(p.types, file), mandated: p.mandated.map(|m| renumber_type(m, file)) };
    FunctionInfo {
        parameters: info.parameters.iter().map(parameter).collect(),
        method: info.method,
        varargs: info.varargs.as_ref().map(parameter),
        returns: renumber_values(&info.returns, file),
        fresh_returns: info.fresh_returns.iter().map(|fresh| fresh.as_ref().map(|fresh| FreshObject {
            made: fresh.made.map(|made| renumber(made, file)),
            ..fresh.clone()
        })).collect(),
        calls: info.calls.iter().map(|&id| renumber(id, file)).collect(),
        forwarded_calls: info.forwarded_calls.iter().map(|call| ForwardedCall {
            arguments: call.arguments.iter().map(|&argument| renumber_forwarded(argument, file)).collect(),
            ..call.clone()
        }).collect(),
        field_writes: info.field_writes.iter().map(|write| FieldWrite {
            value: write.value.iter().map(|&part| renumber_forwarded(part, file)).collect(),
            ..write.clone()
        }).collect(),
        returned_fields: info.returned_fields.clone(),
        iterated: info.iterated.as_ref().map(|values| renumber_values(values, file)),
        only: info.only.clone(),
        loop_edges: info.loop_edges.iter().map(|edge| {
            let tables = |tables: &[TableId]| tables.iter().map(|&at| renumber(at, file)).collect();
            match edge {
                LoopEdge::ParameterToField(p, on, field) => LoopEdge::ParameterToField(*p, tables(on), field.clone()),
                LoopEdge::ReturnToField(id, on, field) => LoopEdge::ReturnToField(renumber(*id, file), tables(on), field.clone()),
                LoopEdge::FieldToParameter(on, field, id, i) => LoopEdge::FieldToParameter(tables(on), field.clone(), renumber(*id, file), *i),
                LoopEdge::ParameterToParameter(p, id, i) => LoopEdge::ParameterToParameter(*p, renumber(*id, file), *i),
            }
        }).collect(),
        returned_parameters: info.returned_parameters.clone(),
    }
}

pub(super) fn renumber_forwarded(forwarded: Forwarded, file: &impl Fn(FileId) -> FileId) -> Forwarded {
    match forwarded {
        Forwarded::Value(ty) => Forwarded::Value(renumber_type(ty, file)),
        other => other,
    }
}
