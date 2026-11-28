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

//! How values are represented: [`TypeSet`] bits, the functions, tables and
//! named objects a value can be, string literals, and table identities

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct TypeSet(u8);

impl TypeSet {
    pub const NEVER: TypeSet = TypeSet(0);
    pub const NIL: TypeSet = TypeSet(1 << 0);
    pub const FALSE: TypeSet = TypeSet(1 << 1);
    pub const TRUE: TypeSet = TypeSet(1 << 2);
    pub const NUMBER: TypeSet = TypeSet(1 << 3);
    pub const STRING: TypeSet = TypeSet(1 << 4);
    pub const FUNCTION: TypeSet = TypeSet(1 << 5);
    pub const TABLE: TypeSet = TypeSet(1 << 6);
    /// userdata and thread
    pub const OTHER: TypeSet = TypeSet(1 << 7);

    pub const BOOLEAN: TypeSet = TypeSet(Self::FALSE.0 | Self::TRUE.0);
    pub const UNKNOWN: TypeSet = TypeSet(u8::MAX);

    pub fn union(self, other: TypeSet) -> TypeSet {
        TypeSet(self.0 | other.0)
    }
    pub fn without(self, other: TypeSet) -> TypeSet {
        TypeSet(self.0 & !other.0)
    }
    pub fn contains(self, other: TypeSet) -> bool {
        self.0 & other.0 == other.0
    }
    /// The values that let `a or b` return `a`, and `a and b` continue to `b`
    pub fn truthy(self) -> TypeSet {
        self.without(Self::NIL.union(Self::FALSE))
    }
    pub fn intersection(self, other: TypeSet) -> TypeSet {
        TypeSet(self.0 & other.0)
    }
    /// The values that let `a and b` return `a`
    pub fn falsy(self) -> TypeSet {
        TypeSet(self.0 & (Self::NIL.0 | Self::FALSE.0))
    }
}

impl std::ops::BitOr for TypeSet {
    type Output = TypeSet;
    fn bitor(self, rhs: TypeSet) -> TypeSet {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for TypeSet {
    fn bitor_assign(&mut self, rhs: TypeSet) {
        *self = self.union(rhs);
    }
}

impl TypeSet {
    /// One name per possibility, e.g. ["number", "nil"], or a single "unknown…"
    pub fn names(self) -> Vec<&'static str> {
        let all_values = Self::UNKNOWN.truthy();
        if self.contains(all_values) {
            return vec![match (self.contains(Self::NIL), self.contains(Self::FALSE)) {
                (true, true) => "unknown",
                (false, true) => "unknown (not nil)",
                (true, false) => "unknown (not false)",
                (false, false) => "unknown (truthy)",
            }];
        }
        if self == Self::NEVER {
            return vec!["never"];
        }
        let mut names = Vec::new();
        if self.contains(Self::NUMBER) { names.push("number") }
        if self.contains(Self::STRING) { names.push("string") }
        if self.contains(Self::BOOLEAN) {
            names.push("boolean")
        } else if self.contains(Self::TRUE) {
            names.push("true")
        } else if self.contains(Self::FALSE) {
            names.push("false")
        }
        if self.contains(Self::FUNCTION) { names.push("function") }
        if self.contains(Self::TABLE) { names.push("table") }
        if self.contains(Self::OTHER) { names.push("userdata") }
        if self.contains(Self::NIL) { names.push("nil") }
        names
    }
}

impl fmt::Display for TypeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.names().join("|"))
    }
}

/// Identifies a function by where its definition starts, which is the same
/// across walks and loop passes
pub type FunctionId = Location;

/// One file of an analysis, by its position in the list analysed
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FileId(pub u32);

/// A position in one of the files analysed. Identifies functions and tables
/// the same way across walks
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Location {
    pub file: FileId,
    pub offset: TextSize,
    /// Tells apart several things identified by one position: 0 for something
    /// written there, and 1 + n for the nth object a call there returns new.
    /// With `POOL_OBJECT` added, the objects of the pool that is
    pub slot: u32,
}

/// Added to a pool's slot for the slot of the one object standing for all the
/// pool's objects
pub(super) const POOL_OBJECT: u32 = 1 << 31;

/// Added to the slot of a pseudo identity: one a variable holding a table of
/// unknown origin is given where it's declared
pub(super) const PSEUDO: u32 = 1 << 30;

/// Added to the slot of a table a deep copy makes inside the copy a
/// `CopyTable` call makes, with the rest of the slot a hash of where in it
/// that table is, so it's the same on every walk
pub(super) const COPY: u32 = 1 << 29;

/// Added to the slot of a table one of the definitions' functions returns new
/// at a call (see `FreshObject`), with the function's id above `FAMILY_SHIFT`
/// and the return's slot below. Every table that function makes at that
/// return is of one family, which stands for any of them (see `family`)
pub(super) const FAMILY_MEMBER: u32 = 1 << 28;
pub(super) const FAMILY_SHIFT: u32 = 8;
/// The most functions of the definitions whose tables have a family
pub(super) const MOST_FAMILIES: u32 = 1 << 20;

