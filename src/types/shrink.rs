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

//! Which writes an edit shrinks: for two fresh analyses of an addon, one
//! file differing by one edited span, the keys some function body wrote less
//! to after it than before (or no longer wrote), and how far a reset from
//! only those keys would reach. Only a measurement, for the incremental
//! analysis design: where nothing shrinks, an analysis made from the last
//! one needs no reset at all

use super::*;
use super::reach::{cycle_reach, reach_from};

/// One edit's measurement
pub struct Shrinkage {
    /// Whether the edit is inside a function body, rather than only at a
    /// file's top level, whose writes aren't recorded
    pub in_function: bool,
    /// Function bodies compared
    pub units: usize,
    /// Keys some body wrote less to, by kind
    pub shrunk: Vec<String>,
    /// Of those, function signatures that lost something
    pub signatures: usize,
    /// Bodies a reset from the shrunk keys would walk again, and of how many
    pub reach: usize,
    pub total: usize,
    /// Keys in a cycle with a shrunk key, and the bodies reading them: what a
    /// reset needs to cover, as the rest is rebuilt from the writes
    pub cycle_keys: usize,
    pub cycle_reach: usize,
}

/// Where things in the old analysis are in the new one: `file` had the span
/// `start..old_end` replaced by text ending at `new_end`
struct Remap<'a> {
    file: FileId,
    start: usize,
    old_end: usize,
    new_end: usize,
    bindings: HashMap<(FileId, Option<TextSize>, &'a str, bool), BindingId>,
    old: &'a Analysis,
}

impl<'a> Remap<'a> {
    fn location(&self, at: Location) -> Option<Location> {
        if at.file != self.file {
            return Some(at)
        }
        let offset = usize::from(at.offset);
        if offset <= self.start {
            Some(at)
        } else if offset >= self.old_end {
            Some(Location { offset: TextSize::from((offset - self.old_end + self.new_end) as u32), ..at })
        } else {
            None
        }
    }

    fn binding(&self, id: BindingId) -> Option<BindingId> {
        let binding = self.old.bindings.get(id)?;
        let global = binding.kind == BindingKind::Global;
        let declared = match binding.declaration {
            Some(range) => Some(self.location(Location { file: binding.file, offset: range.start(), slot: 0 })?.offset),
            None => None,
        };
        let file = if global { FileId(0) } else { binding.file };
        self.bindings.get(&(file, declared, &*binding.name, global)).copied()
    }

    fn named(&self, named: NamedRef) -> Option<NamedRef> {
        Some(match named {
            NamedRef::Table(at) => NamedRef::Table(self.location(at)?),
            NamedRef::Instance { named, at, extended } => NamedRef::Instance { named, at: self.location(at)?, extended },
            NamedRef::Tables(_) => {
                let tables: Option<Vec<TableId>> = named.identities().into_iter().map(|at| self.location(at)).collect();
                NamedRef::several_tables(tables?)
            }
            NamedRef::Several(_) => {
                let objects: Option<Vec<NamedRef>> = named.alternatives().into_iter().map(|object| self.named(object)).collect();
                NamedRef::several(objects?)
            }
            other => other,
        })
    }

    fn ty(&self, ty: Type) -> Option<Type> {
        let function = match ty.function {
            FunctionRef::One(id) => FunctionRef::One(self.location(id)?),
            other => other,
        };
        Some(Type { function, named: self.named(ty.named)?, ..ty })
    }

    fn values(&self, values: &Values) -> Option<Values> {
        Some(Values {
            values: values.values.iter().map(|&v| self.ty(v)).collect::<Option<_>>()?,
            rest: self.ty(values.rest)?,
        })
    }