/// The pseudo file of families' identities: past any file's and those of
/// dependencies (`LAYER_FILES`), and before those of namespaces
pub(super) const FAMILIES: FileId = FileId(u32::MAX - (1 << 20));

/// The pseudo file of stand-ins: one identity for each place that holds
/// more tables made in the files than are kept apart (`MOST_OBJECTS`), its
/// slot the place's number (see `stand_in`). What it stands for, its
/// members, is in `Analysis::stand_ins`. Just before families'
pub(super) const STAND_INS: FileId = FileId(u32::MAX - (1 << 20) - 1);

/// Somewhere a value is kept between statements or walks, which a set of
/// tables too large to keep apart is replaced in by its stand-in
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Place {
    /// A local or parameter, by its declaration
    Binding(Location),
    /// A global, by its name's hash
    Global(u64),
    /// A table's field, by its name's hash
    Field(TableId, u64),
    /// What a table holds under computed keys
    Entries(TableId),
    EntryKeys(TableId),
    /// A field of tables of unknown identity, by its name's hash
    AnyField(u64),
    /// A function's return, by position
    Return(FunctionId, usize),
    /// A function's parameter, by position
    Parameter(FunctionId, usize),
}

/// Each place's number, shared by every analysis, so a stand-in is the same
/// identity on every walk
pub(super) static PLACES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<Place, u32>>> = std::sync::LazyLock::new(Default::default);

/// The stand-in for the tables at `place`
pub(super) fn stand_in(place: Place) -> TableId {
    let mut places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
    let next = places.len() as u32;
    let slot = *places.entry(place).or_insert(next);
    Location { file: STAND_INS, offset: TextSize::new(0), slot }
}

/// The stand-in for the tables at `place`, if it has one yet
pub(super) fn existing_stand_in(place: Place) -> Option<TableId> {
    let places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
    places.get(&place).map(|&slot| Location { file: STAND_INS, offset: TextSize::new(0), slot })
}

pub(super) fn is_stand_in(at: TableId) -> bool {
    at.file == STAND_INS
}

/// `ty`, to be kept at `place`: if it's more tables than are kept apart,
/// the place's stand-in, noted in `stand_ins` as standing for them. With
/// the stand-in and the tables it stands for that it didn't before
pub(super) fn kept_at(stand_ins: &mut HashMap<TableId, Vec<TableId>>, place: Place, ty: Type) -> (Type, Option<(TableId, Vec<TableId>)>) {
    if !matches!(ty.named, NamedRef::Table(_) | NamedRef::Tables(_)) {
        return (ty, None)
    }
    // Once a place has a stand-in, whatever tables are kept there are it:
    // what's kept only grows, so it would have been anyway, and the same
    // tables are always written the same way
    let at = match (many_tables(ty.named), existing_stand_in(place)) {
        (_, Some(at)) if stand_ins.contains_key(&at) => at,
        (true, _) => stand_in(place),
        (false, _) => return (ty, None),
    };
    let known = stand_ins.entry(at).or_default();
    let mut added: Vec<TableId> = ty.named.identities().into_iter().filter(|&m| m != at && known.binary_search(&m).is_err()).collect();
    added.sort();
    added.dedup();
    known.extend(added.iter().copied());
    known.sort();
    let added = (!added.is_empty()).then_some((at, added));
    (Type { named: NamedRef::Table(at), ..ty }, added)
}

/// The union of `types`, with the tables of all of them joined into one set
/// at once rather than one at a time: for joining what many tables hold
pub(super) fn union_all(types: impl IntoIterator<Item = Type>) -> Option<Type> {
    let mut tables: Vec<TableId> = Vec::new();
    let mut joined: Option<Type> = None;
    for ty in types {
        let rest = match ty.named {
            NamedRef::Table(_) | NamedRef::Tables(_) => {
                tables.extend(ty.named.identities());
                Type { named: NamedRef::None, ..ty }
            }
            _ => ty,
        };
        joined = Some(joined.map_or(rest, |joined| joined | rest));
    }
    let mut joined = joined?;
    if !tables.is_empty() {
        tables.sort();
        tables.dedup();
        let named = match tables.as_slice() {
            [one] => NamedRef::Table(*one),
            _ => NamedRef::several_tables(tables),
        };
        joined |= Type { named, ..Type::default() };
    }
    Some(joined)
}

/// How many tables a type holds, without looking past a few
pub(super) fn many_tables(named: NamedRef) -> bool {
    match named {
        NamedRef::Tables(id) => object_set(id).len() > MOST_OBJECTS,
        _ => false,
    }
}

/// The identity of a new table the definitions' function `function` returns
/// at a call at `at`, in return position `slot`, if it can have one
pub(super) fn family_member(at: Location, function: BuiltinId, slot: usize) -> Option<Location> {
    let function = Some(function).filter(|&f| f < MOST_FAMILIES)?;
    let slot = u32::try_from(slot).ok().filter(|&s| s + 1 < 1 << FAMILY_SHIFT)?;
    Some(Location { slot: FAMILY_MEMBER | function << FAMILY_SHIFT | (slot + 1), ..at })
}

/// For a table one of the definitions' functions made, or that function's
/// family itself, the family: one identity for every table it makes there,
/// which a union of several of them is, holding what each was given. Other
/// tables have none
pub(super) fn family(at: Location) -> Option<Location> {
    if at.file == FAMILIES {
        return Some(at)
    }
    if at.slot & (POOL_OBJECT | PSEUDO | COPY) != 0 || at.slot & FAMILY_MEMBER == 0 {
        return None
    }
    Some(Location { file: FAMILIES, offset: TextSize::new(0), slot: at.slot })
}

/// Added to the slot of a table written directly inside another table's
/// constructor, as `{ A = {...}, B = {...} }`'s records are, with the
/// outer `{`'s offset below it. Those of one constructor are its elements
/// (see `elements_of`). Only for tables none of the flags above marks
pub(super) const ELEMENT: u32 = 1 << 27;
/// The slot of the identity standing for every element of one constructor
pub(super) const ELEMENTS: u32 = ELEMENT | (ELEMENT - 1);
/// The slot of the identity holding what's written through `ELEMENTS`'s,
/// which each element may then have
pub(super) const THROUGH_ELEMENTS: u32 = ELEMENT | (ELEMENT - 2);

/// For a table written directly inside a constructor, the identity of all
/// that constructor's elements: one table for any of them, holding what
/// each was given. A union of more tables than are kept apart is it, so
/// a loop over a long list of records still sees their fields
pub(super) fn elements_of(at: Location) -> Option<Location> {
    let flags = at.slot & (POOL_OBJECT | PSEUDO | COPY | FAMILY_MEMBER);
    if flags != 0 || at.slot & ELEMENT == 0 || at.slot == ELEMENTS || at.slot == THROUGH_ELEMENTS {
        return None
    }
    Some(Location { offset: TextSize::new(at.slot & (ELEMENT - 1)), slot: ELEMENTS, ..at })
}

/// For the identity of a constructor's elements, the one holding what's
/// written through it (see `THROUGH_ELEMENTS`)
pub(super) fn through_elements(at: Location) -> Option<Location> {
    (at.slot == ELEMENTS && at.slot & (POOL_OBJECT | PSEUDO | COPY | FAMILY_MEMBER) == 0)
        .then_some(Location { slot: THROUGH_ELEMENTS, ..at })
}

/// Which function a value is, when it can be one
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum FunctionRef {
    /// Can't be a function
    #[default]
    None,
    /// Exactly this function
    One(FunctionId),
    /// A function from the preset definitions
    Builtin(BuiltinId),
    /// Any of several functions (`One` or `Builtin`), as `cond and f or g`
    /// is: an index into `FUNCTION_SETS`. Calling it calls each
    Several(u32),
    /// A function we can't pin down: a parameter, an unknown call result, or
    /// one of more functions than `MOST_FUNCTIONS`
    Unknown,
}

/// The most functions `FunctionRef::Several` keeps apart. More are unknown
pub(super) const MOST_FUNCTIONS: usize = 8;

/// Each list of functions a `FunctionRef::Several` stands for, sorted, once
/// each. Shared by every analysis, like `OBJECT_SETS`
#[derive(Default)]
pub(super) struct FunctionSets {
    pub(super) lists: Vec<&'static [FunctionRef]>,
    pub(super) ids: std::collections::HashMap<&'static [FunctionRef], u32>,
}

pub(super) static FUNCTION_SETS: std::sync::LazyLock<std::sync::Mutex<FunctionSets>> = std::sync::LazyLock::new(Default::default);

impl FunctionRef {
    pub(super) fn union(self, other: FunctionRef) -> FunctionRef {
        match (self, other) {
            (FunctionRef::None, x) | (x, FunctionRef::None) => x,
            (a, b) if a == b => a,
            (FunctionRef::Unknown, _) | (_, FunctionRef::Unknown) => FunctionRef::Unknown,
            (a, b) => {
                let mut all = a.alternatives();
                all.extend(b.alternatives());
                FunctionRef::several(all)
            }
        }
    }

    /// Each function this may be: itself, or those of `Several`. None for
    /// `None` and `Unknown`
    pub fn alternatives(self) -> Vec<FunctionRef> {
        match self {
            FunctionRef::One(_) | FunctionRef::Builtin(_) => vec![self],
            FunctionRef::Several(id) => {
                let sets = FUNCTION_SETS.lock().unwrap_or_else(|e| e.into_inner());
                sets.lists.get(id as usize).map(|list| list.to_vec()).unwrap_or_default()
            }
            FunctionRef::None | FunctionRef::Unknown => Vec::new(),
        }
    }