    fn read(&self, read: &Read) -> Option<Read> {
        Some(match read {
            Read::Binding(id) => Read::Binding(self.binding(*id)?),
            Read::Function(at) => Read::Function(self.location(*at)?),
            Read::Field(at, name) => Read::Field(self.location(*at)?, *name),
            Read::Fields(at) => Read::Fields(self.location(*at)?),
            Read::Entries(at) => Read::Entries(self.location(*at)?),
            Read::Metatable(at) => Read::Metatable(self.location(*at)?),
            Read::Mixins(at) => Read::Mixins(self.location(*at)?),
            Read::MixedInto(at) => Read::MixedInto(self.location(*at)?),
            Read::Parent(at) => Read::Parent(self.location(*at)?),
            Read::Triggered(at, event) => Read::Triggered(self.location(*at)?, *event),
            Read::PoolObject(at) => Read::PoolObject(self.location(*at)?),
            Read::Varargs(at) => Read::Varargs(self.location(*at)?),
            Read::CallCycle(at) => Read::CallCycle(self.location(*at)?),
            Read::Method(at) => Read::Method(self.location(*at)?),
            Read::SelfType(at) => Read::SelfType(self.location(*at)?),
            // A stand-in is the same identity whatever moves
            Read::StandIn(at) => Read::StandIn(*at),
            Read::ParameterTypes(at) => Read::ParameterTypes(self.location(*at)?),
            Read::Saved(at) => Read::Saved(self.location(*at)?),
            other => other.clone(),
        })
    }
}

/// What a body gave one key: a type, values, a function, or only that it
/// wrote there (for writes whose detail this measurement doesn't compare)
#[derive(Clone)]
enum Given {
    Type(Type),
    Values(Values),
    Function(FunctionInfo),
    Written,
}

/// What each write of a body gives each key
fn given(write: &Write) -> Vec<(Read, Given)> {
    let one = |given: Given| write.keys().into_iter().map(|key| (key, given.clone())).collect();
    match write {
        Write::Field(_, _, ty) | Write::AnyTableField(_, ty) | Write::Metatable(_, ty) | Write::Parent(_, ty)
            | Write::PoolObject(_, ty) | Write::Binding(_, ty) => one(Given::Type(*ty)),
        Write::Entry(_, key, value) => one(Given::Values(Values { values: vec![*key, *value], rest: Type::NIL })),
        Write::CallArguments(_, values) | Write::Triggered(_, _, values) => one(Given::Values(values.clone())),
        Write::Function(_, info) => one(Given::Function(info.clone())),
        _ => one(Given::Written),
    }
}

fn join(a: Given, b: &Given) -> Given {
    match (a, b) {
        (Given::Type(a), Given::Type(b)) => Given::Type(a | *b),
        (Given::Values(a), Given::Values(b)) => Given::Values(a.union(b)),
        (_, b) => b.clone(),
    }
}

/// Whether `old` (in the new analysis's places) is within `new`. For a
/// function: its returns, the functions it calls and its parameters' types
fn within(remap: &Remap, old: &Given, new: &Given) -> bool {
    match (old, new) {
        (Given::Type(old), Given::Type(new)) => remap.ty(*old).is_some_and(|old| old | *new == *new),
        (Given::Values(old), Given::Values(new)) => remap.values(old).is_some_and(|old| old.union(new) == *new),
        (Given::Function(old), Given::Function(new)) => function_within(remap, old, new),
        (Given::Written, _) => true,
        _ => false,
    }
}

fn function_within(remap: &Remap, old: &FunctionInfo, new: &FunctionInfo) -> bool {
    let returns = remap.values(&old.returns).is_some_and(|old| old.union(&new.returns) == new.returns);
    let calls = old.calls.iter().all(|&id| remap.location(id).is_some_and(|id| new.calls.contains(&id)));
    let parameters = old.parameters.len() <= new.parameters.len()
        && old.parameters.iter().zip(&new.parameters).all(|(old, new)| {
            remap.ty(old.types).is_some_and(|old| old | new.types == new.types)
                && old.mandated == new.mandated.or(old.mandated)
        });
    returns && calls && parameters
}