    /// Any of `functions` (each `One` or `Builtin`): one, or several, each
    /// once, or unknown past `MOST_FUNCTIONS`
    pub fn several(mut functions: Vec<FunctionRef>) -> FunctionRef {
        functions.sort();
        functions.dedup();
        match functions.len() {
            0 => return FunctionRef::None,
            1 => return functions[0],
            n if n > MOST_FUNCTIONS => return FunctionRef::Unknown,
            _ => {}
        }
        let mut sets = FUNCTION_SETS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = sets.ids.get(functions.as_slice()) {
            return FunctionRef::Several(id)
        }
        let list: &'static [FunctionRef] = Box::leak(functions.into_boxed_slice());
        let id = sets.lists.len() as u32;
        sets.lists.push(list);
        sets.ids.insert(list, id);
        FunctionRef::Several(id)
    }

    /// Each function this may be changed by `change`, which may give none
    /// for one, making the whole none
    pub(super) fn map_functions(self, change: impl Fn(FunctionRef) -> Option<FunctionRef>) -> Option<FunctionRef> {
        match self {
            FunctionRef::Several(_) => Some(FunctionRef::several(self.alternatives().into_iter().map(change).collect::<Option<_>>()?)),
            other => change(other),
        }
    }
}

/// Which named type (`Frame`, `ItemKey`, ...) a table or userdata value is
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum NamedRef {
    /// Can't be a table or userdata
    #[default]
    None,
    One(NamedId),
    /// A named type that may have members beyond its documented ones, like a
    /// frame made from a template (whose Lua mixin adds methods). Missing
    /// methods aren't reported on these
    Extended(NamedId),
    /// A WoW object made in the files, by `CreateFrame` or a method like
    /// `CreateTexture`: its type, where it was made (its identity, for its own
    /// fields and its parent), and whether a template may have added members.
    /// Shown as its type
    Instance { named: NamedId, at: TableId, extended: bool },
    /// A table made in this file: its identity, for tracking its fields. Shown
    /// as a plain table
    Table(TableId),
    /// Any of several WoW objects (`One`, `Extended` or `Instance`), as a
    /// variable given different frames holds: an index into `OBJECT_SETS`.
    /// It has the members all of them have. With at least one of those, it
    /// may also hold tables made in the files (`Table`), as `frame or {}` is
    Several(u32),
    /// Any of several tables made in the files (`Table`), as a field reset
    /// to a new table holds: an index into `OBJECT_SETS`. Shown as a plain
    /// table. What's written to it may be in any of them, and what's read is
    /// what any of them has
    Tables(u32),
    /// A plain table, or one of several named types
    Unknown,
}

/// The most objects `NamedRef::Several` keeps apart. More are joined as the
/// closest type they all are
pub(super) const MOST_OBJECTS: usize = 8;

/// Each list of objects a `NamedRef::Several` stands for, sorted, once each.
/// Shared by every analysis, so a dependency's types mean the same in its
/// dependents'
#[derive(Default)]
pub(super) struct ObjectSets {
    pub(super) lists: Vec<&'static [NamedRef]>,
    pub(super) ids: std::collections::HashMap<&'static [NamedRef], u32>,
}

pub(super) static OBJECT_SETS: std::sync::LazyLock<std::sync::Mutex<ObjectSets>> = std::sync::LazyLock::new(Default::default);

pub(super) fn object_set(id: u32) -> &'static [NamedRef] {
    let sets = OBJECT_SETS.lock().unwrap_or_else(|e| e.into_inner());
    sets.lists.get(id as usize).copied().unwrap_or_default()
}

thread_local! {
    /// The definitions of the analysis running on this thread, for joining
    /// named types (see `NamedRef::union`)
    static ANALYSING: std::cell::RefCell<Option<Arc<Definitions>>> = const { std::cell::RefCell::new(None) };
}

/// Makes `definitions` those `NamedRef::union` joins types by on this
/// thread, until it's dropped
pub(super) struct Analysing(Option<Arc<Definitions>>);

impl Analysing {
    pub(super) fn with(definitions: &Arc<Definitions>) -> Analysing {
        Analysing(ANALYSING.with(|current| current.replace(Some(definitions.clone()))))
    }
}

impl Drop for Analysing {
    fn drop(&mut self) {
        ANALYSING.with(|current| *current.borrow_mut() = self.0.take());
    }
}

/// The closest type named types `a` and `b` both are, by the definitions of
/// the analysis running. The same whichever is given first
pub(super) fn common_object_type(a: NamedId, b: NamedId) -> Option<NamedId> {
    let (a, b) = (a.min(b), a.max(b));
    ANALYSING.with(|current| current.borrow().as_ref()?.common_ancestor(a, b))
}

/// Where a table was made (its `{`), or where the variable given a pseudo
/// identity was declared. The same across walks
pub type TableId = Location;

impl NamedRef {
    /// Joined, keeping different WoW objects apart (see `Several`)
    pub(super) fn union(self, other: NamedRef) -> NamedRef {
        match (self, other) {
            (NamedRef::None, x) | (x, NamedRef::None) => x,
            (a, b) if a == b => a,
            // Tables of one family: its identity
            (a, b) if a.in_family().is_some() && a.in_family() == b.in_family() => a.in_family().unwrap(),
            (a, b) => match (a.objects(), b.objects()) {
                (Some(mut all), Some(more)) => {
                    all.extend(more);
                    NamedRef::several(all)
                }
                _ => match (a.tables(), b.tables()) {
                    (Some(mut all), Some(more)) => {
                        all.extend(more);
                        NamedRef::several_tables(all)
                    }
                    // WoW objects and tables made in the files: each
                    _ => match (a.members(), b.members()) {
                        (Some(mut all), Some(more)) => {
                            all.extend(more);
                            NamedRef::several(all)
                        }
                        _ => NamedRef::Unknown,
                    },
                },
            },
        }
    }

    /// The tables made in the files this may be, if it's only those
    pub(super) fn tables(self) -> Option<Vec<TableId>> {
        match self {
            NamedRef::Table(at) => Some(vec![at]),
            NamedRef::Tables(id) => Some(object_set(id).iter().filter_map(|t| t.identity()).collect()),
            _ => None,
        }
    }

    /// Any of the tables `tables`: one, or several, each once. Those of one
    /// family are its identity. Past `MOST_OBJECTS`, those written in one
    /// constructor are its elements' (see `elements_of`), and past that
    /// again none is known
    pub(super) fn several_tables(tables: Vec<TableId>) -> NamedRef {
        let mut joined: Vec<NamedRef> = tables.iter()
            .map(|&at| match family(at) {
                Some(f) if tables.iter().any(|&other| other != at && family(other) == Some(f)) => NamedRef::Table(f),
                _ => NamedRef::Table(at),
            })
            .collect();
        joined.sort();
        joined.dedup();
        // Too many to keep apart: those written in one constructor are any
        // of its elements
        if joined.len() > MOST_OBJECTS {
            for table in &mut joined {
                if let Some(elements) = table.identity().and_then(elements_of) {
                    *table = NamedRef::Table(elements);
                }
            }
            joined.sort();
            joined.dedup();
        }
        // However many: where it's kept, more than are kept apart are
        // replaced by a stand-in (see `Walker::stored`)
        if joined.len() == 1 {
            return joined[0]
        }
        let mut sets = OBJECT_SETS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = sets.ids.get(joined.as_slice()) {
            return NamedRef::Tables(id)
        }
        let list: &'static [NamedRef] = Box::leak(joined.into_boxed_slice());
        let id = sets.lists.len() as u32;
        sets.lists.push(list);
        sets.ids.insert(list, id);
        NamedRef::Tables(id)
    }

    /// Every table or object made in the files this may be: its identity, or
    /// each of `Tables`
    pub fn identities(self) -> Vec<TableId> {
        match self {
            NamedRef::Tables(_) => self.tables().unwrap_or_default(),
            other => other.identity().into_iter().collect(),
        }
    }

    /// For a table or object of a family (see `family`), the family's, as
    /// it's shown when it may be any of them
    pub(super) fn in_family(self) -> Option<NamedRef> {
        match self {
            NamedRef::Table(at) => Some(NamedRef::Table(family(at)?)),
            // Whether one may have members beyond its type's isn't up to the
            // call that made it
            NamedRef::Instance { named, at, extended } => Some(NamedRef::Instance { named, at: family(at)?, extended }),
            _ => None,
        }
    }

    /// The WoW objects and tables made in the files this may be, if it's
    /// only those: tables as `Table`
    pub(super) fn members(self) -> Option<Vec<NamedRef>> {
        self.objects().or_else(|| Some(self.tables()?.into_iter().map(NamedRef::Table).collect()))
    }

    /// The objects this may be, if it's only WoW objects (and, with some,
    /// tables made in the files)
    pub(super) fn objects(self) -> Option<Vec<NamedRef>> {
        match self {
            NamedRef::One(_) | NamedRef::Extended(_) | NamedRef::Instance { .. } => Some(vec![self]),
            NamedRef::Several(id) => Some(object_set(id).to_vec()),
            _ => None,
        }
    }

    /// Each WoW object this may be: itself, or those of `Several`. None for
    /// anything else
    pub fn alternatives(self) -> Vec<NamedRef> {
        self.objects().unwrap_or_default()
    }