/// The keys each body gave, joined
fn gives(record: &Record) -> HashMap<Read, Given> {
    let mut all: HashMap<Read, Given> = HashMap::default();
    for write in record.own_writes.iter() {
        for (key, given) in given(write) {
            let joined = match all.remove(&key) {
                Some(before) => join(before, &given),
                None => given,
            };
            all.insert(key, joined);
        }
    }
    all
}

/// The one span `new` differs from `old` in: where it starts, and where it
/// ends in each
pub fn span(old: &str, new: &str) -> (usize, usize, usize) {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut start = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while !(old.is_char_boundary(start) && new.is_char_boundary(start)) {
        start -= 1;
    }
    let most = o.len().min(n.len()) - start;
    let mut end = o.iter().rev().zip(n.iter().rev()).take(most).take_while(|(a, b)| a == b).count();
    while !(old.is_char_boundary(o.len() - end) && new.is_char_boundary(n.len() - end)) {
        end -= 1;
    }
    (start, o.len() - end, n.len() - end)
}

/// Which writes the edit shrank, from `old` (made before it) to `new`, and
/// how far a reset from them would reach in `new`
pub fn shrinkage(old: &Analysis, new: &Analysis, file: FileId, start: usize, old_end: usize, new_end: usize, files: usize) -> Shrinkage {
    let _analysing = Analysing::with(&new.definitions);
    let bindings = new.bindings.iter().enumerate()
        .map(|(id, b)| {
            let global = b.kind == BindingKind::Global;
            ((if global { FileId(0) } else { b.file }, b.declaration.map(|d| d.start()), &*b.name, global), id)
        })
        .collect();
    let remap = Remap { file, start, old_end, new_end, bindings, old };
    let in_function = new.roots.get(file.0 as usize).is_some_and(|green| {
        let root = SyntaxNode::new_root(green.clone());
        let offset = TextSize::from(start.min(usize::from(root.text_range().end())) as u32);
        root.token_at_offset(offset).left_biased().is_some_and(|token| {
            token.parent_ancestors().any(|node| node.kind() == SyntaxKind::FunctionDefinition)
        })
    });
    let mut shrunk: HashSet<Read> = HashSet::default();
    let mut signatures = 0;
    let mut units = 0;
    for (id, record) in old.records.iter().filter(|(id, _)| (id.file.0 as usize) < files) {
        units += 1;
        let before = gives(record);
        let after = remap.location(*id).and_then(|id| new.records.get(&id)).map(|record| gives(record)).unwrap_or_default();
        for (key, given) in &before {
            let Some(key) = remap.read(key) else { continue };
            let kept = after.get(&key).is_some_and(|now| within(&remap, given, now));
            if !kept {
                if matches!(key, Read::Function(_)) {
                    signatures += 1;
                }
                shrunk.insert(key);
            }
        }
        // The bindings it declares: what closures inside it read of them
        for old_id in record.bindings_start..record.bindings_start + record.bindings.len() {
            let Some(new_id) = remap.binding(old_id) else { continue };
            let (Some(was), Some(now)) = (old.bindings.get(old_id), new.bindings.get(new_id)) else { continue };
            if !remap.ty(was.types).is_some_and(|was| was | now.types == now.types) {
                shrunk.insert(Read::Binding(new_id));
            }
        }
    }
    let seeds: Vec<Read> = shrunk.iter().cloned().collect();
    let (reach, total) = reach_from(new, &seeds, files);
    let (cycle_keys, cycle_reach) = cycle_reach(new, &seeds, files);
    let mut kinds: Vec<String> = shrunk.iter().map(|key| format!("{key:?}").split('(').next().unwrap_or_default().to_string()).collect();
    kinds.sort();
    Shrinkage { in_function, units, shrunk: kinds, signatures, reach, total, cycle_keys, cycle_reach }
}