    /// Any of `objects`: one, or several, each once. The same object, or the
    /// same type of unknown object, is joined, as is any number past
    /// `MOST_OBJECTS`, to the closest type they all are
    pub(super) fn several(mut objects: Vec<NamedRef>) -> NamedRef {
        // Several of a family are its identity
        let families: Vec<Option<NamedRef>> = objects.iter().map(|o| o.in_family()).collect();
        for (i, object) in objects.iter_mut().enumerate() {
            if let Some(family) = families[i]
                && families.iter().enumerate().any(|(j, f)| j != i && *f == Some(family)) {
                *object = family;
            }
        }
        objects.sort();
        let mut joined: Vec<NamedRef> = Vec::with_capacity(objects.len());
        for object in objects {
            let same = match (joined.last().copied(), object) {
                (Some(last), object) if last == object => Some(last),
                (Some(NamedRef::Instance { named, at, extended: e }), NamedRef::Instance { named: n, at: a, extended: f }) if named == n && at == a => {
                    Some(NamedRef::Instance { named, at, extended: e || f })
                }
                (Some(NamedRef::One(a) | NamedRef::Extended(a)), NamedRef::Extended(b)) if a == b => Some(NamedRef::Extended(a)),
                _ => None,
            };
            match same {
                Some(same) => *joined.last_mut().unwrap() = same,
                None => joined.push(object),
            }
        }
        if joined.len() == 1 {
            return joined[0]
        }
        // Only tables, as mapping each of several may leave
        if joined.iter().all(|o| matches!(o, NamedRef::Table(_))) {
            return NamedRef::several_tables(joined.iter().filter_map(|o| o.identity()).collect())
        }
        if joined.len() > MOST_OBJECTS {
            // A plain table isn't of any WoW type
            if joined.iter().any(|o| matches!(o, NamedRef::Table(_))) {
                return NamedRef::Unknown
            }
            let mut types = joined.iter().filter_map(|o| o.object_type()).map(|(id, _)| id);
            let first = types.next();
            return match types.try_fold(first, |common, id| Some(common_object_type(common?, id))).flatten() {
                Some(common) => NamedRef::Extended(common),
                None => NamedRef::Unknown,
            }
        }
        let mut sets = OBJECT_SETS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = sets.ids.get(joined.as_slice()) {
            return NamedRef::Several(id)
        }
        let list: &'static [NamedRef] = Box::leak(joined.into_boxed_slice());
        let id = sets.lists.len() as u32;
        sets.lists.push(list);
        sets.ids.insert(list, id);
        NamedRef::Several(id)
    }

    /// Joined as a named number type (an enum): one, or none known
    pub(super) fn union_numeric(self, other: NamedRef) -> NamedRef {
        match (self, other) {
            (NamedRef::None, x) | (x, NamedRef::None) => x,
            (a, b) if a == b => a,
            (a, b) => match (a.object_type(), b.object_type()) {
                (Some((a, e)), Some((b, f))) if a == b => if e || f { NamedRef::Extended(a) } else { NamedRef::One(a) },
                // Objects of different types are of the closest type both are
                // (a frame made from a template and a plain one are frames),
                // though either may have members beyond it
                (Some((a, _)), Some((b, _))) => match common_object_type(a, b) {
                    Some(common) => NamedRef::Extended(common),
                    None => NamedRef::Unknown,
                },
                _ => NamedRef::Unknown,
            },
        }
    }

    /// The same, but possibly with members beyond its type's
    pub(super) fn loosened(self) -> NamedRef {
        match self {
            NamedRef::One(id) => NamedRef::Extended(id),
            NamedRef::Instance { named, at, .. } => NamedRef::Instance { named, at, extended: true },
            NamedRef::Several(_) => NamedRef::several(self.alternatives().into_iter().map(NamedRef::loosened).collect()),
            other => other,
        }
    }

    /// Each object this may be changed by `change`
    pub(super) fn map_objects(self, change: impl Fn(NamedRef) -> NamedRef) -> NamedRef {
        match self {
            NamedRef::Several(_) => NamedRef::several(self.alternatives().into_iter().map(change).collect()),
            other => change(other),
        }
    }

    /// The named type, if it's one
    pub fn named_id(self) -> Option<NamedId> {
        self.object_type().map(|(id, _)| id)
    }

    /// The named type, and whether it may have members beyond the type's
    pub(super) fn object_type(self) -> Option<(NamedId, bool)> {
        match self {
            NamedRef::One(id) => Some((id, false)),
            NamedRef::Extended(id) => Some((id, true)),
            NamedRef::Instance { named, extended, .. } => Some((named, extended)),
            _ => None,
        }
    }

    /// Where the table or object was made, if it's one made in the files
    pub fn identity(self) -> Option<TableId> {
        match self {
            NamedRef::Table(at) | NamedRef::Instance { at, .. } => Some(at),
            _ => None,
        }
    }
}

/// A value's possible types, which function it is if it can be one, which
/// named type if it can be a table or userdata, and which named number type
/// (an enum like `Enum.ItemQuality`) if it can be a number. `function` is None
/// exactly when `set` doesn't contain FUNCTION, `named` exactly when it has
/// neither TABLE nor OTHER, and `numeric` exactly when it has no NUMBER.
/// `numeric` is Unknown for a plain number. `literal` is which strings it
/// may be, when it may be one: None without STRING, Unknown for any string
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Type {
    pub set: TypeSet,
    pub function: FunctionRef,
    pub named: NamedRef,
    pub numeric: NamedRef,
    pub literal: Literals,
}

/// Which strings a value may be, when it may be one
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum Literals {
    /// Can't be a string
    #[default]
    None,
    /// One of these, written in the files as literals: an index into
    /// `LITERAL_SETS`
    Some(u32),
    /// Any string
    Unknown,
}

/// The most strings `Literals::Some` keeps apart. More are any string
pub(super) const MOST_LITERALS: usize = 8;

/// The longest string kept as a literal. A longer one is just a string,
/// like a message, which says little more written out
pub(super) const LONGEST_LITERAL: usize = 64;

/// Each list of strings a `Literals::Some` stands for, sorted, once each.
/// Shared by every analysis, like `OBJECT_SETS`
#[derive(Default)]
pub(super) struct LiteralSets {
    pub(super) lists: Vec<&'static [&'static str]>,
    pub(super) ids: std::collections::HashMap<&'static [&'static str], u32>,
    pub(super) strings: std::collections::HashSet<&'static str>,
}

pub(super) static LITERAL_SETS: std::sync::LazyLock<std::sync::Mutex<LiteralSets>> = std::sync::LazyLock::new(Default::default);

impl Literals {
    /// Exactly the string `text` is, from a literal's source (with its
    /// quotes), if it's one kept apart: quoted, short, with no escapes,
    /// so two spelled the same are the same string. Else any string
    pub fn of_literal(source: &str) -> Literals {
        let source = source.trim();
        let Some(quote) = source.chars().next().filter(|&c| c == '"' || c == '\'') else { return Literals::Unknown };
        let Some(text) = source.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) else { return Literals::Unknown };
        if text.contains('\\') || text.len() > LONGEST_LITERAL {
            return Literals::Unknown
        }
        Literals::of(vec![text])
    }

    /// Any of `strings`, or any string past `MOST_LITERALS`
    pub(super) fn of(strings: Vec<&str>) -> Literals {
        let mut sets = LITERAL_SETS.lock().unwrap_or_else(|e| e.into_inner());
        Literals::of_in(&mut sets, strings)
    }

    pub(super) fn of_in(sets: &mut LiteralSets, mut strings: Vec<&str>) -> Literals {
        strings.sort();
        strings.dedup();
        if strings.len() > MOST_LITERALS {
            return Literals::Unknown
        }
        if strings.is_empty() {
            return Literals::None
        }
        let strings: Vec<&'static str> = strings.into_iter().map(|text| match sets.strings.get(text) {
            Some(&kept) => kept,
            None => {
                let kept: &'static str = Box::leak(text.to_string().into_boxed_str());
                sets.strings.insert(kept);
                kept
            }
        }).collect();
        if let Some(&id) = sets.ids.get(strings.as_slice()) {
            return Literals::Some(id)
        }
        let list: &'static [&'static str] = Box::leak(strings.into_boxed_slice());
        let id = sets.lists.len() as u32;
        sets.lists.push(list);
        sets.ids.insert(list, id);
        Literals::Some(id)
    }

    /// The strings this may be, if it's only some
    pub fn strings(self) -> Option<&'static [&'static str]> {
        let Literals::Some(id) = self else { return None };
        let sets = LITERAL_SETS.lock().unwrap_or_else(|e| e.into_inner());
        sets.lists.get(id as usize).copied()
    }

    pub(super) fn union(self, other: Literals) -> Literals {
        match (self, other) {
            (Literals::None, x) | (x, Literals::None) => x,
            (a, b) if a == b => a,
            (Literals::Some(a), Literals::Some(b)) => {
                let mut sets = LITERAL_SETS.lock().unwrap_or_else(|e| e.into_inner());
                let list = |id: u32| sets.lists.get(id as usize).copied().unwrap_or_default();
                let all: Vec<&str> = list(a).iter().chain(list(b)).copied().collect();
                Literals::of_in(&mut sets, all)
            }
            _ => Literals::Unknown,
        }
    }

    /// Without the string `text`: none left if it was the only one
    pub(super) fn without(self, text: &str) -> Literals {
        match self.strings() {
            Some(strings) => Literals::of(strings.iter().copied().filter(|&s| s != text).collect()),
            None => self,
        }
    }
}

/// The types a named type can be based on
pub(super) const OBJECTS: TypeSet = TypeSet(TypeSet::TABLE.0 | TypeSet::OTHER.0);

impl Type {
    pub const NIL: Type = Type { set: TypeSet::NIL, function: FunctionRef::None, named: NamedRef::None, numeric: NamedRef::None, literal: Literals::None };
    pub const UNKNOWN: Type = Type {
        set: TypeSet::UNKNOWN,
        function: FunctionRef::Unknown,
        named: NamedRef::Unknown,
        numeric: NamedRef::Unknown,
        literal: Literals::Unknown,
    };

    /// A type with no known function or named-type identity
    pub fn of(set: TypeSet) -> Type {
        let function = if set.contains(TypeSet::FUNCTION) { FunctionRef::Unknown } else { FunctionRef::None };
        let named = if set.intersection(OBJECTS) != TypeSet::NEVER { NamedRef::Unknown } else { NamedRef::None };
        let numeric = if set.contains(TypeSet::NUMBER) { NamedRef::Unknown } else { NamedRef::None };
        let literal = if set.contains(TypeSet::STRING) { Literals::Unknown } else { Literals::None };
        Type { set, function, named, numeric, literal }
    }
    /// A string written as the literal `source` (with its quotes)
    pub fn string_literal(source: &str) -> Type {
        Type { literal: Literals::of_literal(source), ..Type::of(TypeSet::STRING) }
    }
    pub fn function(id: FunctionId) -> Type {
        Type { function: FunctionRef::One(id), ..Type::of(TypeSet::FUNCTION) }
    }
    pub fn builtin(id: BuiltinId) -> Type {
        Type { function: FunctionRef::Builtin(id), ..Type::of(TypeSet::FUNCTION) }
    }
    /// A table made in the file
    pub fn table(id: TableId) -> Type {
        Type { named: NamedRef::Table(id), ..Type::of(TypeSet::TABLE) }
    }
    /// A value of a named type: a table or userdata, or a number for an enum
    pub fn named(id: NamedId, base: TypeSet) -> Type {
        if base == TypeSet::NUMBER {
            Type { numeric: NamedRef::One(id), ..Type::of(base) }
        } else {
            Type { named: NamedRef::One(id), ..Type::of(base) }
        }
    }
    pub fn union(self, other: Type) -> Type {
        Type {
            set: self.set | other.set,
            function: self.function.union(other.function),
            named: self.named.union(other.named),
            numeric: self.numeric.union_numeric(other.numeric),
            literal: self.literal.union(other.literal),
        }
    }
    /// As a field read that may be what's found, or something else, is: any
    /// of several functions, which may not be all it could be, is unknown, so
    /// a call to it doesn't call each as if it were. One function is kept,
    /// as it always has been
    pub(super) fn guessed(self) -> Type {
        match self.function {
            FunctionRef::Several(_) => Type { function: FunctionRef::Unknown, ..self },
            _ => self,
        }
    }
    /// Functions, tables and userdata are always truthy, so they're kept
    pub fn truthy(self) -> Type {
        Type { set: self.set.truthy(), ..self }
    }
    pub fn falsy(self) -> Type {
        Type::of(self.set.falsy())
    }
    pub fn without(self, other: TypeSet) -> Type {
        let set = self.set.without(other);
        let function = if set.contains(TypeSet::FUNCTION) { self.function } else { FunctionRef::None };
        let named = if set.intersection(OBJECTS) != TypeSet::NEVER { self.named } else { NamedRef::None };
        let numeric = if set.contains(TypeSet::NUMBER) { self.numeric } else { NamedRef::None };
        let literal = if set.contains(TypeSet::STRING) { self.literal } else { Literals::None };
        Type { set, function, named, numeric, literal }
    }
}

impl std::ops::BitOr for Type {
    type Output = Type;
    fn bitor(self, rhs: Type) -> Type {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for Type {
    fn bitor_assign(&mut self, rhs: Type) {
        *self = self.union(rhs);
    }
}

/// A list of values, like a call's results: a type per position, then `rest`
/// for every position after those
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Values {
    pub values: Vec<Type>,
    pub rest: Type,
}

impl Values {
    /// No values, so every position is nil
    pub fn none() -> Values {
        Values { values: Vec::new(), rest: Type::NIL }
    }
    pub fn unknown() -> Values {
        Values { values: Vec::new(), rest: Type::UNKNOWN }
    }
    /// Nothing known yet, before the first walk has seen any of it
    pub fn nothing() -> Values {
        Values { values: Vec::new(), rest: Type::default() }
    }
    pub fn get(&self, index: usize) -> Type {
        self.values.get(index).copied().unwrap_or(self.rest)
    }
    pub fn first(&self) -> Type {
        self.get(0)
    }
    pub fn union(&self, other: &Values) -> Values {
        let len = self.values.len().max(other.values.len());
        Values {
            values: (0..len).map(|i| self.get(i) | other.get(i)).collect(),
            rest: self.rest | other.rest,
        }
    }
}
