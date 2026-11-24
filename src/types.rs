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

//! Type tracking for un-annotated Lua, across the files of an addon.
//!
//! One walk over the tree resolves every variable name to a [`Binding`] (respecting
//! scopes and shadowing) and records a [`Reference`] with the type the variable has
//! at that point.
//!
//! Types are a [`TypeSet`] bitset, so unions (from branches, `and`/`or`, or
//! reassignment) are a single OR.
//!
//! Flow rules, chosen to over-approximate rather than be wrong:
//! * An assignment *replaces* a variable's current type if it is in the same block
//!   the variable was declared in (or an unconditional `do` block inside it).
//!   Otherwise (if branch, loop, function body) it is *unioned* with it.
//! * A read from inside a different function than the declaration (an upvalue)
//!   gets the union of every value ever assigned, since the closure could run at
//!   any time.
//! * Globals start as unknown, because other files (or the WoW API) may set them.
//!
//! * Loop bodies are walked until the types flowing back to the top of the loop
//!   stop changing, so a read sees values assigned later in the body by an
//!   earlier iteration.
//!
//! * Values that can be functions remember which function (a [`FunctionRef`]),
//!   and each function records what it returns, so calls have result types.
//!
//! Files are walked in load order (their `.toc` order) as one program: globals
//! carry over from file to file, and functions and table fields are shared. In
//! an addon, `...` at the top of a file is the addon's name and its namespace
//! table, one table shared by all its files.
//!
//! The files are walked at least twice. On the first walk, reading an upvalue,
//! calling a function defined further down, or reading a parameter gives
//! unknown. Each later walk uses the previous walk's final types for those, which
//! stays sound because every walk can only over-approximate. Walking stops when
//! everything a walk reads from the one before is unchanged, or if a walk's
//! state repeats an earlier one's, as walking on would then cycle.
//!
//! * Parameters are typed by what the body requires of them, if it passes them
//!   unconditionally to a parameter declared with a single type (`floor(x)`
//!   makes `x` a number). Otherwise they get the union of the arguments at every
//!   call, if every call is visible: a local function only ever called directly.
//!   Otherwise they're unknown.
//! * Tables and userdata can be a named type from the definitions, like `Frame`
//!   or `ItemKey` (a [`NamedRef`]), so WoW objects aren't just `table`.
//! * Tables made in the file have an internal identity (never shown), so their
//!   fields are tracked: each `{ ... }` is one table, and a local or parameter
//!   holding a table of unknown origin (`local _, ns = ...`) gets a pseudo
//!   identity of its own. A field's type is the union of every value assigned to
//!   it anywhere in the file (`{ x = 1 }`, `t.x = 2`, `function t:x() end`), so
//!   a read before the first write isn't shown as nil. Writes through a table
//!   whose identity isn't known (or a WoW object, shared by every `Frame`) are
//!   kept by field name and used when nothing more specific is known.
//! * Globals the file never assigns can have preset types from [`Definitions`]
//!   (the Lua standard library, the WoW API), including dotted fields of those
//!   globals like `string.format`.
//!
//! * A call passing one of its function's parameters (or `...`) straight on
//!   takes it as settled at the end of the same walk, so a chain of them
//!   settles together rather than one link per walk.
//! * The first walk starts from nothing known rather than unknown, so types
//!   grow walk by walk: a cycle (a field written with what's read from it)
//!   settles on what really flows round it instead of staying unknown. If
//!   walks start to cycle between states, each from then on is combined with
//!   the one before, which only grows, so it settles covering both.
//! * Each walk records, for every function body, what it read and everything
//!   it did. On the next walk a body that read nothing which has changed since
//!   is replayed from that record instead of walked: the same result, without
//!   the work. Turning this off (`WOW_LS_NO_REPLAY`) is only for checking it
//!
//! Known gaps: globals the file doesn't assign (like saved variables) have no
//! table identity. Recursive calls return unknown.

/// The analysis's maps and sets. Its keys are small (positions, names,
/// type ids), and nothing they hold comes from someone trying to make keys
/// collide, so a fast hash beats the standard one, built to resist that. It
/// also makes iteration order the same from run to run
pub type HashMap<K, V> = std::collections::HashMap<K, V, FastHash>;
pub type HashSet<K> = std::collections::HashSet<K, FastHash>;
pub type FastHash = std::hash::BuildHasherDefault<FastHasher>;

/// The hash `rustc` uses for its own maps: each word is mixed in with a
/// rotate, an xor and a multiply
#[derive(Default, Clone, Copy)]
pub struct FastHasher {
    hash: u64,
}

impl FastHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl std::hash::Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn write_u8(&mut self, i: u8) { self.add(i.into()) }
    fn write_u16(&mut self, i: u16) { self.add(i.into()) }
    fn write_u32(&mut self, i: u32) { self.add(i.into()) }
    fn write_u64(&mut self, i: u64) { self.add(i) }
    fn write_usize(&mut self, i: usize) { self.add(i as u64) }
    fn finish(&self) -> u64 { self.hash }
}
use std::fmt;
use std::sync::Arc;

use rowan::{NodeOrToken, TextRange, TextSize};

use crate::ast::*;
use crate::definitions::{BuiltinId, Definitions, NamedId};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

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
const POOL_OBJECT: u32 = 1 << 31;

/// Added to the slot of a pseudo identity: one a variable holding a table of
/// unknown origin is given where it's declared
const PSEUDO: u32 = 1 << 30;

/// Which function a value is, when it can be one
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FunctionRef {
    /// Can't be a function
    #[default]
    None,
    /// Exactly this function
    One(FunctionId),
    /// A function from the preset definitions
    Builtin(BuiltinId),
    /// A function we can't pin down: a parameter, an unknown call result, or one
    /// of several functions
    Unknown,
}

impl FunctionRef {
    fn union(self, other: FunctionRef) -> FunctionRef {
        match (self, other) {
            (FunctionRef::None, x) | (x, FunctionRef::None) => x,
            (a, b) if a == b => a,
            _ => FunctionRef::Unknown,
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
    /// It has the members all of them have
    Several(u32),
    /// A plain table, or one of several named types
    Unknown,
}

/// The most objects `NamedRef::Several` keeps apart. More are joined as the
/// closest type they all are
const MOST_OBJECTS: usize = 8;

/// Each list of objects a `NamedRef::Several` stands for, sorted, once each.
/// Shared by every analysis, so a dependency's types mean the same in its
/// dependents'
#[derive(Default)]
struct ObjectSets {
    lists: Vec<&'static [NamedRef]>,
    ids: std::collections::HashMap<&'static [NamedRef], u32>,
}

static OBJECT_SETS: std::sync::LazyLock<std::sync::Mutex<ObjectSets>> = std::sync::LazyLock::new(Default::default);

fn object_set(id: u32) -> &'static [NamedRef] {
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
struct Analysing(Option<Arc<Definitions>>);

impl Analysing {
    fn with(definitions: &Arc<Definitions>) -> Analysing {
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
fn common_object_type(a: NamedId, b: NamedId) -> Option<NamedId> {
    let (a, b) = (a.min(b), a.max(b));
    ANALYSING.with(|current| current.borrow().as_ref()?.common_ancestor(a, b))
}

/// Where a table was made (its `{`), or where the variable given a pseudo
/// identity was declared. The same across walks
pub type TableId = Location;

impl NamedRef {
    /// Joined, keeping different WoW objects apart (see `Several`)
    fn union(self, other: NamedRef) -> NamedRef {
        match (self, other) {
            (NamedRef::None, x) | (x, NamedRef::None) => x,
            (a, b) if a == b => a,
            (a, b) => match (a.objects(), b.objects()) {
                (Some(mut all), Some(more)) => {
                    all.extend(more);
                    NamedRef::several(all)
                }
                _ => NamedRef::Unknown,
            },
        }
    }

    /// The objects this may be, if it's only WoW objects
    fn objects(self) -> Option<Vec<NamedRef>> {
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
    fn several(mut objects: Vec<NamedRef>) -> NamedRef {
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
        if joined.len() > MOST_OBJECTS {
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
    fn union_numeric(self, other: NamedRef) -> NamedRef {
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
    fn loosened(self) -> NamedRef {
        match self {
            NamedRef::One(id) => NamedRef::Extended(id),
            NamedRef::Instance { named, at, .. } => NamedRef::Instance { named, at, extended: true },
            NamedRef::Several(_) => NamedRef::several(self.alternatives().into_iter().map(NamedRef::loosened).collect()),
            other => other,
        }
    }

    /// Each object this may be changed by `change`
    fn map_objects(self, change: impl Fn(NamedRef) -> NamedRef) -> NamedRef {
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
    fn object_type(self) -> Option<(NamedId, bool)> {
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
/// `numeric` is Unknown for a plain number
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Type {
    pub set: TypeSet,
    pub function: FunctionRef,
    pub named: NamedRef,
    pub numeric: NamedRef,
}

/// The types a named type can be based on
const OBJECTS: TypeSet = TypeSet(TypeSet::TABLE.0 | TypeSet::OTHER.0);

impl Type {
    pub const NIL: Type = Type { set: TypeSet::NIL, function: FunctionRef::None, named: NamedRef::None, numeric: NamedRef::None };
    pub const UNKNOWN: Type = Type {
        set: TypeSet::UNKNOWN,
        function: FunctionRef::Unknown,
        named: NamedRef::Unknown,
        numeric: NamedRef::Unknown,
    };

    /// A type with no known function or named-type identity
    pub fn of(set: TypeSet) -> Type {
        let function = if set.contains(TypeSet::FUNCTION) { FunctionRef::Unknown } else { FunctionRef::None };
        let named = if set.intersection(OBJECTS) != TypeSet::NEVER { NamedRef::Unknown } else { NamedRef::None };
        let numeric = if set.contains(TypeSet::NUMBER) { NamedRef::Unknown } else { NamedRef::None };
        Type { set, function, named, numeric }
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
        Type { set, function, named, numeric }
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Parameter {
    pub name: String,
    pub types: Type,
    /// The type the function requires, if it requires exactly one. Calls to the
    /// function then pass that requirement on to their arguments
    pub mandated: Option<Type>,
}

impl Parameter {
    /// A declared parameter, which mandates its type if it's a single one.
    /// Optional (`number|nil`) and multi-type parameters don't
    pub fn declared(name: &str, types: Type) -> Parameter {
        let single = [TypeSet::NUMBER, TypeSet::STRING, TypeSet::BOOLEAN, TypeSet::TRUE, TypeSet::FALSE,
            TypeSet::FUNCTION, TypeSet::TABLE, TypeSet::OTHER];
        let mandated = single.contains(&types.set).then_some(types);
        Parameter { name: name.to_string(), types, mandated }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FunctionInfo {
    /// Not counting `self` for a method
    pub parameters: Vec<Parameter>,
    /// Defined with `:`, so it takes `self` before its parameters
    pub method: bool,
    /// The type of each extra argument, if the function takes `...`
    pub varargs: Option<Parameter>,
    pub returns: Values,
    /// For each return value that's a table or object the function makes, so
    /// each call returns a new one: the one made in the function (whose fields
    /// and parent each call's gets), and the parameters it mixes in
    pub fresh_returns: Vec<Option<FreshObject>>,
    /// The functions whose results its body uses, by calling them, so calls
    /// that come back round to it are known to be recursive
    pub calls: Vec<FunctionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FreshObject {
    pub made: TableId,
    pub mixed_parameters: Vec<usize>,
    /// When it's a pool, the parameters mixed into its objects (by its init
    /// or reset function)
    pub pool_mixed_parameters: Option<Vec<usize>>,
}

/// What calling a function reads of it: all but what it calls, which
/// `Read::Calls` covers, and its parameters' types, which `Read::ParameterTypes`
/// does. Those settle walk by walk, and only its own body reads them
struct Signature<'a>(&'a FunctionInfo);

impl PartialEq for Signature<'_> {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0, other.0);
        a.method == b.method && a.varargs == b.varargs && a.returns == b.returns && a.fresh_returns == b.fresh_returns
            && a.parameters.len() == b.parameters.len()
            && a.parameters.iter().zip(&b.parameters).all(|(a, b)| a.name == b.name && a.mandated == b.mandated)
    }
}

impl FunctionInfo {
    fn signature(&self) -> Signature<'_> {
        Signature(self)
    }

    /// What the function requires of the argument at `index`
    fn mandated(&self, index: usize) -> Option<Type> {
        match self.parameters.get(index) {
            Some(parameter) => parameter.mandated,
            None => self.varargs.as_ref().and_then(|v| v.mandated),
        }
    }
}

pub type BindingId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Local,
    Parameter,
    Global,
}

impl fmt::Display for BindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindingKind::Local => write!(f, "local"),
            BindingKind::Parameter => write!(f, "parameter"),
            BindingKind::Global => write!(f, "global"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Binding {
    /// Shared, as replaying a body copies its bindings
    pub name: std::rc::Rc<str>,
    pub kind: BindingKind,
    /// None for globals, which have no single declaration
    pub declaration: Option<TextRange>,
    /// Union of every value assigned anywhere in the file
    pub types: Type,
    /// The file declaring it, or for a global, the first file using it
    pub file: FileId,
    /// Where a local or parameter can be seen by name, from just after its
    /// declaration to the end of its scope. None for globals
    pub visible: Option<TextRange>,
    function: usize,
    scope_depth: usize,
    assigned: bool,
    /// Written after its declaration (so far, on this walk)
    reassigned: bool,
}

impl Binding {
    /// Whether the files give it a value: always for a local or parameter, and
    /// for a global, whether any file assigns it
    pub fn assigned(&self) -> bool {
        self.assigned
    }
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub file: FileId,
    pub range: TextRange,
    pub binding: BindingId,
    /// The type at this point: the value read, or the value assigned for a write
    pub types: Type,
    pub write: bool,
    /// A read that's directly called: `f` in `f(x)`
    pub callee: bool,
    /// For a callee, what this call returns when that's more than the function
    /// says (`CreateFrame("Button")` giving a `Button`)
    pub call_returns: Option<Values>,
}

/// A use of a field name: `b` in `a.b`, `a.b = v`, `a:b()`, `function a:b()`
/// or `{ b = v }`
#[derive(Debug, Clone)]
pub struct FieldReference {
    pub file: FileId,
    /// The field name
    pub range: TextRange,
    /// Where the code reaching the field is, for `Analysis::field_path`. A
    /// pointer, found again in its file when needed, as keeping a node for
    /// every field reference made freeing a walk's results slow
    access: crate::syntax::SyntaxNodePtr,
    /// The token naming it, kept to read the name from without finding it in
    /// the tree, which walks needed for every field reference
    token: rowan::GreenToken,
    /// The field's type, or for a write the value assigned
    pub types: Type,
    /// The table or object made in the files that the field is on, when it's
    /// known which: what go to definition looks for its assignments on
    pub table: Option<TableId>,
    pub write: bool,
    /// A read that's directly called: `f` in `t.f(x)` or `t:f(x)`
    pub callee: bool,
    /// For a callee, what this call returns when that's more than the function
    /// says (a pool's `Acquire` giving the pool's object)
    pub call_returns: Option<Values>,
}

impl FieldReference {
    /// The field name
    fn name(&self) -> &str {
        let text = self.token.text();
        // `"b"` in `a["b"]`
        string_field_name(text).unwrap_or(text)
    }

    /// How the field was reached, without whitespace or comments, like
    /// `addonTable.hiddenTable` or `addonTable["hiddenTable"]`
    fn path(&self, roots: &[SyntaxNode]) -> String {
        let Some(access) = roots.get(self.file.0 as usize).and_then(|root| self.access.try_to_node(root)) else {
            return String::new()
        };
        let end = self.range.end();
        let tokens: Vec<SyntaxToken> = access.descendants_with_tokens()
            .filter_map(|t| t.into_token())
            .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment))
            .collect();
        let mut path: String = tokens.iter().filter(|t| t.text_range().end() <= end).map(|t| t.text()).collect();
        // A key in brackets is closed by the next one
        if tokens.iter().any(|t| t.text_range().start() >= end && t.kind() == SyntaxKind::RightSquareBracket) && path.contains('[') {
            path.push(']');
        }
        path
    }
}

/// What a table holds beyond its `name = value` fields: array items, and
/// anything stored under a computed key (`t[k] = v`, `{ [k] = v }`,
/// `table.insert(t, v)`). Values never include nil, as storing nil removes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Entries {
    pub keys: Type,
    pub values: Type,
}

/// What a condition proves about the locals it tests: the type each has
/// where it's true, and where it's false
#[derive(Default)]
struct Narrowing {
    when_true: Vec<(BindingId, Type)>,
    when_false: Vec<(BindingId, Type)>,
}

impl Narrowing {
    fn negated(self) -> Narrowing {
        Narrowing { when_true: self.when_false, when_false: self.when_true }
    }
}

/// Both of two sets of narrowed types holding: a local in both is the
/// intersection
fn both(mut a: Vec<(BindingId, Type)>, b: Vec<(BindingId, Type)>) -> Vec<(BindingId, Type)> {
    for (id, ty) in b {
        match a.iter_mut().find(|(other, _)| *other == id) {
            Some((_, existing)) => *existing = intersect(*existing, ty.set),
            None => a.push((id, ty)),
        }
    }
    a
}

/// The part of `ty` that's in `set`. If none of it is, the analysis must have
/// missed something, so `set` itself
fn intersect(ty: Type, set: TypeSet) -> Type {
    // Nothing known yet stays nothing, so types only grow walk by walk
    if ty.set == TypeSet::NEVER {
        return ty
    }
    let narrowed = ty.without(TypeSet::UNKNOWN.without(set));
    if narrowed.set == TypeSet::NEVER { Type::of(set) } else { narrowed }
}

/// A binary expression as the tree its precedences make
enum Operation {
    Operand(Expression),
    Binary(Operator, Box<Operation>, Box<Operation>),
}

/// Precedence climbing: operators bind tighter the higher their precedence.
/// Associativity doesn't change the result type for any operator, nor what a
/// condition proves, so everything is treated as left associative
fn operation_tree(operands: &mut std::vec::IntoIter<Expression>, operators: &[Operator], position: &mut usize, min_precedence: u8) -> Option<Operation> {
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

/// The type set `type(x)` names
fn type_name_set(name: &str) -> Option<TypeSet> {
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

/// A key a table is indexed with
enum Key {
    /// A string literal that could be a field name: `t["a"]` is `t.a`. With
    /// the literal's token, to record the field's use at
    Name(String, Option<SyntaxToken>),
    Other(Type),
}

/// A problem found while analysing, like a call of a method a WoW type
/// doesn't have
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: FileId,
    pub range: TextRange,
    pub message: String,
}

/// Where a name is given a value, for go to definition: the name's range in
/// one of the files analysed, or a dependency's (numbered by `layer_file`).
/// Empty at the start of a function when only the function is known
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Site {
    pub file: FileId,
    pub range: TextRange,
}

/// Adds `sites` to `to`, leaving out any already there: an addon reached
/// through two dependencies is added twice
fn add_sites(to: &mut Vec<Site>, sites: &[Site]) {
    for site in sites {
        if !to.contains(site) {
            to.push(*site);
        }
    }
}

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
    pub function_fields: HashSet<String>,
    pub entries: HashMap<TableId, Entries>,
    pub metatables: HashMap<TableId, Type>,
    pub mixins: HashMap<TableId, Vec<Type>>,
    pub mixed_into: HashMap<TableId, (NamedId, bool)>,
    pub parents: HashMap<TableId, Type>,
    pub pool_objects: HashMap<TableId, Type>,
    pub table_names: HashMap<TableId, String>,
    /// Where each global is assigned
    pub global_sites: HashMap<String, Vec<Site>>,
    /// Where each field of each table is assigned
    pub field_sites: HashMap<TableId, HashMap<String, Vec<Site>>>,
    /// Where fields of tables whose identity wasn't known are assigned, by name
    pub any_field_sites: HashMap<String, Vec<Site>>,
}

/// Where the files of addons in a `Layer` are numbered from: far above any
/// addon's own files, and below the pseudo files of namespaces
const LAYER_FILES: u32 = 1 << 30;

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

fn layer_file_numbers() -> &'static std::sync::Mutex<std::collections::HashMap<String, u32>> {
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
        for (at, &ty) in &analysis.parents {
            *own.parents.entry(renumber(*at, &file)).or_default() |= renumber_type(ty, &file);
        }
        for (at, &ty) in &analysis.pool_objects {
            *own.pool_objects.entry(renumber(*at, &file)).or_default() |= renumber_type(ty, &file);
        }
        for (at, name) in &analysis.table_names {
            own.table_names.insert(renumber(*at, &file), name.clone());
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
        for (at, &ty) in &other.parents {
            *self.parents.entry(*at).or_default() |= ty;
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
    fn field(&self, table: TableId, name: &str, depth: usize) -> Option<Type> {
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
fn renumber(location: Location, file: &impl Fn(FileId) -> FileId) -> Location {
    Location { file: file(location.file), ..location }
}

fn renumber_named(named: NamedRef, file: &impl Fn(FileId) -> FileId) -> NamedRef {
    match named {
        NamedRef::Table(at) => NamedRef::Table(renumber(at, file)),
        NamedRef::Instance { named, at, extended } => NamedRef::Instance { named, at: renumber(at, file), extended },
        NamedRef::Several(_) => named.map_objects(|object| renumber_named(object, file)),
        other => other,
    }
}

fn renumber_type(ty: Type, file: &impl Fn(FileId) -> FileId) -> Type {
    let function = match ty.function {
        FunctionRef::One(id) => FunctionRef::One(renumber(id, file)),
        other => other,
    };
    Type { function, named: renumber_named(ty.named, file), numeric: renumber_named(ty.numeric, file), set: ty.set }
}

fn renumber_values(values: &Values, file: &impl Fn(FileId) -> FileId) -> Values {
    Values { values: values.values.iter().map(|&v| renumber_type(v, file)).collect(), rest: renumber_type(values.rest, file) }
}

fn renumber_function(info: &FunctionInfo, file: &impl Fn(FileId) -> FileId) -> FunctionInfo {
    let parameter = |p: &Parameter| Parameter { name: p.name.clone(), types: renumber_type(p.types, file), mandated: p.mandated.map(|m| renumber_type(m, file)) };
    FunctionInfo {
        parameters: info.parameters.iter().map(parameter).collect(),
        method: info.method,
        varargs: info.varargs.as_ref().map(parameter),
        returns: renumber_values(&info.returns, file),
        fresh_returns: info.fresh_returns.iter().map(|fresh| fresh.as_ref().map(|fresh| FreshObject {
            made: renumber(fresh.made, file),
            ..fresh.clone()
        })).collect(),
        calls: info.calls.iter().map(|&id| renumber(id, file)).collect(),
    }
}

/// A file to analyse
pub struct SourceFile {
    pub root: SyntaxNode,
    /// The addon the file is part of, by its place in load order, so `...` at
    /// its top is that addon's name and namespace. Addons analysed together
    /// (one and those it depends on) each have their own namespace
    pub addon: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Analysis {
    pub bindings: Vec<Binding>,
    /// Sorted by file, then position
    pub references: Vec<Reference>,
    /// Sorted by file, then position
    pub field_references: Vec<FieldReference>,
    pub functions: HashMap<FunctionId, FunctionInfo>,
    pub definitions: Arc<Definitions>,
    /// Every value assigned to each field of each table made in the file
    pub fields: HashMap<TableId, HashMap<String, Type>>,
    /// Values assigned to fields of tables whose identity wasn't known, by name
    pub any_table_fields: HashMap<String, Type>,
    /// What each table made in the files holds beyond its named fields
    pub entries: HashMap<TableId, Entries>,
    /// Each table's metatable, from `setmetatable`
    pub metatables: HashMap<TableId, Type>,
    /// What `...` is in each function taking it, when every call is seen: the
    /// arguments past its named parameters
    pub vararg_values: HashMap<FunctionId, Values>,
    /// How many walks it took to settle
    pub walks: usize,
    /// Each file's tree, for what field references point to
    roots: Vec<SyntaxNode>,
    /// What the addons this one depends on make visible to it
    pub layer: Arc<Layer>,
    /// References that read an upvalue, given the binding's final type by
    /// `complete`
    deferred: Vec<usize>,
    /// Calls passing a parameter on, for `Record`s to point into
    pending_calls: Vec<PendingCall>,
    /// What walking each function body did, for the next walk to replay
    records: HashMap<FunctionId, Arc<Record>>,
    /// Function bodies replayed rather than walked, over every walk, and
    /// walked
    pub replayed: usize,
    pub walked: usize,
    /// Every field name any table in the files was given a function under
    pub function_fields: HashSet<String>,
    /// The parent of each WoW object made in the files: what it was made with
    /// (`CreateFrame`'s parent, or the receiver of `CreateTexture` and the like)
    /// and anything passed to its `SetParent`
    pub parents: HashMap<TableId, Type>,
    /// The tables each table or object made in the files had mixed into it (by
    /// `Mixin` or `CreateFromMixins`), whose fields it has too
    pub mixins: HashMap<TableId, Vec<Type>>,
    /// For each table mixed into WoW objects (directly, or through tables made
    /// from it with `CreateFromMixins`), the one type those objects all are,
    /// and whether they may have members beyond that type's. What `self` is in
    /// the table's methods
    pub mixed_into: HashMap<TableId, (NamedId, bool)>,
    /// The name each table made in the files was first stored under (a
    /// variable or a field path like `ns.ButtonMixin`), to show its mixins by
    pub table_names: HashMap<TableId, String>,
    /// What `Acquire` gives for each pool made in the files (by
    /// `CreateFramePool` and the like): one object standing for all of them,
    /// whose fields and mixins come from the pool's init and reset functions
    pub pool_objects: HashMap<TableId, Type>,
    /// The value of each global that XML names as a mixin wow_ls doesn't know
    /// (see `Definitions::xml_mixins`), as the files or those they depend on
    /// assign it
    pub lua_mixins: HashMap<String, Type>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// One file on its own
    pub fn analyse_file(root: &SyntaxNode, definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse(&[SourceFile { root: root.clone(), addon: None }], definitions)
    }

    /// Files in the order they load, as one program. The `FileId`s in the
    /// result are positions in `files`
    pub fn analyse(files: &[SourceFile], definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse_over(files, definitions, Arc::default())
    }

    /// `analyse`, for files loaded after the addons `layer` comes from
    pub fn analyse_over(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>) -> Analysis {
        // Replaying is only ever faster; turning it off is for checking it
        let replay = std::env::var_os("WOW_LS_NO_REPLAY").is_none();
        Analysis::analyse_replaying(files, definitions, layer, replay)
    }

    /// `analyse`, replaying function bodies whose inputs haven't changed
    /// since the walk before, or walking every one every time
    fn analyse_replaying(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>, replay: bool) -> Analysis {
        let _analysing = Analysing::with(definitions);
        let mut result = Walker::new(None, definitions, &layer, Default::default(), replay).walk(files);
        // Nothing to compare the first walk with, but no body it recorded can
        // be replayed anyway, as it walked without a previous walk
        let mut changed = HashSet::default();
        let mut seen = HashSet::from_iter([result.fingerprint()]);
        // Whether walks have started to cycle, so each is combined with the
        // one before
        let mut combining = false;
        for walks in 2.. {
            let mut next = Walker::new(Some(&result), definitions, &layer, changed, replay).walk(files);
            next.walks = walks;
            next.replayed += result.replayed;
            next.walked += result.walked;
            if combining {
                next.combine_inputs(&result);
            }
            changed = next.changed_reads(&result);
            // Nothing the next walk would read has changed
            let settled = changed.is_empty();
            // Walks are deterministic, so a state coming round again would
            // cycle forever (a field and parameter feeding each other can
            // swap between two types). From then on each walk keeps what
            // the one before had too: both sides of the cycle, which only
            // grows, so it settles
            combining |= !seen.insert(next.fingerprint());
            result = next;
            if settled {
                break
            }
        }
        result.complete();
        result
    }

    /// What only the result shows, not needed between walks: done once, for
    /// the last walk
    fn complete(&mut self) {
        // Only the next walk needed them
        self.records = HashMap::default();
        self.pending_calls = Vec::new();
        for &index in &self.deferred {
            let binding = self.references[index].binding;
            self.references[index].types = self.bindings[binding].types;
        }
        self.references.sort_by_key(|r| (r.file, r.range.start()));
        self.field_references.sort_by_key(|r| (r.file, r.range.start()));
        self.diagnostics.sort_by_key(|d| (d.file, d.range.start()));
        // References are sorted, so the first write of each table wins.
        // Not parameters, whose types come from what calls pass them
        // Only writes of a table can name one, and paths are slow to find
        let is_table = |types: Type| matches!(types.named, NamedRef::Table(_));
        let variables = self.references.iter()
            .filter(|r| r.write && is_table(r.types) && self.bindings[r.binding].kind != BindingKind::Parameter)
            .map(|r| (r.file, r.range.start(), r.types, self.bindings[r.binding].name.to_string()));
        let fields = self.field_references.iter().filter(|f| f.write && is_table(f.types))
            .map(|f| (f.file, f.range.start(), f.types, f.path(&self.roots)));
        let mut named: Vec<_> = variables.chain(fields).collect();
        named.sort_by_key(|&(file, start, ..)| (file, start));
        for (_, _, types, name) in named {
            if let NamedRef::Table(at) = types.named {
                self.table_names.entry(at).or_insert(name);
            }
        }
    }

    /// What a walk from `self` would read differently from one from `older`.
    /// Empty when the walks have settled
    fn changed_reads(&self, older: &Analysis) -> HashSet<Read> {
        let mut changed = HashSet::default();
        fn keys<K: std::hash::Hash + Eq + Copy, V>(a: &HashMap<K, V>, b: &HashMap<K, V>) -> HashSet<K> {
            a.keys().chain(b.keys()).copied().collect()
        }
        let longest = self.bindings.len().max(older.bindings.len());
        for id in 0..longest {
            let same = match (self.bindings.get(id), older.bindings.get(id)) {
                (Some(a), Some(b)) => a.name == b.name && a.types == b.types && a.assigned == b.assigned,
                _ => false,
            };
            if !same {
                changed.insert(Read::Binding(id));
            }
        }
        for id in keys(&self.functions, &older.functions) {
            let (a, b) = (self.functions.get(&id), older.functions.get(&id));
            if a.map(FunctionInfo::signature) != b.map(FunctionInfo::signature) {
                changed.insert(Read::Function(id));
            }
            if a.map(|f| &f.calls) != b.map(|f| &f.calls) {
                changed.insert(Read::Calls(id));
            }
            let types_differ = match (a, b) {
                (Some(a), Some(b)) => a.parameters.iter().map(|p| p.types).ne(b.parameters.iter().map(|p| p.types)),
                (a, b) => a.is_some() != b.is_some(),
            };
            if types_differ {
                changed.insert(Read::ParameterTypes(id));
            }
        }
        for at in keys(&self.fields, &older.fields) {
            let (a, b) = (self.fields.get(&at), older.fields.get(&at));
            if a == b {
                continue
            }
            changed.insert(Read::Fields(at));
            let empty = HashMap::default();
            let (a, b) = (a.unwrap_or(&empty), b.unwrap_or(&empty));
            for name in a.keys().chain(b.keys()) {
                if a.get(name) != b.get(name) {
                    changed.insert(Read::Field(at, name_key(name)));
                }
            }
        }
        for name in self.any_table_fields.keys().chain(older.any_table_fields.keys()) {
            if self.any_table_fields.get(name) != older.any_table_fields.get(name) {
                changed.insert(Read::AnyTableField(name_key(name)));
            }
        }
        for name in self.function_fields.symmetric_difference(&older.function_fields) {
            changed.insert(Read::FunctionField(name_key(name)));
        }
        for at in keys(&self.entries, &older.entries) {
            if self.entries.get(&at) != older.entries.get(&at) {
                changed.insert(Read::Entries(at));
            }
        }
        for at in keys(&self.metatables, &older.metatables) {
            if self.metatables.get(&at) != older.metatables.get(&at) {
                changed.insert(Read::Metatable(at));
            }
        }
        for at in keys(&self.mixins, &older.mixins) {
            if self.mixins.get(&at) != older.mixins.get(&at) {
                changed.insert(Read::Mixins(at));
            }
        }
        for at in keys(&self.mixed_into, &older.mixed_into) {
            if self.mixed_into.get(&at) != older.mixed_into.get(&at) {
                changed.insert(Read::MixedInto(at));
            }
        }
        for at in keys(&self.parents, &older.parents) {
            if self.parents.get(&at) != older.parents.get(&at) {
                changed.insert(Read::Parent(at));
            }
        }
        for at in keys(&self.pool_objects, &older.pool_objects) {
            if self.pool_objects.get(&at) != older.pool_objects.get(&at) {
                changed.insert(Read::PoolObject(at));
            }
        }
        for id in keys(&self.vararg_values, &older.vararg_values) {
            if self.vararg_values.get(&id) != older.vararg_values.get(&id) {
                changed.insert(Read::Varargs(id));
            }
        }
        if self.lua_mixins != older.lua_mixins {
            changed.insert(Read::LuaMixins);
        }
        changed
    }

    /// Adds what `other` had to everything a walk reads from this one
    fn combine_inputs(&mut self, other: &Analysis) {
        for (at, fields) in &other.fields {
            let mine = self.fields.entry(*at).or_default();
            for (name, &ty) in fields {
                *mine.entry(name.clone()).or_default() |= ty;
            }
        }
        for (at, &ty) in &other.parents {
            *self.parents.entry(*at).or_default() |= ty;
        }
        for (at, mixins) in &other.mixins {
            let mine = self.mixins.entry(*at).or_default();
            for mixin in mixins {
                if !mine.contains(mixin) {
                    mine.push(*mixin);
                }
            }
        }
        for (at, &(named, extended)) in &other.mixed_into {
            match self.mixed_into.get(at).copied() {
                None => { self.mixed_into.insert(*at, (named, extended)); }
                Some((mine, mine_extended)) if mine == named => { self.mixed_into.insert(*at, (named, extended || mine_extended)); }
                // Mixed into objects of no one type
                Some(_) => { self.mixed_into.remove(at); }
            }
        }
        for (at, &ty) in &other.pool_objects {
            *self.pool_objects.entry(*at).or_default() |= ty;
        }
        for (name, &ty) in &other.any_table_fields {
            *self.any_table_fields.entry(name.clone()).or_default() |= ty;
        }
        self.function_fields.extend(other.function_fields.iter().cloned());
        for (at, entries) in &other.entries {
            let mine = self.entries.entry(*at).or_default();
            mine.keys |= entries.keys;
            mine.values |= entries.values;
        }
        for (at, &ty) in &other.metatables {
            *self.metatables.entry(*at).or_default() |= ty;
        }
        for (id, values) in &other.vararg_values {
            let combined = match self.vararg_values.get(id) {
                Some(mine) => mine.union(values),
                None => values.clone(),
            };
            self.vararg_values.insert(*id, combined);
        }
        for (id, theirs) in &other.functions {
            let Some(mine) = self.functions.get_mut(id) else {
                self.functions.insert(*id, theirs.clone());
                continue
            };
            for (parameter, other_parameter) in mine.parameters.iter_mut().zip(&theirs.parameters) {
                parameter.types |= other_parameter.types;
            }
            if let (Some(varargs), Some(other_varargs)) = (&mut mine.varargs, &theirs.varargs) {
                varargs.types |= other_varargs.types;
            }
            mine.returns = mine.returns.union(&theirs.returns);
        }
        for (binding, theirs) in self.bindings.iter_mut().zip(&other.bindings) {
            if binding.name == theirs.name {
                binding.types |= theirs.types;
                binding.assigned |= theirs.assigned;
            }
        }
    }

    /// A hash of everything a walk reads from the one before
    fn fingerprint(&self) -> u64 {
        use std::hash::{BuildHasher, Hash};
        // Spreads a weak hash's bits, so sums of them rarely collide
        fn mixed(value: impl Hash) -> u64 {
            let mut z = FastHash::default().hash_one(value).wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        // Maps iterate in no fixed order, so their entries' hashes are added,
        // which doesn't depend on it. Each map has its own tag
        fn map<K: Hash, V: Hash>(tag: u8, map: &HashMap<K, V>) -> u64 {
            map.iter().fold(0u64, |sum, entry| sum.wrapping_add(mixed((tag, entry))))
        }
        let fields = self.fields.iter()
            .flat_map(|(at, fields)| fields.iter().map(move |field| (at, field)))
            .fold(0u64, |sum, entry| sum.wrapping_add(mixed((0u8, entry))));
        let function_fields = self.function_fields.iter().fold(0u64, |sum, name| sum.wrapping_add(mixed((1u8, name))));
        let bindings = mixed(self.bindings.iter().map(|b| (&b.name, b.types, b.assigned)).collect::<Vec<_>>());
        mixed([
            fields,
            function_fields,
            bindings,
            map(2, &self.parents),
            map(3, &self.mixins),
            map(4, &self.mixed_into),
            map(5, &self.pool_objects),
            map(6, &self.any_table_fields),
            map(7, &self.entries),
            map(8, &self.metatables),
            map(9, &self.vararg_values),
            map(10, &self.functions),
        ])
    }

    /// e.g. `number?`, `Frame|string` or `function(a, b) -> string`
    pub fn describe(&self, ty: Type) -> String {
        self.describe_with(ty, true)
    }

    /// Without function signatures, which could recurse forever on
    /// `function f() return f end`
    fn describe_shallow(&self, ty: Type) -> String {
        self.describe_with(ty, false)
    }

    fn describe_with(&self, ty: Type, signatures: bool) -> String {
        // Any of several objects: each as it would be on its own, once
        if let NamedRef::Several(_) = ty.named {
            let mut objects: Vec<String> = Vec::new();
            for object in ty.named.alternatives() {
                let one = self.describe_with(Type { named: object, ..Type::of(ty.set.intersection(OBJECTS)) }, signatures);
                if !objects.contains(&one) {
                    objects.push(one);
                }
            }
            let several = objects.len() > 1;
            let objects = objects.iter()
                .map(|one| if several && one.contains(' ') { format!("({one})") } else { one.clone() })
                .collect::<Vec<_>>()
                .join("|");
            let rest = ty.without(OBJECTS);
            return match rest.set {
                TypeSet::NEVER => objects,
                TypeSet::NIL if several => format!("({objects})?"),
                TypeSet::NIL => format!("{objects}?"),
                _ => format!("{objects}|{}", self.describe_with(rest, signatures)),
            }
        }
        let names = ty.set.names();
        let signature = match ty.function {
            FunctionRef::One(id) if signatures => self.function_info(FunctionRef::One(id)).map(|f| self.signature(f)),
            FunctionRef::Builtin(id) if signatures => self.definitions.function(id).map(|f| self.signature(f)),
            _ => None,
        };
        let named = ty.named.named_id()
            .and_then(|id| self.definitions.named_type(id))
            .filter(|named| !named.namespace);
        let numeric = match ty.numeric {
            NamedRef::One(id) => self.definitions.named_type(id),
            _ => None,
        };
        // Nested tables are just `table`, which also stops a table holding itself recursing
        let contents = if signatures { self.describe_contents(ty) } else { None };
        // A single type that can also be nil is written `T?`
        let optional = names.len() == 2 && names[1] == "nil" && !names[0].starts_with("unknown");
        let wrap = names.len() > 1;
        let names: Vec<String> = names.iter()
            .filter(|&&name| !(optional && name == "nil"))
            .map(|&name| match (&signature, named, name) {
                (Some(s), _, "function") if wrap && s.contains(" -> ") => format!("({s})"),
                (Some(s), _, "function") => s.clone(),
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] && signatures
                    && ty.named.identity().is_some_and(|at| self.pool_object(at).is_some()) => {
                    let object = ty.named.identity().and_then(|at| self.pool_object(at)).unwrap_or(Type::UNKNOWN);
                    format!("{}<{}>", named.name, self.describe_shallow(object))
                }
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] => {
                    let mixins = ty.named.identity().map(|at| self.object_mixins(at)).unwrap_or_default();
                    match mixins.is_empty() {
                        true => named.name.clone(),
                        false if wrap || optional => format!("({} & {})", named.name, mixins.join(" & ")),
                        false => format!("{} & {}", named.name, mixins.join(" & ")),
                    }
                }
                (_, _, "number") if numeric.is_some() => numeric.map_or_else(String::new, |n| n.name.clone()),
                (_, _, "table") if contents.is_some() => {
                    let contents = contents.clone().unwrap_or_default();
                    // `(string[])|number`, but `string[]?`
                    if wrap && !optional && contents.ends_with("[]") { format!("({contents})") } else { contents }
                }
                _ => name.to_string(),
            })
            .collect();
        let described = names.join("|");
        if optional { described + "?" } else { described }
    }

    /// The names of the mixins a WoW object made in the files has, in the order
    /// they were mixed in. In a mixin's own methods, `self` is the mixin
    fn object_mixins(&self, at: TableId) -> Vec<String> {
        let mut names = Vec::new();
        match self.table_name(at) {
            Some(name) => names.push(name.clone()),
            None => self.collect_mixins(at, &mut names, 0),
        }
        names
    }

    /// Named mixins are listed by name. Unnamed ones (an inline
    /// `CreateFromMixins(A, B)`, or the frame a function made and returns) are
    /// listed by their own mixins
    fn collect_mixins(&self, at: TableId, names: &mut Vec<String>, depth: usize) {
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return
        }
        for mixin in self.mixins.get(&at).or_else(|| self.layer.mixins.get(&at)).into_iter().flatten() {
            // One the definitions give, by its name
            let defined = mixin.named.named_id().and_then(|id| self.definitions.named_type(id)).filter(|named| named.namespace);
            if let Some(named) = defined {
                if !names.contains(&named.name) {
                    names.push(named.name.clone());
                }
                continue
            }
            let Some(mixin) = mixin.named.identity() else { continue };
            match self.table_name(mixin) {
                Some(name) => if !names.contains(name) { names.push(name.clone()) },
                None => self.collect_mixins(mixin, names, depth + 1),
            }
        }
    }

    /// The name a table was first stored under, here or in a dependency
    fn table_name(&self, at: TableId) -> Option<&String> {
        self.table_names.get(&at).or_else(|| self.layer.table_names.get(&at))
    }

    /// What a pool made here or in a dependency holds
    fn pool_object(&self, at: TableId) -> Option<Type> {
        self.pool_objects.get(&at).or_else(|| self.layer.pool_objects.get(&at)).copied()
    }

    fn signature(&self, function: &FunctionInfo) -> String {
        self.signature_returning(function, &function.returns)
    }

    /// A callee's type, with what the call returns when that's known better
    /// than the function's own returns
    pub fn describe_callee(&self, ty: Type, call_returns: Option<&Values>) -> String {
        let function = self.function_info(ty.function);
        match (function, call_returns) {
            // Only when it's sure to be the function, so the signature is all of it
            (Some(function), Some(returns)) if ty.set == TypeSet::FUNCTION => self.signature_returning(function, returns),
            _ => self.describe(ty),
        }
    }

    fn signature_returning(&self, function: &FunctionInfo, returns: &Values) -> String {
        let parameter = |p: &Parameter| match p.types {
            Type::UNKNOWN => p.name.clone(),
            types => format!("{}: {}", p.name, self.describe_shallow(types)),
        };
        let mut parameters: Vec<String> = function.parameters.iter().map(parameter).collect();
        if let Some(varargs) = &function.varargs {
            parameters.push(parameter(varargs));
        }
        let mut signature = format!("function({})", parameters.join(", "));
        let mut parts: Vec<String> = returns.values.iter().map(|&t| self.describe_shallow(t)).collect();
        if returns.rest != Type::NIL {
            parts.push("...".to_string());
        }
        if !parts.is_empty() {
            signature += " -> ";
            signature += &parts.join(", ");
        }
        signature
    }

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
        if let (true, FunctionRef::One(id)) = (sites.is_empty(), field.types.function) {
            sites.push(Site { file: id.file, range: TextRange::empty(id.offset) });
        }
        sites
    }

    fn variable_sites(&self, binding: &Binding) -> Vec<Site> {
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

    /// Where field `name` of `table` is assigned. As for reading it, the
    /// writes to tables of unknown identity are the fallback
    fn field_sites(&self, table: Option<TableId>, name: &str) -> Vec<Site> {
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
        sites
    }

    /// The table that field `name` of `table` comes from: its own, else the
    /// one a mixin (the last first) or its metatable's `__index` gives it
    fn field_holder(&self, table: TableId, name: &str, depth: usize) -> Option<TableId> {
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
    fn describe_contents(&self, ty: Type) -> Option<String> {
        let NamedRef::Table(at) = ty.named else { return None };
        if !self.entries.contains_key(&at) && !self.layer.entries.contains_key(&at) {
            return None
        }
        let contents = self.contents(at)?;
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
        field.path(&self.roots)
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

struct Scope {
    names: HashMap<String, BindingId>,
    /// Code in this scope might not run (or might run many times)
    conditional: bool,
    /// Where the scope's code ends, after which its locals can't be seen
    end: TextSize,
}

#[derive(Default)]
struct Walker<'a> {
    /// Results of the previous walk, used where this walk doesn't know yet
    prior: Option<&'a Analysis>,
    /// Each file's tree
    roots: Vec<SyntaxNode>,
    /// What the addons depended on make visible
    layer: Arc<Layer>,
    /// What changed between the previous walk and the one before it, so
    /// bodies that read none of it can be replayed
    changed: HashSet<Read>,
    /// Shared results that bodies walked (not replayed) on this walk changed
    dirty: HashSet<Read>,
    /// Whether bodies can be replayed, rather than always walked
    replay: bool,
    /// The function bodies being walked, innermost last, and what each has read
    frames: Vec<Frame>,
    reads: std::cell::RefCell<Vec<HashSet<Read>>>,
    records: HashMap<FunctionId, Arc<Record>>,
    replayed: usize,
    walked: usize,
    /// The file being walked
    file: FileId,
    /// The addon that file is part of, by its place in load order
    addon: Option<u32>,

    definitions: Arc<Definitions>,
    /// The preset types at `BUILTIN_PATHS`, as calls check for them by path
    builtins: Vec<(&'static str, Option<Type>)>,
    bindings: Vec<Binding>,
    /// Type of each binding at the current point of the walk
    current: Vec<Type>,
    /// While walking a loop, each binding whose current type was set, with
    /// the type before, so a pass compares only what it changed
    loop_log: Vec<(BindingId, Type)>,
    /// Loops being walked, innermost last
    loop_depth: usize,
    references: Vec<Reference>,
    field_references: Vec<FieldReference>,
    diagnostics: Vec<Diagnostic>,
    /// References that read an upvalue, resolved once all assignments are known
    deferred: Vec<usize>,
    scopes: Vec<Scope>,
    globals: HashMap<String, BindingId>,
    /// Names added to `globals`, in order, so a loop pass can be undone
    globals_created: Vec<String>,
    function: usize,
    function_count: usize,
    functions: HashMap<FunctionId, FunctionInfo>,
    /// Union of the `return`s seen so far, for each function being walked
    returns: Vec<Option<Values>>,
    /// The functions called so far, for each function being walked
    calls: Vec<Vec<FunctionId>>,
    /// Index in `scopes` of the current function's body
    function_scope: usize,
    /// Inside the right of an `and`/`or`, which might not run
    short_circuit_depth: usize,
    /// Union of the arguments at every direct call of each function, except
    /// those in `pending_calls`
    call_arguments: HashMap<FunctionId, Values>,
    /// Calls passing a parameter straight on, settled at the end of the walk
    pending_calls: Vec<PendingCall>,
    /// Each function's id, by its number in the walk
    function_numbers: HashMap<usize, FunctionId>,
    /// Each parameter's type on entry to its function, to tell when it's
    /// been narrowed
    parameter_entry: HashMap<BindingId, Type>,
    /// The single type each parameter has been required to have so far. None
    /// when two requirements conflicted
    mandates: HashMap<BindingId, Option<Type>>,
    /// The local each function literal was directly assigned to
    assigned_to: HashMap<FunctionId, BindingId>,
    /// Field writes seen on this walk. Reads use the previous walk's, as a
    /// field can be written after (or in another function from) a read
    fields: HashMap<TableId, HashMap<String, Type>>,
    any_table_fields: HashMap<String, Type>,
    /// Entries seen on this walk. Reads use the previous walk's, like fields
    entries: HashMap<TableId, Entries>,
    /// Metatables seen on this walk. Reads use the previous walk's, like fields
    metatables: HashMap<TableId, Type>,
    /// Worked out with parameter types, at the end of the walk
    vararg_values: HashMap<FunctionId, Values>,
    /// The functions being walked, innermost last
    function_ids: Vec<FunctionId>,
    /// Calls to iterator makers like `pairs` and `gmatch`, by where they are,
    /// and what each variable of a loop over them gets, when that's known
    iterated: HashMap<Location, Values>,
    /// Parents seen on this walk. Reads use the previous walk's, like fields
    parents: HashMap<TableId, Type>,
    /// The field names each function was stored under, in any table
    stored_in_fields: HashMap<FunctionId, HashSet<String>>,
    /// Mixins seen on this walk. Reads use the previous walk's, like fields
    mixins: HashMap<TableId, Vec<Type>>,
    /// What each table made in the files was mixed into on this walk
    mixed_into: HashMap<TableId, Vec<NamedRef>>,
    /// Pools made on this walk, and what they hold
    pool_objects: HashMap<TableId, Type>,
    /// Functions passed straight to a pool, which calls them only as it
    /// documents (with its objects)
    pool_callbacks: HashSet<FunctionId>,
    /// Calls to a pool's `EnumerateActive`, by where they are, and the objects
    /// the iterator gives
    enumerated: HashMap<Location, Type>,
    /// The function being walked's code, to tell what it makes, for each
    /// function being walked
    function_ranges: Vec<(TextRange, usize)>,
    /// Per function being walked: what each return position returns new, so
    /// far (None once returns disagree)
    fresh: Vec<Option<Vec<Option<FreshObject>>>>,
    /// Which parameter of its function each parameter binding is
    parameter_index: HashMap<BindingId, usize>,
    /// For tables and objects made in a function: which of its parameters
    /// were mixed into them
    parameter_mixins: HashMap<TableId, Vec<usize>>,
}

/// Where a call's callee was read
#[derive(Clone, Copy)]
enum CalleeRead {
    /// An index into `references`
    Variable(usize),
    /// An index into `field_references`
    Field(usize),
}

/// What a mixin was mixed into, as far as its methods' `self` goes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixinTarget {
    /// Nothing seen
    Nothing,
    /// Only WoW objects, all of this type (and whether they may have members
    /// beyond it)
    Object(NamedId, bool),
    /// A plain table, or objects with no type in common
    Other,
}

/// A change a walk makes to what it's gathering across function bodies. Each
/// is made through `Walker::apply` and recorded, so a body whose inputs
/// haven't changed can make its changes again without being walked
#[derive(Debug, Clone, PartialEq)]
enum Write {
    Field(TableId, String, Type),
    AnyTableField(String, Type),
    StoredInField(FunctionId, String),
    Entry(TableId, Type, Type),
    Metatable(TableId, Type),
    CallArguments(FunctionId, Values),
    MixinsReplaced(TableId, Vec<Type>),
    MixinAdded(TableId, Type),
    MixedInto(TableId, NamedRef),
    PoolCallback(FunctionId),
    AssignedTo(FunctionId, BindingId),
    ParameterMixin(TableId, usize),
    Parent(TableId, Type),
    PoolObject(TableId, Type),
    Function(FunctionId, FunctionInfo),
    /// A variable declared outside the body was assigned this
    Binding(BindingId, Type),
}

/// Something a function body's walk read that a later walk could see
/// differently: from the previous walk, or another body earlier in this one
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Read {
    Binding(BindingId),
    Function(FunctionId),
    /// A field by name, which goes by its name's hash. Two names sharing a
    /// hash only means a body is walked again when it needn't be
    Field(TableId, u64),
    /// All of a table's fields, as `t[k]` and `pairs` see them
    Fields(TableId),
    AnyTableField(u64),
    FunctionField(u64),
    Entries(TableId),
    Metatable(TableId),
    Mixins(TableId),
    MixedInto(TableId),
    Parent(TableId),
    PoolObject(TableId),
    Varargs(FunctionId),
    /// Only the functions a function calls, which finding recursion reads
    Calls(FunctionId),
    /// Only the types of a function's parameters, as the previous walk settled them
    ParameterTypes(FunctionId),
    /// What the globals XML names as mixins are
    LuaMixins,
}

/// A name's hash, for `Read`
fn name_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// What walking a function body did: everything it added and changed, and
/// everything it read. A later walk where none of what it read has changed
/// does the same again instead of walking it
#[derive(Debug)]
struct Record {
    /// What the body was walked with, which must be the same to replay it
    self_type: Option<Type>,
    with_prior: bool,
    bindings_start: usize,
    function_count_start: usize,
    /// The bindings it declared, as they were when it ended. Shared, like the
    /// rest, with the records replaying it on later walks, which are the same
    bindings: Arc<[Binding]>,
    current: Arc<[Type]>,
    /// What it added to the walk's lists, as positions in those of the walk
    /// that recorded it
    references: std::ops::Range<usize>,
    field_references: std::ops::Range<usize>,
    diagnostics: std::ops::Range<usize>,
    pending_calls: std::ops::Range<usize>,
    /// Upvalue reads, by position among its references
    deferred: Arc<[usize]>,
    /// Globals it was first to use
    globals: Arc<[(String, BindingId)]>,
    /// Functions it holds, itself included
    function_count: usize,
    writes: Arc<[Arc<Write>]>,
    reads: Arc<HashSet<Read>>,
}

/// A function body being walked, for its `Record`
struct Frame {
    bindings_start: usize,
    references_start: usize,
    field_references_start: usize,
    diagnostics_start: usize,
    pending_calls_start: usize,
    deferred_start: usize,
    globals_created_start: usize,
    function_count_start: usize,
    writes: Vec<Arc<Write>>,
}

/// A call passing a parameter (or `...`) of the code calling it straight on.
/// Its arguments are settled with that parameter at the end of the walk,
/// rather than taken from the previous walk, so a chain of such calls
/// (`a(1)` calling `b(x)` calling `c(y)`) settles in one walk
#[derive(Debug, Clone)]
struct PendingCall {
    callee: FunctionId,
    /// The arguments as the walk saw them, by the callee's parameter positions
    known: Values,
    /// Positions holding a parameter passed on: which function's, which
    /// parameter, and the types it was narrowed to at the call, if it was
    parameters: Vec<(usize, FunctionId, usize, Option<TypeSet>)>,
    /// From this position on, the arguments are that function's own `...`
    varargs: Option<(usize, FunctionId)>,
}

/// How much had been recorded before a loop pass, to discard that pass
struct Snapshot {
    bindings: usize,
    references: usize,
    field_references: usize,
    diagnostics: usize,
    deferred: usize,
    globals_created: usize,
    pending_calls: usize,
    /// So each pass numbers the loop's functions the same, and the functions
    /// after it are numbered the same whatever passes the loop took, which
    /// replaying them needs
    function_count: usize,
}

/// Every file's top level, which run one after another
const MAIN_CHUNK: usize = 0;

/// The table every file of an addon gets as the second value of `...`, by the
/// addon's place in load order. Not a real file position, so it can't clash
/// with a table made in a file
fn addon_namespace(addon: u32) -> TableId {
    Location { file: FileId(u32::MAX - addon), offset: TextSize::new(0), slot: 0 }
}

/// The library functions calls are checked against, found once for each walk
/// rather than at every call
const BUILTIN_PATHS: [&str; 20] = [
    "assert", "getmetatable", "gmatch", "ipairs", "next", "pairs", "pcall", "rawget", "setmetatable", "unpack", "xpcall",
    "string.gmatch", "table.insert", "table.remove", "tinsert", "tremove",
    "CreateFontStringPool", "CreateFramePool", "CreateObjectPool", "CreateTexturePool",
];

/// Passes after which a loop that still hasn't settled is widened: any variable
/// whose type is still changing becomes unknown. Without this, a loop already
/// terminates (each pass adds at least one bit to some variable's TypeSet), but
/// nested loops multiply their passes. Realistic code settles in 2-3 passes
const LOOP_PASSES_BEFORE_WIDENING: usize = 8;

impl<'a> Walker<'a> {
    fn new(prior: Option<&'a Analysis>, definitions: &Arc<Definitions>, layer: &Arc<Layer>, changed: HashSet<Read>, replay: bool) -> Walker<'a> {
        let builtins = BUILTIN_PATHS.iter().map(|&path| (path, definitions.at_path(path))).collect();
        Walker { prior, changed, replay, layer: layer.clone(), definitions: definitions.clone(), builtins, ..Walker::default() }
    }

    /// A global's preset type: what a dependency gives it, else the
    /// definitions'
    fn preset_global(&self, name: &str) -> Option<Type> {
        self.layer.globals.get(name).copied().or_else(|| self.definitions.global(name))
    }

    /// Notes a read for each function body being walked
    fn note(&self, read: Read) {
        // Only the innermost: each body's reads join its parent's when it ends
        if let Some(innermost) = self.reads.borrow_mut().last_mut() {
            innermost.insert(read);
        }
    }

    /// Each file's top level runs in turn, so globals continue from one file to
    /// the next. Locals end with their file
    fn walk(mut self, files: &[SourceFile]) -> Analysis {
        self.roots = files.iter().map(|file| file.root.clone()).collect();

        for (i, file) in files.iter().enumerate() {
            self.file = FileId(i as u32);
            self.addon = file.addon;
            if let Some(block) = Block::cast(file.root.clone()) {
                self.push_scope(false, file.root.text_range().end());
                self.statements(&block);
                self.pop_scope();
            }
        }
        self.finish()
    }

    fn location(&self, offset: TextSize) -> Location {
        Location { file: self.file, offset, slot: 0 }
    }

    fn function_id(&self, def: &FunctionDefinition) -> FunctionId {
        self.location(def.syntax().text_range().start())
    }

    /// What `...` is here. At the top of an addon's file: its name and
    /// namespace table. In a function: what every call passed past its named
    /// parameters, as the previous walk settled it
    fn vararg_values(&self) -> Option<Values> {
        if self.function == MAIN_CHUNK {
            return self.addon.map(|addon| Values {
                values: vec![Type::of(TypeSet::STRING), Type::table(addon_namespace(addon))],
                rest: Type::NIL,
            })
        }
        match self.prior {
            // Nothing known yet: types grow from here, walk by walk
            None => Some(Values::nothing()),
            Some(prior) => {
                let function = self.function_ids.last()?;
                self.note(Read::Varargs(*function));
                prior.vararg_values.get(function).cloned()
            }
        }
    }

    fn finish(mut self) -> Analysis {
        self.settle_parameter_types();
        for binding in self.bindings.iter_mut() {
            if binding.kind == BindingKind::Global && !binding.assigned {
                binding.types = self.layer.globals.get(&*binding.name).copied()
                    .or_else(|| self.definitions.global(&binding.name))
                    .unwrap_or(Type::UNKNOWN);
            }
        }
        let function_fields = self.fields.values().flat_map(|fields| fields.iter())
            .chain(self.any_table_fields.iter())
            .filter(|(_, ty)| ty.set.contains(TypeSet::FUNCTION))
            .map(|(name, _)| name.clone())
            .collect();
        // XML's mixins that are tables here: `self` in their methods is the
        // object XML mixes each into
        let mut lua_mixins: HashMap<String, Type> = HashMap::default();
        for (name, target) in self.definitions.xml_mixins() {
            if !lua_mixins.contains_key(name) {
                let own = self.globals.get(name.as_str()).map(|&id| &self.bindings[id]).filter(|b| b.assigned).map(|b| b.types);
                if let Some(ty) = own.or_else(|| self.layer.globals.get(name.as_str()).copied()) {
                    lua_mixins.insert(name.clone(), ty);
                }
            }
            if let Some(at) = lua_mixins.get(name).and_then(|ty| ty.named.identity()) {
                self.mixed_into.entry(at).or_default().push(NamedRef::One(*target));
            }
        }
        // And what XML makes those objects in, for `self:GetParent()`
        for (name, parent) in self.definitions.xml_parents() {
            let own = self.globals.get(name.as_str()).map(|&id| &self.bindings[id]).filter(|b| b.assigned).map(|b| b.types);
            let mixin = own.or_else(|| self.layer.globals.get(name.as_str()).copied());
            if let Some(at) = mixin.and_then(|ty| ty.named.identity()) {
                *self.parents.entry(at).or_default() |= Type::named(*parent, TypeSet::TABLE);
            }
        }
        let mixed_into = self.mixed_into.keys()
            .filter_map(|&mixin| match self.mixin_target(mixin, 0) {
                MixinTarget::Object(named, extended) => Some((mixin, (named, extended))),
                _ => None,
            })
            .collect();
        Analysis {
            function_fields,
            mixed_into,
            lua_mixins,
            // Filled in by `complete`, once the last walk is known
            table_names: HashMap::default(),
            deferred: self.deferred,
            pending_calls: self.pending_calls,
            records: self.records,
            replayed: self.replayed,
            walked: self.walked,
            pool_objects: self.pool_objects,
            parents: self.parents,
            mixins: self.mixins,
            diagnostics: self.diagnostics,
            bindings: self.bindings,
            references: self.references,
            field_references: self.field_references,
            functions: self.functions,
            definitions: self.definitions,
            fields: self.fields,
            any_table_fields: self.any_table_fields,
            entries: self.entries,
            metatables: self.metatables,
            vararg_values: self.vararg_values,
            walks: 1,
            roots: self.roots,
            layer: self.layer,
        }
    }

    /// Each parameter's type for the next walk: what the body requires, else the
    /// arguments from every call when every call is visible, else unknown
    fn settle_parameter_types(&mut self) {
        let mut only_called = vec![true; self.bindings.len()];
        for reference in &self.references {
            if !reference.write && !reference.callee {
                only_called[reference.binding] = false;
            }
        }
        // Functions read from a field other than to call them, and field names
        // read where it isn't known which function they hold
        let mut escaped = HashSet::default();
        let mut unresolved = HashSet::default();
        for field in self.field_references.iter().filter(|f| !f.write) {
            match field.types.function {
                FunctionRef::One(function) if !field.callee => { escaped.insert(function); }
                FunctionRef::Unknown => { unresolved.insert(field.name()); }
                _ => {}
            }
        }
        let mut all_seen: HashMap<FunctionId, bool> = HashMap::default();
        for id in self.functions.keys() {
            // Every call to it is one we saw: it was assigned straight to a
            // variable (local or global) only ever called, which never holds
            // another function, and any field it was stored in is only ever
            // called, with no reads of a field by that name that could be it
            let in_variable = self.assigned_to.get(id).map(|&b| {
                let binding = &self.bindings[b];
                binding.types.function == FunctionRef::One(*id) && only_called[b]
            });
            let in_fields = self.stored_in_fields.get(id).map(|names| {
                !escaped.contains(id) && names.iter().all(|name| !unresolved.contains(name.as_str()))
            });
            let all_calls_seen = match (in_variable, in_fields) {
                (None, None) => self.pool_callbacks.contains(id),
                (variable, fields) => variable.unwrap_or(true) && fields.unwrap_or(true),
            };
            all_seen.insert(*id, all_calls_seen);
        }
        let definitions = &self.definitions;
        // A table or object made in the files that meets the requirement is
        // kept, so what it holds and its fields aren't lost to a plain `table`
        // Any of several objects meets it when each does
        let meets = |passed: Type, mandated: Type| {
            let objects = match passed.named {
                NamedRef::Several(_) => passed.named.alternatives(),
                named => vec![named],
            };
            mandated.set.contains(passed.set) && objects.iter().all(|object| {
                object.identity().is_some()
                    && mandated.named.named_id().is_none_or(|m| object.named_id().is_some_and(|p| definitions.is_a(p, m)))
            })
        };
        let mut pending: HashMap<FunctionId, Vec<&PendingCall>> = HashMap::default();
        // The functions each function's parameters are passed on to
        let mut dependents: HashMap<FunctionId, Vec<FunctionId>> = HashMap::default();
        for call in &self.pending_calls {
            pending.entry(call.callee).or_default().push(call);
            let sources = call.parameters.iter().map(|&(_, function, _, _)| function).chain(call.varargs.map(|(_, function)| function));
            for source in sources {
                let callees = dependents.entry(source).or_default();
                if !callees.contains(&call.callee) {
                    callees.push(call.callee);
                }
            }
        }
        // Calls passing a parameter on take it as settled here, so parameters
        // and `...` start from nothing known and grow until they agree. They
        // only grow, and only so far, so this ends. A worklist keeps it to
        // the functions something new is passed to
        let mut ids: Vec<FunctionId> = self.functions.keys().copied().collect();
        ids.sort();
        let mut parameters: HashMap<FunctionId, Vec<Type>> = ids.iter()
            .map(|id| (*id, vec![Type::default(); self.functions[id].parameters.len()]))
            .collect();
        let mut varargs: HashMap<FunctionId, Values> = ids.iter()
            .map(|id| (*id, match self.functions[id].varargs.is_some() && all_seen[id] {
                true => Values { values: Vec::new(), rest: Type::default() },
                false => Values::unknown(),
            }))
            .collect();
        // Each function once, then again whenever what's passed on to it grows
        let mut queue: std::collections::VecDeque<FunctionId> = ids.iter().copied().collect();
        let mut queued: HashSet<FunctionId> = ids.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            queued.remove(&id);
            let id = &id;
            let mut changed = false;
            {
                let function = &self.functions[id];
                let mut passed = self.call_arguments.get(id).cloned();
                for call in pending.get(id).into_iter().flatten() {
                    let resolved = resolve_pending(call, &parameters, &varargs);
                    passed = Some(match passed {
                        Some(passed) => passed.union(&resolved),
                        None => resolved,
                    });
                }
                for (i, parameter) in function.parameters.iter().enumerate() {
                    let ty = match (parameter.mandated, &passed) {
                        (Some(mandated), Some(passed)) if all_seen[id] && meets(passed.get(i), mandated) => passed.get(i),
                        (Some(mandated), _) => mandated,
                        (None, Some(passed)) if all_seen[id] => passed.get(i),
                        _ => Type::UNKNOWN,
                    };
                    let slot = &mut parameters.get_mut(id).expect("every function")[i];
                    if *slot | ty != *slot {
                        *slot |= ty;
                        changed = true;
                    }
                }
                if let (true, true, Some(passed)) = (function.varargs.is_some(), all_seen[id], &passed) {
                    let values = Values {
                        values: passed.values.iter().skip(function.parameters.len()).copied().collect(),
                        rest: passed.rest,
                    };
                    let slot = varargs.get_mut(id).expect("every function");
                    let grown = slot.union(&values);
                    if grown != *slot {
                        *slot = grown;
                        changed = true;
                    }
                }
            }
            if changed {
                for &dependent in dependents.get(id).into_iter().flatten() {
                    // A dependency's function: only that addon settles its parameters
                    if !self.functions.contains_key(&dependent) {
                        continue
                    }
                    if queued.insert(dependent) {
                        queue.push_back(dependent);
                    }
                }
            }
        }
        for (id, function) in self.functions.iter_mut() {
            for (parameter, &ty) in function.parameters.iter_mut().zip(&parameters[id]) {
                parameter.types = ty;
            }
            let has_calls = self.call_arguments.contains_key(id) || pending.contains_key(id);
            if let (Some(shown), true, true) = (&mut function.varargs, all_seen[id], has_calls) {
                let values = varargs[id].clone();
                // For the signature: anything one of them can be. Some calls
                // passing fewer isn't a nil worth showing
                let mut any = values.values.iter().fold(Type::default(), |all, &v| all | v.without(TypeSet::NIL));
                if values.rest != Type::NIL {
                    any |= values.rest.without(TypeSet::NIL);
                }
                // Not knowing them at all is just unknown, not `unknown (not nil)`
                let unknown = any == Type::default() || any.set == TypeSet::UNKNOWN.without(TypeSet::NIL);
                shown.types = if unknown { Type::UNKNOWN } else { any };
                self.vararg_values.insert(*id, values);
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            bindings: self.bindings.len(),
            references: self.references.len(),
            field_references: self.field_references.len(),
            diagnostics: self.diagnostics.len(),
            deferred: self.deferred.len(),
            globals_created: self.globals_created.len(),
            pending_calls: self.pending_calls.len(),
            function_count: self.function_count,
        }
    }

    /// Forgets the bindings and references recorded since the snapshot. Changes
    /// to older bindings are kept: their current types are what's being carried
    /// round the loop, and their all-assignment unions only grow pass to pass
    fn restore(&mut self, snapshot: &Snapshot) {
        self.bindings.truncate(snapshot.bindings);
        self.current.truncate(snapshot.bindings);
        self.references.truncate(snapshot.references);
        self.field_references.truncate(snapshot.field_references);
        self.diagnostics.truncate(snapshot.diagnostics);
        self.deferred.truncate(snapshot.deferred);
        self.pending_calls.truncate(snapshot.pending_calls);
        self.function_count = snapshot.function_count;
        for name in self.globals_created.drain(snapshot.globals_created..) {
            self.globals.remove(&name);
        }
    }

    /// Walks a loop body (including any condition checked each iteration)
    /// repeatedly, feeding the types at the end back in at the start, until they
    /// stop changing. Assignments inside a loop only ever widen the types of
    /// variables from outside it, so this converges. Only the final pass's
    /// bindings and references are kept.
    fn loop_body(&mut self, mut body: impl FnMut(&mut Walker)) {
        for pass in 1.. {
            let snapshot = self.snapshot();
            let log_start = self.loop_log.len();
            self.loop_depth += 1;
            body(self);
            self.loop_depth -= 1;
            // Each older binding the pass set, with its type on entry: the
            // first logged. Those it ends without changing don't count
            let mut seen = HashSet::default();
            let changed: Vec<BindingId> = self.loop_log[log_start..].iter()
                .filter(|&&(id, _)| id < snapshot.bindings && seen.insert(id))
                .filter(|&&(id, entry)| self.current[id] != entry)
                .map(|&(id, _)| id)
                .collect();
            // Kept for the loops around this one, which compare what they changed
            if self.loop_depth == 0 {
                self.loop_log.clear();
            }
            if changed.is_empty() {
                return
            }
            if pass >= LOOP_PASSES_BEFORE_WIDENING {
                // Each further pass makes at least one more variable unknown
                for id in changed {
                    self.set_current(id, Type::UNKNOWN);
                }
            }
            self.restore(&snapshot);
        }
    }

    /// Sets a binding's current type, noting the type before when in a loop
    fn set_current(&mut self, id: BindingId, types: Type) {
        if self.loop_depth > 0 && self.current[id] != types {
            self.loop_log.push((id, self.current[id]));
        }
        self.current[id] = types;
    }

    fn push_scope(&mut self, conditional: bool, end: TextSize) {
        self.scopes.push(Scope { names: HashMap::default(), conditional, end });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn new_binding(&mut self, name: &str, kind: BindingKind, declaration: Option<TextRange>, types: Type, function: usize, scope_depth: usize) -> BindingId {
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
        });
        self.current.push(types);
        id
    }

    fn declare(&mut self, token: &SyntaxToken, kind: BindingKind, mut types: Type) -> BindingId {
        if types.named == NamedRef::Unknown {
            // A table we can't identify: track its fields under this variable
            types.named = NamedRef::Table(Location { slot: PSEUDO, ..self.location(token.text_range().start()) });
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

    fn resolve(&mut self, name: &str) -> BindingId {
        for scope in self.scopes.iter().rev() {
            if let Some(&id) = scope.names.get(name) {
                return id;
            }
        }
        self.resolve_global(name)
    }

    /// The global `name`, even where a local hides it (as `_G.name` reaches it)
    fn resolve_global(&mut self, name: &str) -> BindingId {
        if let Some(&id) = self.globals.get(name) {
            return id;
        }
        let id = self.new_binding(name, BindingKind::Global, None, Type::default(), MAIN_CHUNK, 0);
        self.set_current(id, Type::UNKNOWN);
        self.globals.insert(name.to_string(), id);
        self.globals_created.push(name.to_string());
        id
    }

    fn is_global(&self, name: &str) -> bool {
        !self.scopes.iter().any(|s| s.names.contains_key(name))
    }

    fn read(&mut self, token: &SyntaxToken) -> Type {
        let id = self.resolve(token.text());
        self.read_binding(token, id)
    }

    /// A read of binding `id` at `token`
    fn read_binding(&mut self, token: &SyntaxToken, id: BindingId) -> Type {
        let index = self.references.len();
        let types = if self.bindings[id].function == self.function {
            self.current[id]
        } else {
            // An upvalue: the closure could run after any assignment
            self.deferred.push(index);
            self.prior_binding_types(id)
        };
        let types = self.builtin_global(id).unwrap_or(types);
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: false, callee: false, call_returns: None });
        types
    }

    /// Every value the previous walk saw assigned to this binding. Bindings are
    /// created in the same order on every walk, so the ids line up. On the
    /// first walk nothing is known yet, so types grow from nothing rather
    /// than shrinking from unknown: a cycle (a field written with what's read
    /// from it) then settles on what really flows round it, where starting
    /// from unknown would keep it unknown forever
    fn prior_binding_types(&self, id: BindingId) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::Binding(id));
        prior.bindings.get(id)
            .filter(|prior| prior.name == self.bindings[id].name)
            .map_or(Type::UNKNOWN, |prior| prior.types)
    }

    /// True for a global the file never assigns (as far as this walk has seen
    /// on the first walk, anywhere on the second), so presets can apply
    fn is_unassigned_global(&self, id: BindingId) -> bool {
        self.note(Read::Binding(id));
        let binding = &self.bindings[id];
        let prior = self.prior.and_then(|p| p.bindings.get(id)).filter(|p| p.name == binding.name);
        binding.kind == BindingKind::Global && !binding.assigned && prior.is_none_or(|p| !p.assigned)
    }

    fn builtin_global(&self, id: BindingId) -> Option<Type> {
        if !self.is_unassigned_global(id) {
            return None
        }
        self.preset_global(&self.bindings[id].name)
    }

    fn write(&mut self, token: &SyntaxToken, types: Type) {
        let id = self.resolve(token.text());
        self.write_binding(token, id, types)
    }

    /// A write of binding `id` at `token`
    fn write_binding(&mut self, token: &SyntaxToken, id: BindingId, types: Type) {
        let binding = &mut self.bindings[id];
        binding.types |= types;
        binding.assigned = true;
        binding.reassigned = true;
        let innermost_conditional = self.scopes.iter().rposition(|s| s.conditional);
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

    fn block(&mut self, block: Option<Block>, conditional: bool) {
        if let Some(block) = block {
            self.push_scope(conditional, block.syntax().text_range().end());
            self.statements(&block);
            self.pop_scope();
        }
    }

    fn statements(&mut self, block: &Block) {
        for child in block.syntax().children() {
            match Statement::cast(child.clone()) {
                Some(statement) => self.statement(&statement),
                // Half-typed code like `frame.` that isn't a statement yet: its
                // names are still read, for hover and completion
                None if child.kind() == SyntaxKind::Identifier => {
                    if let Some(expression) = Expression::cast(child) {
                        self.expression(&expression);
                    }
                }
                None => {}
            }
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::LocalAssign(s) => {
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, names.len());
                // `local x = x` reads the outer `x`: the new one is seen after the
                // statement's code, not counting the whitespace it ends with
                // Back from its last token, as a value can be a whole function
                let start = s.syntax().text_range().start();
                let after = std::iter::successors(s.syntax().last_token(), |t| t.prev_token())
                    .take_while(|t| t.text_range().start() >= start)
                    .find(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment))
                    .map_or(s.syntax().text_range().end(), |t| t.text_range().end());
                // `local x =` with its value still to be typed: what follows is the value
                let unfinished = expressions.is_empty()
                    && s.syntax().children_with_tokens().any(|e| e.kind() == SyntaxKind::Assign);
                for (i, (name, types)) in names.iter().zip(types).enumerate() {
                    let binding = self.declare(name, BindingKind::Local, types);
                    if let Some(visible) = &mut self.bindings[binding].visible {
                        let start = if unfinished { visible.end() } else { after.min(visible.end()) };
                        *visible = TextRange::new(start, visible.end());
                    }
                    if let Some(Expression::Function(def)) = expressions.get(i) {
                        self.apply(Write::AssignedTo(self.function_id(def), binding));
                    }
                }
            }
            Statement::Assign(s) => {
                let targets = s.variable_list().map(|v| v.identifiers()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, targets.len());
                for (i, (target, types)) in targets.iter().zip(types).enumerate() {
                    match (target.simple_name(), self.global_table_access(target.syntax())) {
                        (Some(name), _) => {
                            self.write(&name, types);
                            if let Some(Expression::Function(def)) = expressions.get(i) {
                                let binding = self.resolve(name.text());
                                self.apply(Write::AssignedTo(self.function_id(def), binding));
                            }
                        }
                        // `_G.name = v`: the global
                        (None, Some((token, name))) => {
                            self.identifier_parts(target.syntax());
                            let id = self.resolve_global(&name);
                            self.write_binding(&token, id, types);
                        }
                        (None, None) => match field_access(target.syntax()) {
                            Some((base, field)) => {
                                let base = self.expression(&base);
                                self.field_write(base, field.text(), types);
                                self.field_reference(target.syntax(), &field, base, types, true);
                            }
                            None => match index_access(target.syntax()) {
                                // `t[k] = v`
                                Some((base, key)) => {
                                    let base = self.expression(&base);
                                    match self.index_key(&key) {
                                        Key::Name(name, token) => {
                                            self.field_write(base, &name, types);
                                            if let Some(token) = token {
                                                self.field_reference(target.syntax(), &token, base, types, true);
                                            }
                                        }
                                        Key::Other(key) => self.entry_write(base, key, types),
                                    }
                                }
                                None => self.identifier_parts(target.syntax()),
                            },
                        },
                    }
                }
            }
            Statement::FunctionCall(call) => {
                self.function_call(call);
                // Past `assert(v)`, `v` is true: otherwise it raised an error
                if self.calls_library_global(call, "assert") {
                    if let Some(condition) = call.arguments().first() {
                        for (id, ty) in self.narrowing(condition).when_true {
                            self.set_current(id, ty);
                        }
                    }
                }
            }
            Statement::Do(s) => self.block(s.block(), false),
            Statement::While(s) => self.loop_body(|w| {
                let proved = s.condition().map(|condition| {
                    w.expression(&condition);
                    w.narrowing(&condition)
                }).unwrap_or_default();
                w.with_narrowing(&proved.when_true, |w| w.block(s.block(), true));
            }),
            Statement::Repeat(s) => self.loop_body(|w| {
                // The `until` condition can see the block's locals
                w.push_scope(true, s.syntax().text_range().end());
                if let Some(block) = s.block() {
                    w.statements(&block);
                }
                if let Some(condition) = s.condition() {
                    w.expression(&condition);
                }
                w.pop_scope();
            }),
            Statement::If(s) => {
                // Each branch runs only if the conditions before it were false
                let mut earlier_false: Vec<(BindingId, Type)> = Vec::new();
                // Without an `else`, code after an `if` whose branches all
                // return runs only when every condition was false
                let mut all_return = s.else_branch().is_none();
                for (i, branch) in s.if_branches().iter().enumerate() {
                    let block = branch.block();
                    all_return &= block.as_ref().is_some_and(always_returns);
                    let proved = self.with_narrowing(&earlier_false.clone(), |w| {
                        let condition = branch.expression()?;
                        // An `elseif` condition only runs if the ones before failed
                        w.short_circuit_depth += usize::from(i > 0);
                        w.expression(&condition);
                        w.short_circuit_depth -= usize::from(i > 0);
                        let proved = w.narrowing(&condition);
                        w.with_narrowing(&both(earlier_false.clone(), proved.when_true.clone()), |w| w.block(block, true));
                        Some(proved)
                    });
                    match proved {
                        Some(proved) => earlier_false = both(earlier_false, proved.when_false),
                        None => self.block(branch.block(), true),
                    }
                }
                if let Some(branch) = s.else_branch() {
                    self.with_narrowing(&earlier_false.clone(), |w| w.block(branch.block(), true));
                }
                if all_return {
                    for (id, ty) in earlier_false {
                        self.set_current(id, ty);
                    }
                }
            }
            Statement::ForCountLoop(s) => {
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                for e in &expressions {
                    self.expression(e);
                }
                self.loop_body(|w| {
                    w.push_scope(true, s.syntax().text_range().end());
                    if let Some(name) = s.name_token() {
                        w.declare(&name, BindingKind::Local, Type::of(TypeSet::NUMBER));
                    }
                    if let Some(block) = s.block() {
                        w.statements(&block);
                    }
                    w.pop_scope();
                });
            }
            Statement::ForInLoop(s) => {
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let iterator = self.iterator_name(&expressions);
                let iterator_function = expressions.iter().map(|e| self.expression(e)).next();
                // `for object in pool:EnumerateActive()`
                let enumerated = match expressions.as_slice() {
                    [Expression::FunctionCall(call)] => self.enumerated.get(&self.location(call.syntax().text_range().start())).copied(),
                    _ => None,
                };
                // `for k, v in pairs(t)` over a table whose contents are known, or
                // `for w in s:gmatch(p)`
                let iterated = match expressions.as_slice() {
                    [Expression::FunctionCall(call)] => self.iterated.get(&self.location(call.syntax().text_range().start())).cloned(),
                    _ => None,
                };
                // Otherwise each pass calls the iterator, so the variables are
                // what it returns, if it's a function we know
                let iterated = iterated.or_else(|| {
                    let function = iterator_function?.function;
                    match function {
                        FunctionRef::One(id) => Some(self.returns_of(id)),
                        FunctionRef::Builtin(_) => self.known_function(function).map(|f| f.returns.clone()),
                        _ => None,
                    }
                });
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                self.loop_body(|w| {
                    w.push_scope(true, s.syntax().text_range().end());
                    for (i, name) in names.iter().enumerate() {
                        if let (0, Some(object)) = (i, enumerated) {
                            w.declare(name, BindingKind::Local, object);
                            continue
                        }
                        if let Some(values) = &iterated {
                            // The loop stops when the first variable is nil
                            let types = if i == 0 { values.get(0).without(TypeSet::NIL) } else { values.get(i) };
                            w.declare(name, BindingKind::Local, types);
                            continue
                        }
                        let types = match (i, iterator.as_deref()) {
                            (0, Some("ipairs")) => TypeSet::NUMBER,
                            (1, Some("ipairs" | "pairs")) => TypeSet::UNKNOWN.without(TypeSet::NIL),
                            // The loop stops when the first variable is nil
                            (0, _) => TypeSet::UNKNOWN.without(TypeSet::NIL),
                            _ => TypeSet::UNKNOWN,
                        };
                        w.declare(name, BindingKind::Local, Type::of(types));
                    }
                    if let Some(block) = s.block() {
                        w.statements(&block);
                    }
                    w.pop_scope();
                });
            }
            Statement::FunctionDefinition(def) => {
                if def.is_local() {
                    // Declared before the body so the function can call itself
                    if let Some(name) = def.local_name() {
                        let binding = self.declare(&name, BindingKind::Local, Type::function(self.function_id(def)));
                        self.apply(Write::AssignedTo(self.function_id(def), binding));
                    }
                    self.function_body(def, None);
                } else if let Some(identifier) = def.identifier() {
                    match identifier.simple_name() {
                        Some(name) => {
                            self.write(&name, Type::function(self.function_id(def)));
                            let binding = self.resolve(name.text());
                            self.apply(Write::AssignedTo(self.function_id(def), binding));
                            self.function_body(def, None);
                        }
                        // `function a.b:c()` assigns field `c` of `a.b`
                        None => {
                            let owner = self.function_name_owner(&identifier);
                            if let Some((owner, field)) = &owner {
                                let function = Type::function(self.function_id(def));
                                self.field_write(*owner, field.text(), function);
                                self.field_reference(identifier.syntax(), field, *owner, function, true);
                            }
                            // `self` is the table the method is defined on
                            let self_type = identifier.is_call_to_self().then(|| {
                                let owner = owner.map_or(Type::UNKNOWN, |(owner, _)| owner);
                                match owner.named {
                                    // Unknown, so assume a table, with fields of its own
                                    NamedRef::None | NamedRef::Unknown => Type::table(self.function_id(def)),
                                    // A mixin: `self` is the WoW object it's mixed into, with
                                    // the mixin's fields too
                                    NamedRef::Table(at) => match { self.note(Read::MixedInto(at)); self.prior.and_then(|prior| prior.mixed_into.get(&at)).or_else(|| self.layer.mixed_into.get(&at)) } {
                                        Some(&(named, extended)) => Type { named: NamedRef::Instance { named, at, extended }, ..Type::of(TypeSet::TABLE) },
                                        None => Type { named: owner.named, ..Type::of(TypeSet::TABLE) },
                                    },
                                    named => {
                                        let set = owner.set.intersection(OBJECTS);
                                        let set = if set == TypeSet::NEVER { TypeSet::TABLE } else { set };
                                        Type { named, ..Type::of(set) }
                                    }
                                }
                            });
                            self.function_body(def, self_type);
                        }
                    }
                }
            }
            Statement::Return(s) => {
                let values = self.expression_values(&s.expressions());
                if !self.fresh.is_empty() {
                    let made: Vec<Option<FreshObject>> = values.values.iter()
                        .map(|value| {
                            let made = value.named.identity().filter(|&at| self.made_here(at))?;
                            let mixed_parameters = self.parameter_mixins.get(&made).cloned().unwrap_or_default();
                            let pool_mixed_parameters = { self.note(Read::PoolObject(made)); self.pool_objects.get(&made) }.and_then(|object| object.named.identity())
                                .map(|object| self.parameter_mixins.get(&object).cloned().unwrap_or_default());
                            Some(FreshObject { made, mixed_parameters, pool_mixed_parameters })
                        })
                        .collect();
                    if let Some(fresh) = self.fresh.last_mut() {
                        *fresh = Some(match fresh.take() {
                            None => made,
                            // Every return has to agree on what's new
                            Some(earlier) => (0..earlier.len().max(made.len()))
                                .map(|i| match (earlier.get(i), made.get(i)) {
                                    (Some(Some(a)), Some(Some(b))) if a == b => Some(a.clone()),
                                    _ => None,
                                })
                                .collect(),
                        });
                    }
                }
                // At the top of the file there's no function to return from
                if let Some(returns) = self.returns.last_mut() {
                    *returns = Some(match returns.take() {
                        Some(earlier) => earlier.union(&values),
                        None => values,
                    });
                }
            }
        }
    }

    /// `pairs`/`ipairs` when they are the unshadowed globals
    fn iterator_name(&self, expressions: &[Expression]) -> Option<String> {
        let Some(Expression::FunctionCall(call)) = expressions.first() else { return None };
        let Some(Expression::Identifier(callee)) = call.callee() else { return None };
        let name = callee.simple_name()?;
        let name = name.text();
        (matches!(name, "pairs" | "ipairs") && self.is_global(name)).then(|| name.to_string())
    }

    /// `_G.name` or `_G["name"]`, while `_G` is the globals table: the token
    /// naming the global (the string literal for `_G["name"]`) and its name
    fn global_table_access(&self, identifier: &SyntaxNode) -> Option<(SyntaxToken, String)> {
        let is_globals = |base: &Expression| matches!(base, Expression::Identifier(i)
            if i.simple_name().is_some_and(|n| n.text() == "_G") && self.is_global("_G"));
        if let Some((base, field)) = field_access(identifier) {
            return is_globals(&base).then(|| (field.clone(), field.text().to_string()))
        }
        let (base, Expression::Literal(key)) = index_access(identifier)? else { return None };
        if !is_globals(&base) {
            return None
        }
        let name = string_field_name(&key.get_string()?)?.to_string();
        let token = key.syntax().descendants_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == SyntaxKind::String)?;
        Some((token, name))
    }

    /// The table and field a `function a.b:c()` statement assigns: `a.b` and `c`
    fn function_name_owner(&mut self, identifier: &Identifier) -> Option<(Type, SyntaxToken)> {
        let names: Vec<SyntaxToken> = identifier.syntax().children_with_tokens()
            .filter_map(|t| t.into_token())
            .filter(|t| t.kind() == SyntaxKind::Name)
            .collect();
        let (first, rest) = names.split_first()?;
        let (last, middle) = rest.split_last()?;
        let mut owner = self.read(first);
        for name in middle {
            let base = owner;
            owner = self.field_read(base, name.text());
            self.field_reference(identifier.syntax(), name, base, owner, false);
        }
        Some((owner, last.clone()))
    }

    /// `access` is the code reaching the field (`a.b` for `b`), or for a table
    /// constructor's key, the field
    /// entry. `base` is what the field is on
    fn field_reference(&mut self, access: &SyntaxNode, name: &SyntaxToken, base: Type, types: Type, write: bool) {
        let table = base.named.identity();
        self.field_references.push(FieldReference { file: self.file, range: name.text_range(), access: crate::syntax::SyntaxNodePtr::new(access), token: name.green().to_owned(), types, table, write, callee: false, call_returns: None });
    }

    /// The field read just recorded is the function being called
    fn last_field_is_callee(&mut self) {
        if let Some(field) = self.field_references.last_mut() {
            field.callee = true;
        }
    }

    /// The type of field `name` of a value of type `base`
    fn field_read(&self, base: Type, name: &str) -> Type {
        // Any of several objects: what each has, which is only known for a
        // field all of them have
        if let NamedRef::Several(_) = base.named {
            return base.named.alternatives().into_iter()
                .map(|object| self.field_read(Type { named: object, ..base }, name))
                .reduce(Type::union)
                .unwrap_or(Type::UNKNOWN)
        }
        let base = self.definitions.member_source(base);
        if let Some(declared) = self.definitions.field(base, name) {
            return declared
        }
        // Writes can come later in the file, so only a previous walk knows them all
        let Some(prior) = self.prior else { return Type::default() };
        let own = match base.named {
            NamedRef::Table(id) | NamedRef::Instance { at: id, .. } => self.own_field(prior, id, name, 0),
            NamedRef::One(_) | NamedRef::Extended(_) => None,
            NamedRef::Several(_) => unreachable!("read from each object"),
            // Not a table we know, so nothing to go on
            NamedRef::None | NamedRef::Unknown => return Type::UNKNOWN,
        };
        // What the table's own code (or a mixin's) sets wins. Writes to tables of
        // unknown identity are only a fallback: one field name is reused by many
        // unrelated tables (`entry.texts.cooldown` and `pools.cooldown`), so
        // merging them in would lose what's known about this one
        match own {
            Some(own) => own,
            None => {
                self.note(Read::AnyTableField(name_key(name)));
                let own = prior.any_table_fields.get(name).copied();
                let found = match (own, self.layer.any_table_fields.get(name).copied()) {
                    (Some(a), Some(b)) => a | b,
                    (Some(found), None) | (None, Some(found)) => found,
                    (None, None) => Type::UNKNOWN,
                };
                // Some other table's field, perhaps, so not a type to report
                // missing methods by
                Type { named: found.named.loosened(), ..found }
            }
        }
    }

    /// A field of a table made in the files: its own, else one it got from its
    /// mixins (the last mixin first, as later ones overwrite earlier ones)
    fn own_field(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
        self.note(Read::Field(table, name_key(name)));
        // A dependency's table: what it gave the table, and what's written to it here
        let layered = (table.file.0 >= LAYER_FILES).then(|| self.layer.field(table, name, 0)).flatten();
        let own = prior.fields.get(&table).and_then(|fields| fields.get(name)).copied();
        match (layered, own) {
            (Some(a), Some(b)) => return Some(a | b),
            (Some(found), None) | (None, Some(found)) => return Some(found),
            (None, None) => {}
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
    fn metamethod(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
        self.note(Read::Metatable(table));
        if let Some(metatable) = self.layer.metatables.get(&table).and_then(|m| m.named.identity()) {
            if !prior.metatables.contains_key(&table) {
                return self.own_field(prior, metatable, name, depth + 1)
            }
        }
        let metatable = prior.metatables.get(&table)?.named.identity()?;
        self.own_field(prior, metatable, name, depth + 1)
    }

    /// Makes a change to what the walk is gathering, recording it for each
    /// function body being walked
    fn apply(&mut self, write: Write) {
        self.effect(&write);
        self.record_write(write);
    }

    /// What a write changes, without noting it for the bodies being walked
    fn effect(&mut self, write: &Write) {
        /// The entry for `name`, copying the name only when it's new
        fn named<'m, V: Default>(map: &'m mut HashMap<String, V>, name: &str) -> &'m mut V {
            if !map.contains_key(name) {
                map.insert(name.to_string(), V::default());
            }
            map.get_mut(name).expect("inserted above")
        }
        match write {
            Write::Field(at, name, value) => *named(self.fields.entry(*at).or_default(), name) |= *value,
            Write::AnyTableField(name, value) => *named(&mut self.any_table_fields, name) |= *value,
            Write::StoredInField(function, name) => {
                let names = self.stored_in_fields.entry(*function).or_default();
                if !names.contains(name) {
                    names.insert(name.clone());
                }
            }
            Write::Entry(at, key, value) => {
                let entries = self.entries.entry(*at).or_default();
                entries.keys |= *key;
                entries.values |= *value;
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
            Write::MixedInto(mixin, object) => {
                let recorded = self.mixed_into.entry(*mixin).or_default();
                if !recorded.contains(object) {
                    recorded.push(*object);
                }
            }
            Write::PoolCallback(function) => { self.pool_callbacks.insert(*function); }
            Write::AssignedTo(function, binding) => { self.assigned_to.insert(*function, *binding); }
            Write::ParameterMixin(at, parameter) => {
                let recorded = self.parameter_mixins.entry(*at).or_default();
                if !recorded.contains(parameter) {
                    recorded.push(*parameter);
                }
            }
            Write::Parent(at, parent) => *self.parents.entry(*at).or_default() |= *parent,
            Write::PoolObject(at, object) => { self.pool_objects.insert(*at, *object); }
            Write::Function(id, info) => { self.functions.insert(*id, info.clone()); }
            Write::Binding(id, types) => {
                let binding = &mut self.bindings[*id];
                binding.types |= *types;
                binding.assigned = true;
                binding.reassigned = true;
                self.set_current(*id, self.current[*id] | *types);
            }
        }
    }

    /// Notes a change for each function body being walked. A variable's
    /// assignment only matters to bodies it was declared outside of
    fn record_write(&mut self, write: Write) {
        // Only the innermost: each body's writes join its parent's when it ends
        let Some(innermost) = self.frames.last_mut() else { return };
        if let Write::Binding(id, _) = write {
            if id >= innermost.bindings_start {
                return
            }
        }
        innermost.writes.push(Arc::new(write));
    }

    /// A body's reads and writes become its parent's too, as the parent
    /// replayed does all the body did
    fn pass_to_parent(&mut self, reads: &HashSet<Read>, writes: &[Arc<Write>]) {
        if let Some(parent) = self.reads.borrow_mut().last_mut() {
            parent.extend(reads.iter().cloned());
        }
        if let Some(parent) = self.frames.last_mut() {
            for write in writes {
                // An assignment to a variable the parent declared is the parent's own
                if let Write::Binding(id, _) = **write {
                    if id >= parent.bindings_start {
                        continue
                    }
                }
                parent.writes.push(write.clone());
            }
        }
    }

    fn field_write(&mut self, base: Type, name: &str, value: Type) {
        if let FunctionRef::One(function) = value.function {
            self.apply(Write::StoredInField(function, name.to_string()));
        }
        match base.named {
            NamedRef::Table(id) | NamedRef::Instance { at: id, .. } => self.apply(Write::Field(id, name.to_string(), value)),
            // Could be any table, or shared by every object of a WoW type
            NamedRef::One(_) | NamedRef::Extended(_) | NamedRef::Unknown => self.apply(Write::AnyTableField(name.to_string(), value)),
            // Whichever object it is gets it, so each may have it. Also kept
            // for tables whose identity isn't known, as the objects' mixins'
            // `self` (the mixin table) reads what's set on the objects there
            NamedRef::Several(_) => {
                for object in base.named.alternatives() {
                    if let Some(at) = object.identity() {
                        self.apply(Write::Field(at, name.to_string(), value));
                    }
                }
                self.apply(Write::AnyTableField(name.to_string(), value));
            }
            NamedRef::None => {}
        }
    }

    fn function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>) -> FunctionId {
        let id = self.function_id(def);
        if let Some(record) = self.replayable(id, self_type) {
            self.replay(id, &record);
            return id
        }
        self.walked += 1;
        self.frames.push(Frame {
            bindings_start: self.bindings.len(),
            references_start: self.references.len(),
            field_references_start: self.field_references.len(),
            diagnostics_start: self.diagnostics.len(),
            pending_calls_start: self.pending_calls.len(),
            deferred_start: self.deferred.len(),
            globals_created_start: self.globals_created.len(),
            function_count_start: self.function_count,
            writes: Vec::new(),
        });
        self.reads.borrow_mut().push(Default::default());
        self.walk_function_body(def, self_type, id);
        let frame = self.frames.pop().expect("pushed above");
        let reads = self.reads.borrow_mut().pop().expect("pushed above");
        let record = Record {
            self_type,
            with_prior: self.prior.is_some(),
            bindings_start: frame.bindings_start,
            function_count_start: frame.function_count_start,
            bindings: self.bindings[frame.bindings_start..].into(),
            current: self.current[frame.bindings_start..].into(),
            references: frame.references_start..self.references.len(),
            field_references: frame.field_references_start..self.field_references.len(),
            diagnostics: frame.diagnostics_start..self.diagnostics.len(),
            pending_calls: frame.pending_calls_start..self.pending_calls.len(),
            deferred: self.deferred[frame.deferred_start..].iter().map(|index| index - frame.references_start).collect(),
            globals: self.globals_created[frame.globals_created_start..].iter()
                .filter_map(|name| Some((name.clone(), *self.globals.get(name)?)))
                .collect(),
            function_count: self.function_count - frame.function_count_start,
            writes: frame.writes.into(),
            reads: Arc::new(reads),
        };
        self.mark_dirty(id, &record);
        self.pass_to_parent(&record.reads, &record.writes);
        self.records.insert(id, Arc::new(record));
        id
    }

    /// The record of this body from the previous walk, if none of what it
    /// read has changed since, and the walk has got to the same place
    fn replayable(&self, id: FunctionId, self_type: Option<Type>) -> Option<Arc<Record>> {
        if !self.replay {
            return None
        }
        let record = self.prior?.records.get(&id)?;
        let same_place = record.with_prior
            && record.self_type == self_type
            && record.bindings_start == self.bindings.len()
            && record.function_count_start == self.function_count
            && record.globals.iter().all(|(name, _)| !self.globals.contains_key(name));
        let unchanged = || record.reads.iter().all(|read| !self.changed.contains(read) && !self.dirty.contains(read));
        (same_place && unchanged()).then(|| record.clone())
    }

    /// Does what walking the body did, as recorded on the previous walk
    fn replay(&mut self, id: FunctionId, record: &Record) {
        let prior = self.prior.expect("only records from a previous walk are replayed");
        self.replayed += 1;
        let references_start = self.references.len();
        let field_references_start = self.field_references.len();
        let diagnostics_start = self.diagnostics.len();
        let pending_calls_start = self.pending_calls.len();
        self.bindings.extend(record.bindings.iter().cloned());
        self.current.extend(record.current.iter().copied());
        self.references.extend(prior.references[record.references.clone()].iter().cloned());
        self.field_references.extend(prior.field_references[record.field_references.clone()].iter().cloned());
        self.diagnostics.extend(prior.diagnostics[record.diagnostics.clone()].iter().cloned());
        self.pending_calls.extend(prior.pending_calls[record.pending_calls.clone()].iter().cloned());
        self.deferred.extend(record.deferred.iter().map(|index| index + references_start));
        for (name, binding) in record.globals.iter() {
            self.globals.insert(name.clone(), *binding);
            self.globals_created.push(name.clone());
        }
        self.function_count += record.function_count;
        // Recorded for the parent all at once below, rather than one by one
        for write in record.writes.iter() {
            self.effect(write);
        }
        self.pass_to_parent(&record.reads, &record.writes);
        // The same again for the next walk, at this walk's places
        let replayed = Record {
            self_type: record.self_type,
            with_prior: true,
            bindings_start: record.bindings_start,
            function_count_start: record.function_count_start,
            bindings: Arc::clone(&record.bindings),
            current: Arc::clone(&record.current),
            references: references_start..self.references.len(),
            field_references: field_references_start..self.field_references.len(),
            diagnostics: diagnostics_start..self.diagnostics.len(),
            pending_calls: pending_calls_start..self.pending_calls.len(),
            deferred: Arc::clone(&record.deferred),
            globals: Arc::clone(&record.globals),
            function_count: record.function_count,
            writes: Arc::clone(&record.writes),
            reads: Arc::clone(&record.reads),
        };
        self.records.insert(id, Arc::new(replayed));
    }

    /// Bodies later in this walk read some of what a body writes as it is on
    /// this walk, not the previous one: a function's returns, an object's
    /// parent, a pool's object. A body walked again that writes those
    /// differently from last time makes the bodies reading them walk again
    fn mark_dirty(&mut self, id: FunctionId, record: &Record) {
        #[derive(PartialEq)]
        enum Shared<'a> {
            Write(&'a Write),
            Signature(Signature<'a>),
            Calls(&'a [FunctionId]),
        }
        fn shared(writes: &[Arc<Write>]) -> HashMap<Read, Vec<Shared<'_>>> {
            let mut shared: HashMap<Read, Vec<Shared>> = HashMap::default();
            for write in writes {
                match &**write {
                    Write::Function(id, info) => {
                        shared.entry(Read::Function(*id)).or_default().push(Shared::Signature(info.signature()));
                        shared.entry(Read::Calls(*id)).or_default().push(Shared::Calls(&info.calls));
                    }
                    Write::Parent(at, _) => shared.entry(Read::Parent(*at)).or_default().push(Shared::Write(write)),
                    Write::PoolObject(at, _) => shared.entry(Read::PoolObject(*at)).or_default().push(Shared::Write(write)),
                    _ => {}
                }
            }
            shared
        }
        let now = shared(&record.writes);
        let before = self.prior.and_then(|prior| prior.records.get(&id)).map(|old| shared(&old.writes));
        for (read, values) in now {
            if before.as_ref().and_then(|before| before.get(&read)) != Some(&values) {
                self.dirty.insert(read);
            }
        }
    }

    fn walk_function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>, id: FunctionId) {
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true, def.syntax().text_range().end());
        let outer_function_scope = std::mem::replace(&mut self.function_scope, self.scopes.len() - 1);
        let outer_short_circuit = std::mem::take(&mut self.short_circuit_depth);
        self.returns.push(None);
        self.calls.push(Vec::new());
        self.function_ranges.push((def.syntax().text_range(), self.function));
        self.function_ids.push(id);
        self.function_numbers.insert(self.function, id);
        self.fresh.push(None);
        if let Some(self_type) = self_type {
            let depth = self.scopes.len() - 1;
            // Implicit, so it's declared by the method's name: `b` in `function a:b()`
            let declaration = def.identifier().and_then(|identifier| identifier.syntax().children_with_tokens()
                .filter_map(|t| t.into_token())
                .filter(|t| t.kind() == SyntaxKind::Name)
                .last())
                .map(|name| name.text_range());
            let id = self.new_binding("self", BindingKind::Parameter, declaration, self_type, self.function, depth);
            self.bindings[id].assigned = true;
            self.bindings[id].visible = Some(def.syntax().text_range());
            self.scopes[depth].names.insert("self".to_string(), id);
        }
        let params = def.params();
        let parameters = params.as_ref().map(|p| p.parameter_tokens()).unwrap_or_default();
        let parameter_bindings: Vec<BindingId> = parameters.iter().enumerate()
            .map(|(i, param)| {
                let types = self.prior_parameter_types(id, i);
                let binding = self.declare(param, BindingKind::Parameter, types);
                self.parameter_index.insert(binding, i);
                self.parameter_entry.insert(binding, self.current[binding]);
                binding
            })
            .collect();
        let block = def.block();
        if let Some(block) = &block {
            self.statements(block);
        }
        let returned = self.returns.pop().flatten();
        let calls = self.calls.pop().unwrap_or_default();
        self.function_ranges.pop();
        self.function_ids.pop();
        let fresh_returns = self.fresh.pop().flatten().unwrap_or_default();
        let returns = match returned {
            Some(returns) if block.as_ref().is_some_and(always_returns) => returns,
            // Reaching `end` returns nothing
            Some(returns) => returns.union(&Values::none()),
            None => Values::none(),
        };
        self.pop_scope();
        self.function = outer_function;
        self.function_scope = outer_function_scope;
        self.short_circuit_depth = outer_short_circuit;

        let parameters = parameters.iter().zip(&parameter_bindings).map(|(token, binding)| Parameter {
            name: token.text().to_string(),
            // Replaced by `settle_parameter_types` once all calls are seen
            types: self.current[*binding],
            mandated: self.mandates.get(binding).copied().flatten(),
        }).collect();
        let varargs = params.is_some_and(|p| p.ellipsis())
            .then(|| Parameter { name: "...".to_string(), types: Type::UNKNOWN, mandated: None });
        self.apply(Write::Function(id, FunctionInfo { parameters, method: self_type.is_some(), varargs, returns, fresh_returns, calls }));
    }

    fn prior_parameter_types(&self, id: FunctionId, index: usize) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::ParameterTypes(id));
        prior.functions.get(&id)
            .and_then(|function| function.parameters.get(index))
            .map_or(Type::UNKNOWN, |parameter| parameter.types)
    }

    /// The function being called, as far as this walk knows it
    fn known_function(&self, function: FunctionRef) -> Option<&FunctionInfo> {
        match function {
            FunctionRef::One(id) => {
                self.note(Read::Function(id));
                self.functions.get(&id)
                    .or_else(|| self.layer.functions.get(&id))
                    .or_else(|| self.prior?.functions.get(&id))
            }
            FunctionRef::Builtin(id) => self.definitions.function(id),
            _ => None,
        }
    }

    /// Whether the current point runs every time the current function does
    fn runs_unconditionally(&self) -> bool {
        self.function != MAIN_CHUNK
            && self.short_circuit_depth == 0
            && self.scopes[self.function_scope + 1..].iter().all(|scope| !scope.conditional)
    }

    fn local_binding(&self, name: &str) -> Option<BindingId> {
        self.scopes.iter().rev().find_map(|scope| scope.names.get(name).copied())
    }

    /// Of two requirements, the one that satisfies both: the same type, or a
    /// named type that inherits from the other (`Frame` over `ScriptRegion`).
    /// None if they conflict
    fn narrower(&self, a: Type, b: Type) -> Option<Type> {
        if a == b {
            return Some(a)
        }
        let id = |t: Type| t.named.named_id();
        let (x, y) = (id(a)?, id(b)?);
        if a.set != b.set {
            return None
        }
        if self.definitions.is_a(x, y) {
            Some(a)
        } else if self.definitions.is_a(y, x) {
            Some(b)
        } else {
            None
        }
    }

    /// Parameters of the current function passed straight on as arguments that
    /// require one type have to be that type, if the call always runs and the
    /// parameter still holds what the caller passed
    fn require_arguments(&mut self, arguments: &[Expression], required: &[Option<Type>]) {
        if !self.runs_unconditionally() {
            return
        }
        for (argument, required) in arguments.iter().zip(required) {
            let (Some(required), Expression::Identifier(identifier)) = (required, argument) else { continue };
            let Some(id) = identifier.simple_name().and_then(|name| self.local_binding(name.text())) else { continue };
            let binding = &self.bindings[id];
            if binding.kind != BindingKind::Parameter || binding.function != self.function || binding.reassigned {
                continue
            }
            // Required to be a `ScriptRegion` means at least one: it could be a
            // Frame or a FontString, with more methods
            let required = match required.named {
                NamedRef::One(named) if self.definitions.has_subtypes(named) => Type { named: NamedRef::Extended(named), ..*required },
                _ => *required,
            };
            let merged = match self.mandates.get(&id) {
                None => Some(required),
                Some(earlier) => earlier.and_then(|earlier| self.narrower(earlier, required)),
            };
            self.mandates.insert(id, merged);
        }
    }

    fn returns_of(&mut self, id: FunctionId) -> Values {
        let returns = self.known_returns(id);
        let Some(&caller) = self.function_ids.last() else { return returns };
        if let Some(calls) = self.calls.last_mut().filter(|calls| !calls.contains(&id)) {
            calls.push(id);
        }
        if !self.reaches(id, caller) {
            return returns
        }
        // A recursive call: its results include its own, so as many values
        // as it returns would be one more each walk with `return x, f()`.
        // The first is kept apart, the others are joined in `rest`
        let mut values = returns.values.into_iter();
        let first = values.next();
        let rest = values.fold(returns.rest, |rest, value| rest | value);
        Values { values: first.into_iter().collect(), rest }
    }

    /// Whether calling `from` can come round to calling `to`, through the
    /// functions each calls: on this walk for those walked already, else as
    /// the previous walk found
    fn reaches(&self, from: FunctionId, to: FunctionId) -> bool {
        let mut seen = HashSet::default();
        let mut pending = vec![from];
        while let Some(id) = pending.pop() {
            if id == to {
                return true
            }
            if !seen.insert(id) {
                continue
            }
            self.note(Read::Calls(id));
            let function = self.functions.get(&id)
                .or_else(|| self.layer.functions.get(&id))
                .or_else(|| self.prior?.functions.get(&id));
            if let Some(function) = function {
                pending.extend(function.calls.iter().copied());
            }
        }
        false
    }

    fn known_returns(&self, id: FunctionId) -> Values {
        self.note(Read::Function(id));
        // Not walked yet on this walk: defined later in the file, or a recursive call
        if let Some(function) = self.functions.get(&id).or_else(|| self.layer.functions.get(&id)) {
            return function.returns.clone()
        }
        match self.prior {
            None => Values::nothing(),
            Some(prior) => prior.functions.get(&id).map_or_else(Values::unknown, |f| f.returns.clone()),
        }
    }

    /// The values an expression list produces, following Lua's multiple-value
    /// rules: a call or `...` at the end expands to all its values, anywhere
    /// else it's cut to one
    fn expression_values(&mut self, expressions: &[Expression]) -> Values {
        let mut values = Values::none();
        for (i, expression) in expressions.iter().enumerate() {
            let last = i + 1 == expressions.len();
            match expression {
                Expression::FunctionCall(call) if last => {
                    let results = self.function_call(call);
                    values.values.extend(results.values);
                    values.rest = results.rest;
                }
                Expression::VarArgs(_) if last => match self.vararg_values() {
                    Some(arguments) => {
                        values.values.extend(arguments.values);
                        values.rest = arguments.rest;
                    }
                    None => values.rest = Type::UNKNOWN,
                },
                _ => values.values.push(self.expression(expression)),
            }
        }
        values
    }

    /// Types for `count` targets, with nil for any missing values
    fn expression_list(&mut self, expressions: &[Expression], count: usize) -> Vec<Type> {
        let values = self.expression_values(expressions);
        (0..count).map(|i| values.get(i)).collect()
    }

    fn expression(&mut self, expression: &Expression) -> Type {
        match expression {
            Expression::Literal(l) => Type::of(if l.get_string().is_some() {
                TypeSet::STRING
            } else if l.get_number().is_some() {
                TypeSet::NUMBER
            } else if l.is_nil() {
                TypeSet::NIL
            } else if let Some(b) = l.get_bool() {
                if b.trim() == "true" { TypeSet::TRUE } else { TypeSet::FALSE }
            } else {
                TypeSet::UNKNOWN
            }),
            Expression::Function(def) => Type::function(self.function_body(def, None)),
            Expression::TableConstructor(t) => self.table_constructor(t.syntax()),
            // `f()` in the middle of an expression only gives its first value
            Expression::FunctionCall(call) => self.function_call(call).first(),
            Expression::Identifier(identifier) => match (identifier.simple_name(), self.global_table_access(identifier.syntax())) {
                (Some(name), _) => self.read(&name),
                // `_G.name`: the global
                (None, Some((token, name))) => {
                    self.identifier_parts(identifier.syntax());
                    let id = self.resolve_global(&name);
                    self.read_binding(&token, id)
                }
                (None, None) => match field_access(identifier.syntax()) {
                    // `a.b`: field `b` of whatever `a` is
                    Some((base, field)) => {
                        let base = self.expression(&base);
                        let types = self.field_read(base, field.text());
                        self.field_reference(identifier.syntax(), &field, base, types, false);
                        types
                    }
                    None => match index_access(identifier.syntax()) {
                        // `t[k]`
                        Some((base, key)) => {
                            let base = self.expression(&base);
                            match self.index_key(&key) {
                                Key::Name(name, token) => {
                                    let types = self.field_read(base, &name);
                                    if let Some(token) = token {
                                        self.field_reference(identifier.syntax(), &token, base, types, false);
                                    }
                                    types
                                }
                                Key::Other(key) => self.index_read(base, key),
                            }
                        }
                        None => {
                            self.identifier_parts(identifier.syntax());
                            Type::UNKNOWN
                        }
                    },
                },
            },
            Expression::GroupedExpression(g) => match g.get_expression() {
                Some(e) => self.expression(&e),
                None => Type::UNKNOWN,
            },
            Expression::UnaryExpression(u) => {
                if let Some(operand) = u.operand() {
                    self.expression(&operand);
                }
                Type::of(match u.kind() {
                    Operator::Not => TypeSet::BOOLEAN,
                    _ => TypeSet::NUMBER,
                })
            }
            Expression::BinaryExpression(b) => self.binary_expression(b),
            Expression::VarArgs(_) => self.vararg_values().map_or(Type::UNKNOWN, |arguments| arguments.first()),
        }
    }

    fn function_call(&mut self, call: &FunctionCall) -> Values {
        let is_method = call.method_name().is_some();
        // Where the callee's read was recorded, to note what this call returns
        let mut callee_read = None;
        let callee = call.callee().map(|callee| {
            let types = self.expression(&callee);
            match &callee {
                // The read `expression` just recorded
                Expression::Identifier(i) if !is_method && (i.simple_name().is_some() || self.global_table_access(i.syntax()).is_some()) => {
                    if let Some(reference) = self.references.last_mut() {
                        reference.callee = true;
                        callee_read = Some(CalleeRead::Variable(self.references.len() - 1));
                    }
                }
                Expression::Identifier(i) if !is_method && field_access(i.syntax()).is_some() => {
                    self.last_field_is_callee();
                    callee_read = self.field_references.len().checked_sub(1).map(CalleeRead::Field);
                }
                _ => {}
            }
            types
        });
        let receiver = if is_method { callee } else { None };
        // `a:m()` calls field `m` of `a`
        let callee = match (callee, call.method_name()) {
            (Some(base), Some(method)) => {
                let types = self.field_read(base, method.text());
                self.field_reference(call.syntax(), &method, base, types, false);
                self.last_field_is_callee();
                callee_read = Some(CalleeRead::Field(self.field_references.len() - 1));
                self.check_method(base, &method);
                Some(types)
            }
            (callee, _) => callee,
        };
        let arguments = call.arguments();
        let argument_types = self.expression_values(&arguments);
        if let Some(callee) = callee {
            self.table_library_call(call, callee, arguments.len(), &argument_types);
        }
        // Calling a table whose metatable has `__call` calls that, with the table first
        let call_metamethod = callee
            .filter(|c| !c.set.contains(TypeSet::FUNCTION))
            .and_then(|c| Some((c, self.metamethod(self.prior?, c.named.identity()?, "__call", 0)?)))
            .filter(|(_, f)| f.set.contains(TypeSet::FUNCTION));
        let (callee, receiver, is_method) = match call_metamethod {
            Some((table, function)) => (Some(function), Some(table), true),
            None => (callee, receiver, is_method),
        };
        let Some(callee) = callee.filter(|c| c.set.contains(TypeSet::FUNCTION)) else {
            // Nothing known of it yet, like a function read as an upvalue on the
            // first walk, so nothing known of what it returns. Unknown would
            // stay in what it flows into, even a function's own returns
            return match callee {
                Some(callee) if callee.set == TypeSet::NEVER => Values::nothing(),
                _ => Values::unknown(),
            }
        };
        let Some(function) = self.known_function(callee.function) else {
            // A function the files make but this first walk hasn't reached yet
            return match (callee.function, self.prior) {
                (FunctionRef::One(_), None) => Values::nothing(),
                _ => Values::unknown(),
            }
        };
        let required: Vec<Option<Type>> = (0..arguments.len()).map(|i| function.mandated(i)).collect();
        // Where the call passes a template, if it takes one
        let template = function.parameters.iter()
            .position(|p| p.name.to_ascii_lowercase().contains("template"));
        let documented = function.returns.clone();
        let fresh_returns = function.fresh_returns.clone();
        let callee_method = function.method;
        // What the function's own parameters get: `self` is separate for a
        // method, whether it's passed with `:` or explicitly
        let parameter_types = match (function.method, is_method) {
            (true, false) => Values { values: argument_types.values.iter().skip(1).copied().collect(), rest: argument_types.rest },
            (false, true) => {
                let mut values = vec![receiver.unwrap_or(Type::UNKNOWN)];
                values.extend(argument_types.values.iter().copied());
                Values { values, rest: argument_types.rest }
            }
            _ => argument_types.clone(),
        };
        let mut returns = match callee.function {
            FunctionRef::One(id) => {
                match self.pending_call(id, &arguments, &parameter_types, callee_method, is_method) {
                    Some(pending) => self.pending_calls.push(pending),
                    None => self.apply(Write::CallArguments(id, parameter_types.clone())),
                }
                let mut returns = self.returns_of(id);
                for (slot, fresh) in fresh_returns.iter().enumerate() {
                    if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                        returns.values[slot] = self.fresh_object(call, slot, value, fresh, &parameter_types);
                    }
                }
                returns
            }
            _ => self.select(callee, &arguments, &argument_types)
                .or_else(|| self.library_returns(callee, &arguments, &argument_types))
                .or_else(|| self.create_frame(call, callee, &arguments, &argument_types))
                .or_else(|| self.create_pool(call, callee, &arguments, &argument_types))
                .or_else(|| self.mix(call, callee, &arguments, &argument_types))
                .unwrap_or_else(|| documented.clone()),
        };
        if let (Some(receiver), Some(method)) = (receiver, call.method_name()) {
            returns = self.object_method(call, receiver, method.text(), template.and_then(|i| arguments.get(i)), &argument_types, returns);
        }
        self.require_arguments(&arguments, &required);
        if returns != documented {
            let call_returns = Some(returns.clone());
            match callee_read {
                Some(CalleeRead::Variable(i)) => if let Some(r) = self.references.get_mut(i) { r.call_returns = call_returns },
                Some(CalleeRead::Field(i)) => if let Some(f) = self.field_references.get_mut(i) { f.call_returns = call_returns },
                None => {}
            }
        }
        returns
    }

    /// Methods that make or change WoW objects' parents. `returns` is what the
    /// method's documentation says it returns
    fn object_method(&mut self, call: &FunctionCall, receiver: Type, name: &str, template: Option<&Expression>, arguments: &Values, mut returns: Values) -> Values {
        let parent = receiver.named.identity();
        if let Some(object) = parent.and_then(|at| self.pool_object(at)) {
            match name {
                "Acquire" => return Values { values: vec![object, Type::of(TypeSet::BOOLEAN)], rest: Type::NIL },
                "GetNextActive" => return Values { values: vec![object | Type::NIL], rest: Type::NIL },
                "EnumerateActive" => {
                    self.enumerated.insert(self.location(call.syntax().text_range().start()), object);
                }
                _ => {}
            }
        }
        match name {
            "SetParent" => {
                if let Some(at) = parent {
                    self.apply(Write::Parent(at, arguments.first()));
                }
                // Whichever of several objects it is
                for object in self.several_objects(receiver) {
                    self.apply(Write::Parent(object, arguments.first()));
                }
            }
            "GetParent" => {
                // What it was made with or given, if it was made in the files
                if let Some(parent) = parent.and_then(|at| self.known_parent(at)) {
                    returns = Values { values: vec![parent], rest: Type::NIL };
                }
                // Any of several objects made in the files, each with a known parent
                if let NamedRef::Several(_) = receiver.named {
                    let parents: Option<Vec<Type>> = receiver.named.alternatives().into_iter()
                        .map(|object| self.known_parent(object.identity()?))
                        .collect();
                    if let Some(parent) = parents.and_then(|parents| parents.into_iter().reduce(Type::union)) {
                        returns = Values { values: vec![parent], rest: Type::NIL };
                    }
                }
                // `self` in a mixin's method: any object it's mixed into,
                // some perhaps made where the analysis can't see (from a
                // template named by a variable), so not a parent to report
                // missing methods by
                if parent.is_some_and(|at| self.is_mixin(at)) {
                    for value in &mut returns.values {
                        value.named = value.named.loosened();
                    }
                }
            }
            // `parent:CreateTexture()` and the like: a new object of the type the
            // method returns, made with the receiver as its parent
            name if name.starts_with("Create") => {
                let Some(first) = returns.values.first_mut() else { return returns };
                let NamedRef::One(child) = first.named else { return returns };
                if !self.definitions.named_type(child).is_some_and(|t| t.has_methods) {
                    return returns
                }
                let at = self.location(call.syntax().text_range().start());
                let (named, extended) = self.templated(child, template);
                first.named = NamedRef::Instance { named, at, extended };
                self.apply(Write::Parent(at, receiver));
                self.add_lua_mixins(at, named, receiver);
            }
            _ => {}
        }
        returns
    }

    /// Reports `receiver:method()` when the receiver is one WoW type whose
    /// methods are known, and none of them (nor any function the addon puts in
    /// a table under that name, as a mixin might) is called `method`
    fn check_method(&mut self, receiver: Type, method: &SyntaxToken) {
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
            let Some(named) = self.definitions.named_type(id).filter(|named| named.has_methods) else { return };
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
            let own = object.identity().and_then(|at| self.own_field(prior, at, name, 0));
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

    /// Whether every Lua mixin type `id` has (see `Definitions::lua_mixins`)
    /// is a table made in the files or those they depend on, whose fields
    /// the analysis sees
    fn lua_mixins_known(&self, prior: &Analysis, id: NamedId) -> bool {
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
    fn add_lua_mixins(&mut self, at: TableId, named: NamedId, parent: Type) {
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
    fn check_table_method(&mut self, prior: &Analysis, table: TableId, method: &SyntaxToken) {
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
    fn may_gain_fields(&self, prior: &Analysis, table: TableId, depth: usize) -> bool {
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

    /// `CreateFrame("Button", ...)` gives a `Button`: the frame type named by a
    /// literal first argument, if it's a kind of `Frame`, or the type of the
    /// template it's made from (the fourth argument; see `templated`)
    fn create_frame(&mut self, call: &FunctionCall, callee: Type, arguments: &[Expression], argument_types: &Values) -> Option<Values> {
        if self.definitions.global("CreateFrame") != Some(callee) {
            return None
        }
        let kind = self.frame_type(arguments.first())?;
        let (named, extended) = self.templated(kind, arguments.get(3));
        let at = self.location(call.syntax().text_range().start());
        // The third argument, or nil without one
        self.apply(Write::Parent(at, argument_types.get(2)));
        self.add_lua_mixins(at, named, argument_types.get(2));
        let named = NamedRef::Instance { named, at, extended };
        Some(Values { values: vec![Type { named, ..Type::of(TypeSet::TABLE) }], rest: Type::NIL })
    }

    /// The type of an object of type `kind` made from the template `template`
    /// names, and whether it may have members beyond that type's. With no
    /// template, it's `kind`. With one known template for that kind of object
    /// (`CreateFrame("Button", nil, nil, "UIPanelButtonTemplate")`), it's the
    /// template's type. Otherwise, as with several templates, one for another
    /// kind, or one wow_ls doesn't know, it's `kind`, and the template can add
    /// members
    fn templated(&self, kind: NamedId, template: Option<&Expression>) -> (NamedId, bool) {
        let text = match template {
            None => return (kind, false),
            Some(Expression::Literal(literal)) if literal.is_nil() => return (kind, false),
            Some(Expression::Literal(literal)) => literal.get_string(),
            Some(_) => None,
        };
        let name = text.as_deref().map(|t| t.trim().trim_matches(['"', '\'']).trim());
        match name.and_then(|name| self.definitions.template(name)) {
            Some((named, element)) if element == kind => (named, false),
            _ => (kind, true),
        }
    }

    /// The kind of `Frame` a literal frame type names, else `Frame`
    fn frame_type(&self, argument: Option<&Expression>) -> Option<NamedId> {
        let frame = self.definitions.named_type_id("Frame")?;
        Some(match argument {
            Some(Expression::Literal(literal)) => literal.get_string()
                .and_then(|text| self.definitions.named_type_id_ignoring_case(text.trim().trim_matches(['"', '\''])))
                .filter(|&id| self.definitions.is_a(id, frame))
                .unwrap_or(frame),
            _ => frame,
        })
    }

    /// What a pool made in the files holds, from this walk if it's been made
    /// yet, else the previous one
    fn pool_object(&self, pool: TableId) -> Option<Type> {
        self.note(Read::PoolObject(pool));
        self.pool_objects.get(&pool)
            .or_else(|| self.layer.pool_objects.get(&pool))
            .or_else(|| self.prior?.pool_objects.get(&pool))
            .copied()
    }

    /// `CreateFramePool`, `CreateTexturePool`, `CreateFontStringPool` and
    /// `CreateObjectPool` make a pool, whose objects are all one object here:
    /// of the type the pool makes (or its create function returns), made with
    /// the pool's parent. Its init and reset functions are called with it, so
    /// the fields and mixins they give it are what `Acquire` returns
    fn create_pool(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let is = |name: &str| self.builtin(name) == Some(callee);
        let at = self.location(call.syntax().text_range().start());
        let object_at = Location { slot: POOL_OBJECT, ..at };
        // Made from a template, as with `CreateFrame`
        let region = |kind: Option<NamedId>, template: usize| -> Option<Type> {
            let (named, extended) = self.templated(kind?, expressions.get(template));
            let named = NamedRef::Instance { named, at: object_at, extended };
            Some(Type { named, ..Type::of(TypeSet::TABLE) })
        };
        // The object, its parent, then each callback's argument position and
        // whether it's given the pool before the object
        let (object, parent, callbacks): (Type, Option<Type>, &[(usize, bool)]) = if is("CreateFramePool") {
            (region(self.frame_type(expressions.first()), 2)?, Some(arguments.get(1)), &[(3, true), (5, false)])
        } else if is("CreateTexturePool") {
            (region(self.definitions.named_type_id("Texture"), 3)?, Some(arguments.get(0)), &[(4, true)])
        } else if is("CreateFontStringPool") {
            (region(self.definitions.named_type_id("FontString"), 3)?, Some(arguments.get(0)), &[(4, true)])
        } else if is("CreateObjectPool") {
            // Whatever the create function returns, which it's given the pool for
            let pool = self.pool_type(at);
            let create = arguments.get(0);
            let object = match create.function {
                FunctionRef::One(id) => self.returns_of(id).first(),
                FunctionRef::Builtin(id) => self.definitions.function(id).map_or(Type::UNKNOWN, |f| f.returns.first()),
                _ => Type::UNKNOWN,
            };
            self.pass_to_callback(expressions.first(), create, Values { values: vec![pool], rest: Type::NIL });
            (object, None, &[(1, true)])
        } else {
            return None
        };
        if let Some(parent) = parent {
            self.apply(Write::Parent(object_at, parent));
        }
        let pool = self.pool_type(at);
        self.apply(Write::PoolObject(at, object));
        for &(index, with_pool) in callbacks {
            let values = if with_pool { vec![pool, object] } else { vec![object] };
            self.pass_to_callback(expressions.get(index), arguments.get(index), Values { values, rest: Type::NIL });
        }
        Some(Values { values: vec![pool], rest: Type::NIL })
    }

    /// A pool made at `at`
    fn pool_type(&self, at: TableId) -> Type {
        match self.definitions.named_type_id("ObjectPool") {
            // Only the main methods are listed, so others aren't reported
            Some(named) => Type { named: NamedRef::Instance { named, at, extended: true }, ..Type::of(TypeSet::TABLE) },
            None => Type::table(at),
        }
    }

    /// A function given to a pool, which calls it with `arguments`. That call
    /// is seen like any other, and passing it there doesn't let it escape
    fn pass_to_callback(&mut self, expression: Option<&Expression>, function: Type, arguments: Values) {
        let FunctionRef::One(id) = function.function else { return };
        self.apply(Write::CallArguments(id, arguments));
        let Some(Expression::Identifier(identifier)) = expression else {
            // A function written right there, which nothing else can call
            if matches!(expression, Some(Expression::Function(_))) {
                self.apply(Write::PoolCallback(id));
            }
            return
        };
        // The read of the variable or field holding it counts as a call
        let range = identifier.syntax().text_range();
        if identifier.simple_name().is_some() {
            if let Some(reference) = self.references.iter_mut().rev().find(|r| r.file == self.file && range.contains_range(r.range)) {
                reference.callee = true;
            }
        } else if field_access(identifier.syntax()).is_some() {
            if let Some(field) = self.field_references.iter_mut().rev().find(|f| f.file == self.file && !f.write && range.contains_range(f.range)) {
                field.callee = true;
            }
        }
    }

    /// The parents an object made in the files is known to have
    fn known_parent(&self, at: TableId) -> Option<Type> {
        self.note(Read::Parent(at));
        // From the previous walk, which saw every `SetParent`, and this one so
        // far. Parents only gain types, so the two together are never wrong
        let prior = self.prior.and_then(|prior| prior.parents.get(&at)).copied();
        let current = self.parents.get(&at).copied();
        let layered = self.layer.parents.get(&at).copied();
        match (prior, current, layered) {
            (None, None, None) => None,
            (a, b, c) => Some(a.unwrap_or_default() | b.unwrap_or_default() | c.unwrap_or_default()),
        }
    }

    /// Where each of several objects `ty` may be was made, when it's that
    fn several_objects(&self, ty: Type) -> Vec<TableId> {
        match ty.named {
            NamedRef::Several(_) => ty.named.alternatives().into_iter().filter_map(NamedRef::identity).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether the table made at `at` is a mixin mixed into WoW objects, as
    /// the previous walk found
    fn is_mixin(&self, at: TableId) -> bool {
        self.note(Read::MixedInto(at));
        self.prior.is_some_and(|prior| prior.mixed_into.contains_key(&at)) || self.layer.mixed_into.contains_key(&at)
    }

    /// Whether a table or object was made by the function being walked
    fn made_here(&self, at: TableId) -> bool {
        at.slot & !PSEUDO == 0 && at.file == self.file && self.function_ranges.last().is_some_and(|(range, _)| range.contains(at.offset))
    }

    /// The innermost function being walked that made a table or object (or a
    /// pool whose object it is), as its number in the walk
    fn maker(&self, at: TableId) -> Option<usize> {
        if at.file != self.file || at.slot & !(POOL_OBJECT | PSEUDO) != 0 {
            return None
        }
        self.function_ranges.iter().rev().find(|(range, _)| range.contains(at.offset)).map(|&(_, function)| function)
    }

    /// The function (by its number in the walk) and index of the parameter
    /// that `expression` reads, if it reads one that still holds its argument.
    /// It can be an enclosing function's
    fn parameter_read(&self, expression: &Expression) -> Option<(usize, usize)> {
        let Expression::Identifier(identifier) = expression else { return None };
        let id = self.local_binding(identifier.simple_name()?.text())?;
        let binding = &self.bindings[id];
        if binding.reassigned {
            return None
        }
        Some((binding.function, *self.parameter_index.get(&id)?))
    }

    /// The call to `callee` as one to settle at the end of the walk, if it
    /// passes a parameter of the code calling it (or its `...`) straight on.
    /// `known` is its arguments by the callee's parameter positions
    fn pending_call(&self, callee: FunctionId, arguments: &[Expression], known: &Values, callee_method: bool, is_method: bool) -> Option<PendingCall> {
        // How far each argument is from the parameter it fills: `self` is
        // separate for a method, whether passed with `:` or explicitly
        let offset: isize = match (callee_method, is_method) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        let mut parameters = Vec::new();
        let mut varargs = None;
        for (i, argument) in arguments.iter().enumerate() {
            let Ok(position) = usize::try_from(i as isize + offset) else { continue };
            // The top of a file has no caller: its `...` is the addon's
            if i + 1 == arguments.len() && matches!(argument, Expression::VarArgs(_)) && self.function != MAIN_CHUNK {
                varargs = self.function_ids.last().map(|&function| (position, function));
                continue
            }
            let Expression::Identifier(identifier) = argument else { continue };
            let Some(id) = identifier.simple_name().and_then(|name| self.local_binding(name.text())) else { continue };
            let binding = &self.bindings[id];
            // Only while it still holds what was passed
            if binding.reassigned {
                continue
            }
            let (Some(&index), Some(&function)) = (self.parameter_index.get(&id), self.function_numbers.get(&binding.function)) else { continue };
            // Narrowed here, as in `if type(x) == "string" then f(x) end`. An
            // enclosing function's parameter isn't narrowed
            let narrowed = (binding.function == self.function)
                .then(|| self.current[id])
                .filter(|&now| self.parameter_entry.get(&id) != Some(&now))
                .map(|now| now.set);
            parameters.push((position, function, index, narrowed));
        }
        (!parameters.is_empty() || varargs.is_some()).then(|| PendingCall { callee, known: known.clone(), parameters, varargs })
    }

    /// A function's new object, for one call: a new identity at the call, with
    /// the object the function made mixed in (for the fields it set there)
    /// along with that call's arguments for the parameters it mixed in
    fn fresh_object(&mut self, call: &FunctionCall, slot: usize, value: Type, fresh: &FreshObject, arguments: &Values) -> Type {
        let at = Location { slot: slot as u32 + 1, ..self.location(call.syntax().text_range().start()) };
        let named = match value.named {
            NamedRef::Table(_) => NamedRef::Table(at),
            NamedRef::Instance { named, extended, .. } => NamedRef::Instance { named, at, extended },
            other => return Type { named: other, ..value },
        };
        let mut mixins = vec![value];
        mixins.extend(fresh.mixed_parameters.iter().map(|&i| arguments.get(i)));
        for mixin in &mixins {
            self.record_mixed_into(*mixin, named);
        }
        self.apply(Write::MixinsReplaced(at, mixins));
        if let Some(parent) = self.known_parent(fresh.made) {
            self.apply(Write::Parent(at, parent));
        }
        // A pool: its objects are new for the call too, with the call's
        // arguments for the parameters its init or reset function mixed in
        if let (Some(mixed), Some(inner)) = (&fresh.pool_mixed_parameters, self.pool_object(fresh.made)) {
            let object_at = Location { slot: at.slot | POOL_OBJECT, ..at };
            let object = match inner.named {
                NamedRef::Instance { named, extended, .. } => Type { named: NamedRef::Instance { named, at: object_at, extended }, ..inner },
                NamedRef::Table(_) => Type { named: NamedRef::Table(object_at), ..inner },
                _ => inner,
            };
            if object.named.identity() == Some(object_at) {
                let mut mixins = vec![inner];
                mixins.extend(mixed.iter().map(|&i| arguments.get(i)));
                for mixin in &mixins {
                    self.record_mixed_into(*mixin, object.named);
                }
                self.apply(Write::MixinsReplaced(object_at, mixins));
                if let Some(parent) = inner.named.identity().and_then(|inner| self.known_parent(inner)) {
                    self.apply(Write::Parent(object_at, parent));
                }
            }
            self.apply(Write::PoolObject(at, object));
        }
        Type { named, ..value }
    }

    /// `CreateFromMixins(a, b)` makes a table with the fields of `a` and `b`,
    /// and `Mixin(object, a, b)` adds them to `object` and returns it
    fn mix(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let (object, mixins, mixin_expressions) = if self.definitions.global("CreateFromMixins") == Some(callee) {
            (Type::table(self.location(call.syntax().text_range().start())), &arguments.values[..], expressions)
        } else if self.definitions.global("Mixin") == Some(callee) {
            let (object, mixins) = arguments.values.split_first()?;
            (*object, mixins, expressions.get(1..).unwrap_or_default())
        } else {
            return None
        };
        // Mixing a parameter into something the function makes, so each call
        // mixes in what it passes
        // (or an enclosing function makes, like a pool's object in its init function)
        if let Some((at, maker)) = object.named.identity().and_then(|at| Some((at, self.maker(at)?))) {
            let parameters: Vec<usize> = mixin_expressions.iter()
                .filter_map(|e| self.parameter_read(e))
                .filter(|&(function, _)| function == maker)
                .map(|(_, index)| index)
                .collect();
            for parameter in parameters {
                self.apply(Write::ParameterMixin(at, parameter));
            }
        }
        for mixin in mixins {
            self.record_mixed_into(*mixin, object.named);
        }
        // Only a table or object made in the files can keep track of its
        // mixins: it, or each of several it may be
        let targets: Vec<TableId> = object.named.identity().into_iter().chain(self.several_objects(object)).collect();
        for at in targets {
            for mixin in mixins {
                self.apply(Write::MixinAdded(at, *mixin));
            }
        }
        Some(Values { values: vec![object], rest: Type::NIL })
    }

    /// `mixin` was mixed into a table or object of type `object`
    fn record_mixed_into(&mut self, mixin: Type, object: NamedRef) {
        let Some(mixin) = mixin.named.identity() else { return };
        if matches!(object, NamedRef::None | NamedRef::Unknown) {
            return
        }
        self.apply(Write::MixedInto(mixin, object));
    }

    /// What `mixin` was mixed into on this walk, following tables made from it
    /// to what they were mixed into
    fn mixin_target(&self, mixin: TableId, depth: usize) -> MixinTarget {
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return MixinTarget::Nothing
        }
        let Some(objects) = self.mixed_into.get(&mixin) else { return MixinTarget::Nothing };
        // Each of several objects it may have been mixed into
        let objects = objects.iter().flat_map(|&object| match object {
            NamedRef::Several(_) => object.alternatives(),
            object => vec![object],
        });
        objects.fold(MixinTarget::Nothing, |target, object| {
            let next = match object.object_type() {
                Some((named, extended)) => MixinTarget::Object(named, extended),
                // A table, which is plain unless it's mixed into something too
                None => match object.identity().map(|at| self.mixin_target(at, depth + 1)) {
                    Some(MixinTarget::Nothing) | None => MixinTarget::Other,
                    Some(target) => target,
                },
            };
            match (target, next) {
                (MixinTarget::Nothing, x) | (x, MixinTarget::Nothing) => x,
                (MixinTarget::Object(a, e), MixinTarget::Object(b, f)) => match self.definitions.common_ancestor(a, b) {
                    Some(common) => MixinTarget::Object(common, e || f),
                    None => MixinTarget::Other,
                },
                _ => MixinTarget::Other,
            }
        })
    }

    /// `select(n, ...)` with a literal `n`, which gives the arguments from the
    /// `n`th on, or `select("#", ...)`, a count. This is how many addons get
    /// their namespace: `local ns = select(2, ...)`
    /// The preset function at a path like `table.insert`, as it's typed
    fn builtin(&self, path: &str) -> Option<Type> {
        match self.builtins.iter().find(|(known, _)| *known == path) {
            Some(&(_, ty)) => ty,
            None => self.definitions.at_path(path),
        }
    }


    /// Standard library functions whose returns depend on their arguments
    fn library_returns(&mut self, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
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
            let function = arguments.get(0).function;
            let known = self.known_function(function).map(|f| f.returns.clone())?;
            let returns = match function {
                FunctionRef::One(id) => self.returns_of(id),
                _ => known,
            };
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
        if let Some(at) = table {
            self.note(Read::Entries(at));
            self.note(Read::Fields(at));
        }
        // What a table's array holds, any of which may be missing
        let array = |prior: &Analysis| self.entries_of(prior, table?)
            .filter(|e| e.keys.set.contains(TypeSet::NUMBER))
            .map(|e| e.values | Type::NIL);
        if is("unpack") {
            return Some(Values { values: Vec::new(), rest: array(self.prior?)? })
        }
        if is("table.remove") || is("tremove") {
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
            let contents = self.contents_of(self.prior?, table?)?;
            return Some(Values { values: vec![contents.keys | Type::NIL, contents.values | Type::NIL], rest: Type::NIL })
        }
        if is("setmetatable") {
            let (table, metatable) = (arguments.get(0), arguments.get(1));
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

    fn select(&self, callee: Type, arguments: &[Expression], argument_types: &Values) -> Option<Values> {
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

    /// Visits the variable reads in `a.b`, `a[k]`, `a:m(x).y` and similar: the
    /// base, index expressions and call arguments, but not field names.
    /// Expressions nest to the left (Identifier[Identifier[a], Dot, Name b]); function
    /// definition names are flat (Identifier[Name a, Dot, Name b, Colon, Name c])
    fn identifier_parts(&mut self, node: &SyntaxNode) {
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
    fn table_constructor(&mut self, node: &SyntaxNode) -> Type {
        let table = Type::table(self.location(node.text_range().start()));
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
                        let value = self.expression(&e);
                        match (keyed, &name, key.take()) {
                            (false, ..) => self.entry_write(table, Type::of(TypeSet::NUMBER), value),
                            (true, Some(name), _) => {
                                self.field_write(table, name.text(), value);
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
    fn index_key(&mut self, key: &Expression) -> Key {
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
    fn entry_write(&mut self, base: Type, key: Type, value: Type) {
        let Some(at) = base.named.identity() else { return };
        let value = value.without(TypeSet::NIL);
        if value.set == TypeSet::NEVER {
            return
        }
        self.apply(Write::Entry(at, key.without(TypeSet::NIL), value));
    }

    /// The type of `t[key]`, from what the previous walk saw stored. It may
    /// not be there, so it can be nil
    fn index_read(&self, base: Type, key: Type) -> Type {
        let Some(at) = base.named.identity() else { return Type::UNKNOWN };
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::Entries(at));
        self.note(Read::Fields(at));
        let entries = self.entries_of(prior, at);
        let mut found = entries.map(|e| e.values);
        // A string key could be any of the named fields
        if key.set.contains(TypeSet::STRING) {
            for value in self.field_values_of(prior, at) {
                *found.get_or_insert_default() |= value;
            }
        }
        match found {
            Some(values) if values.set != TypeSet::NEVER => values | Type::NIL,
            _ => Type::UNKNOWN,
        }
    }

    /// What a table holds beyond its named fields: here, and from a dependency
    fn entries_of(&self, prior: &Analysis, at: TableId) -> Option<Entries> {
        match (prior.entries.get(&at), self.layer.entries.get(&at)) {
            (Some(a), Some(b)) => Some(Entries { keys: a.keys | b.keys, values: a.values | b.values }),
            (a, b) => a.or(b).copied(),
        }
    }

    /// The types of a table's named fields: here, and from a dependency
    fn field_values_of(&self, prior: &Analysis, at: TableId) -> Vec<Type> {
        prior.fields.get(&at).into_iter().chain(self.layer.fields.get(&at)).flat_map(|fields| fields.values().copied()).collect()
    }

    /// Every key and value a table holds, as `pairs` gives them, here and
    /// from a dependency
    fn contents_of(&self, prior: &Analysis, at: TableId) -> Option<Entries> {
        let mut contents = self.entries_of(prior, at).unwrap_or_default();
        for value in self.field_values_of(prior, at) {
            contents.keys |= Type::of(TypeSet::STRING);
            contents.values |= value.without(TypeSet::NIL);
        }
        (contents.values.set != TypeSet::NEVER).then_some(contents)
    }

    /// `table.insert` (or `tinsert`) adds its last argument to the table, and
    /// `pairs` and `ipairs` note what their loop gives
    fn table_library_call(&mut self, call: &FunctionCall, callee: Type, count: usize, arguments: &Values) {
        let is = |path: &str| self.builtin(path) == Some(callee);
        // Every capture (or the whole match) is a string
        if is("string.gmatch") || is("gmatch") {
            let strings = Values { values: Vec::new(), rest: Type::of(TypeSet::STRING) };
            self.iterated.insert(self.location(call.syntax().text_range().start()), strings);
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
        let (Some(prior), Some(at)) = (self.prior, arguments.get(0).named.identity()) else { return };
        self.note(Read::Entries(at));
        self.note(Read::Fields(at));
        let contents = if ipairs {
            // Only the entries under number keys are visited
            self.entries_of(prior, at)
                .filter(|e| e.keys.set.contains(TypeSet::NUMBER))
                .map(|e| (Type::of(TypeSet::NUMBER), e.values))
        } else {
            self.contents_of(prior, at).map(|e| (e.keys, e.values))
        };
        if let Some((key, value)) = contents {
            let values = Values { values: vec![key, value], rest: Type::NIL };
            self.iterated.insert(self.location(call.syntax().text_range().start()), values);
        }
    }

    fn binary_expression(&mut self, b: &BinaryExpression) -> Type {
        match self.binary_tree(b) {
            Some(tree) => self.operation(&tree),
            None => {
                // Syntax error somewhere inside: still read what's there
                for part in b.flatten() {
                    if let BinaryPart::Operand(e) = part {
                        self.expression(&e);
                    }
                }
                Type::UNKNOWN
            }
        }
    }

    fn binary_tree(&self, b: &BinaryExpression) -> Option<Operation> {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        for part in b.flatten() {
            match part {
                BinaryPart::Operand(e) => operands.push(e),
                BinaryPart::Operator(op) => operators.push(op),
            }
        }
        if operands.len() != operators.len() + 1 {
            return None
        }
        let mut position = 0;
        operation_tree(&mut operands.into_iter(), &operators, &mut position, 0)
    }

    /// The right of an `and` runs only if the left is true, and the right of
    /// an `or` only if it's false, so each sees what that proves. Either might
    /// not run at all
    fn operation(&mut self, operation: &Operation) -> Type {
        match operation {
            Operation::Operand(e) => self.expression(e),
            Operation::Binary(op @ (Operator::And | Operator::Or), left, right) => {
                let left_type = self.operation(left);
                let proved = self.operation_narrowing(left);
                let proved = if *op == Operator::And { proved.when_true } else { proved.when_false };
                self.short_circuit_depth += 1;
                let right_type = self.with_narrowing(&proved, |w| w.operation(right));
                self.short_circuit_depth -= 1;
                combine(*op, left_type, right_type)
            }
            Operation::Binary(op, left, right) => {
                let left = self.operation(left);
                let right = self.operation(right);
                combine(*op, left, right)
            }
        }
    }

    /// Runs `f` with locals narrowed to the types given. Afterwards each is
    /// what it was before, with anything `f` assigned it, as if never narrowed
    fn with_narrowing<R>(&mut self, narrowed: &[(BindingId, Type)], f: impl FnOnce(&mut Self) -> R) -> R {
        let saved: Vec<(BindingId, Type)> = narrowed.iter().map(|&(id, _)| (id, self.current[id])).collect();
        for &(id, ty) in narrowed {
            self.set_current(id, ty);
        }
        let result = f(self);
        for (id, before) in saved {
            self.set_current(id, self.current[id] | before);
        }
        result
    }

    /// A local of the current function named by `expression`, and its type
    /// here. Upvalues aren't narrowed, as a closure can run at any time
    fn narrowable(&self, expression: &Expression) -> Option<(BindingId, Type)> {
        let Expression::Identifier(identifier) = expression else { return None };
        let id = self.local_binding(identifier.simple_name()?.text())?;
        (self.bindings[id].function == self.function).then(|| (id, self.current[id]))
    }

    /// What `condition` being true, or false, proves about the locals it tests
    fn narrowing(&self, condition: &Expression) -> Narrowing {
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
                Some((id, ty)) => Narrowing { when_true: vec![(id, ty.truthy())], when_false: vec![(id, ty.falsy())] },
                None => Narrowing::default(),
            },
        }
    }

    fn operation_narrowing(&self, operation: &Operation) -> Narrowing {
        match operation {
            Operation::Operand(e) => self.narrowing(e),
            // Both true; false says nothing about which wasn't
            Operation::Binary(Operator::And, left, right) => Narrowing {
                when_true: both(self.operation_narrowing(left).when_true, self.operation_narrowing(right).when_true),
                when_false: Vec::new(),
            },
            Operation::Binary(Operator::Or, left, right) => Narrowing {
                when_true: Vec::new(),
                when_false: both(self.operation_narrowing(left).when_false, self.operation_narrowing(right).when_false),
            },
            Operation::Binary(op @ (Operator::Equals | Operator::NotEquals), left, right) => {
                let (Operation::Operand(left), Operation::Operand(right)) = (&**left, &**right) else { return Narrowing::default() };
                let narrowing = self.comparison(left, right).or_else(|| self.comparison(right, left)).unwrap_or_default();
                if *op == Operator::NotEquals { narrowing.negated() } else { narrowing }
            }
            _ => Narrowing::default(),
        }
    }

    /// Whether `call` calls the preset global function `name` (not a method,
    /// and not while a local or the files' own global hides it)
    fn calls_library_global(&self, call: &FunctionCall, name: &str) -> bool {
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
    fn comparison(&self, tested: &Expression, against: &Expression) -> Option<Narrowing> {
        let Expression::Literal(literal) = against else { return None };
        if literal.is_nil() {
            let (id, ty) = self.narrowable(tested)?;
            return Some(Narrowing { when_true: vec![(id, Type::NIL)], when_false: vec![(id, ty.without(TypeSet::NIL))] })
        }
        // `type(x)`, while `type` is the library's
        let Expression::FunctionCall(call) = tested else { return None };
        if !self.calls_library_global(call, "type") {
            return None
        }
        let [argument] = call.arguments().try_into().ok()?;
        let (id, ty) = self.narrowable(&argument)?;
        let set = type_name_set(string_field_name(&literal.get_string()?)?)?;
        Some(Narrowing { when_true: vec![(id, intersect(ty, set))], when_false: vec![(id, ty.without(set))] })
    }
}

/// `a.b` as (`a`, `b`), for an expression `a` and a field name `b`. None for
/// indexing (`a[b]`) and plain names
fn field_access(node: &SyntaxNode) -> Option<(Expression, SyntaxToken)> {
    let mut significant = node.children_with_tokens()
        .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment));
    let (Some(NodeOrToken::Node(base)), Some(NodeOrToken::Token(dot)), Some(NodeOrToken::Token(field)), None) =
        (significant.next(), significant.next(), significant.next(), significant.next()) else { return None };
    if dot.kind() != SyntaxKind::Dot || field.kind() != SyntaxKind::Name {
        return None
    }
    Some((Expression::cast(base)?, field))
}

/// A pending call's arguments, with what's passed on as settled so far
fn resolve_pending(call: &PendingCall, parameters: &HashMap<FunctionId, Vec<Type>>, varargs: &HashMap<FunctionId, Values>) -> Values {
    let mut values = call.known.clone();
    for &(position, function, index, narrowed) in &call.parameters {
        let ty = parameters.get(&function).and_then(|p| p.get(index)).copied().unwrap_or(Type::UNKNOWN);
        let ty = match narrowed {
            Some(set) => ty.without(TypeSet::UNKNOWN.without(set)),
            None => ty,
        };
        if let Some(value) = values.values.get_mut(position) {
            *value = ty;
        }
    }
    if let Some((position, function)) = call.varargs {
        let passed = varargs.get(&function).cloned().unwrap_or_else(Values::unknown);
        values.values.truncate(position);
        values.values.extend(passed.values);
        values.rest = passed.rest;
    }
    values
}

/// `t[k]`: the table and the key
fn index_access(node: &SyntaxNode) -> Option<(Expression, Expression)> {
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
fn string_field_name(literal: &str) -> Option<&str> {
    let literal = literal.trim();
    let quote = literal.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let name = literal.strip_prefix(quote)?.strip_suffix(quote)?;
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    (starts_well && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(name)
}

/// Whether every path through the block ends in a `return`. Conservative:
/// loops and `error()` calls count as possibly falling through
fn always_returns(block: &Block) -> bool {
    match block.statements().last() {
        Some(Statement::Return(_)) => true,
        Some(Statement::Do(d)) => d.block().is_some_and(|b| always_returns(&b)),
        Some(Statement::If(chain)) => {
            chain.else_branch().and_then(|e| e.block()).is_some_and(|b| always_returns(&b))
                && chain.if_branches().iter().all(|branch| branch.block().is_some_and(|b| always_returns(&b)))
        }
        _ => false,
    }
}

fn precedence(op: Operator) -> u8 {
    match op {
        Operator::Or => 1,
        Operator::And => 2,
        Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals
            | Operator::GreaterThanOrEquals | Operator::NotEquals | Operator::Equals => 3,
        Operator::Concatenate => 4,
        Operator::Add | Operator::Subtract => 5,
        Operator::Multiply | Operator::Divide | Operator::Modulo => 6,
        Operator::Hat => 7,
        _ => 0,
    }
}

fn combine(op: Operator, left: Type, right: Type) -> Type {
    match op {
        // The right side only runs if the left can be falsy (`or`) or truthy (`and`)
        Operator::Or if left.set.falsy() == TypeSet::NEVER => left,
        Operator::Or => left.truthy() | right,
        Operator::And if left.set.truthy() == TypeSet::NEVER => left,
        Operator::And => left.falsy() | right,
        Operator::Concatenate => Type::of(TypeSet::STRING),
        Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals
            | Operator::GreaterThanOrEquals | Operator::NotEquals | Operator::Equals => Type::of(TypeSet::BOOLEAN),
        // Ignores metatables (__add and friends)
        _ => Type::of(TypeSet::NUMBER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::syntax::Generator;

    fn analyse(source: &str) -> Analysis {
        analyse_with(source, &Arc::new(Definitions::default()))
    }

    fn analyse_with(source: &str, definitions: &Arc<Definitions>) -> Analysis {
        let mut generator = Generator::new(source);
        let root = SyntaxNode::new_root(generator.process_all());
        Analysis::analyse_file(&root, definitions)
    }

    /// Type at each use of `name`, in source order
    fn types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse(source), name)
    }

    fn describe_uses(file: &Analysis, name: &str) -> Vec<String> {
        file.references.iter()
            .filter(|r| file.binding(r).name.as_ref() == name)
            .map(|r| file.describe(r.types))
            .collect()
    }

    #[test]
    fn recursive_returns_settle() {
        // Each walk would add a position to what `f` returns
        let analysis = analyse("local function f() return 1, f() end local a, b, c = f()");
        assert!(analysis.walks <= 5, "{} walks", analysis.walks);
        assert_eq!(describe_uses(&analysis, "c")[0], "number");
        let source = "local function f(stop) if stop then return 1 end return 1, f(true) end local a, b, c = f()";
        assert_eq!(types_of(source, "b")[0], "number?");
        assert_eq!(types_of(source, "c")[0], "number?");
        // Through another function, and through `pcall`
        let source = "local g local function f(x) if x then return 1 end return 'a', g() end g = function() return true, f() end local a, b, c = f()";
        assert_eq!(types_of(source, "a")[0], "number|string");
        assert_eq!(types_of(source, "c")[0], "number|string|true|nil");
        let source = "local function f(x) if x then return 1 end return 2, select(2, pcall(f, true)) end local a, b, c = f()";
        assert!(analyse(source).walks <= 6);
    }

    #[test]
    fn literals() {
        assert_eq!(types_of("local x = 5", "x"), ["number"]);
        assert_eq!(types_of("local x = 'a'", "x"), ["string"]);
        assert_eq!(types_of("local x = true", "x"), ["true"]);
        assert_eq!(types_of("local x = nil", "x"), ["nil"]);
        assert_eq!(types_of("local x = {}", "x"), ["table"]);
        assert_eq!(types_of("local x = function() end", "x"), ["function()"]);
        assert_eq!(types_of("local x", "x"), ["nil"]);
    }

    #[test]
    fn operators() {
        assert_eq!(types_of("local a = 1 + 2 * 3", "a"), ["number"]);
        assert_eq!(types_of("local a = 'a' .. 1", "a"), ["string"]);
        assert_eq!(types_of("local a = 1 < 2", "a"), ["boolean"]);
        assert_eq!(types_of("local a = not x", "a"), ["boolean"]);
        assert_eq!(types_of("local a = #t", "a"), ["number"]);
        assert_eq!(types_of("local a = -#t", "a"), ["number"]);
        assert_eq!(types_of("local a = 2 ^ 3", "a"), ["number"]);
        assert_eq!(types_of("local a = (1 .. 2)", "a"), ["string"]);
    }

    #[test]
    fn and_or() {
        assert_eq!(types_of("local a = x and 1 or 's'", "a"), ["number|string"]);
        assert_eq!(types_of("local a = x or 0", "a"), ["unknown (truthy)"]);
        assert_eq!(types_of("local y = nil local a = y or 0", "a"), ["number"]);
        assert_eq!(types_of("local y = 1 local a = y and 's'", "a"), ["string"]);
        // `or` is lower precedence than `==`
        assert_eq!(types_of("local a = 1 == 2 or 's'", "a"), ["string|true"]);
        assert_eq!(types_of("local y = 's' local a = y or 0", "a"), ["string"]);
        assert_eq!(types_of("local a = nil and 1", "a"), ["nil"]);
    }

    #[test]
    fn reassignment() {
        assert_eq!(types_of("local x = 5 x = 'a' local y = x", "x"), ["number", "string", "string"]);
    }

    #[test]
    fn variables_flow_into_others() {
        assert_eq!(types_of("local x = 5 local y = x + 1 local z = x", "z"), ["number"]);
    }

    #[test]
    fn shadowing() {
        assert_eq!(types_of("local x = 1 do local x = 's' end local y = x", "y"), ["number"]);
        assert_eq!(types_of("local x = 1 local x = x .. 'a' local y = x", "y"), ["string"]);
    }

    #[test]
    fn do_block_assignment_replaces() {
        assert_eq!(types_of("local x = 1 do x = 's' end local y = x", "y"), ["string"]);
    }

    #[test]
    fn conditional_assignment_unions() {
        assert_eq!(types_of("local x = 1 if c then x = 's' end local y = x", "y"), ["number|string"]);
        assert_eq!(types_of("local x = 1 while c do x = 's' end local y = x", "y"), ["number|string"]);
        assert_eq!(types_of("local x if c then x = 1 else x = 's' end local y = x", "y"), ["number|string|nil"]);
    }

    #[test]
    fn loops_carry_types_to_the_next_iteration() {
        assert_eq!(types_of("local x = 1 while c do local y = x x = 's' end", "y"), ["number|string"]);
        assert_eq!(types_of("local x = 1 repeat local y = x x = 's' until x", "y"), ["number|string"]);
        assert_eq!(types_of("local x = 1 for i = 1, 2 do local y = x x = {} end", "y"), ["number|table"]);
        assert_eq!(types_of("local x = 1 for k in pairs(t) do local y = x x = {} end", "y"), ["number|table"]);
        // The `while` condition is checked again after each iteration
        assert_eq!(types_of("local x = 1 while x do x = nil end", "x"), ["number", "number?", "nil"]);
        // Unchanged types need no extra pass
        assert_eq!(types_of("local x = 1 while c do local y = x end", "y"), ["number"]);
    }

    #[test]
    fn loop_carried_chains_converge() {
        let source = "local a, b, c = 1, 1, 1 while w do local y = c c = b b = a a = 's' end";
        assert_eq!(types_of(source, "y"), ["number|string"]);
        let nested = "local x = 1 while p do while q do local y = x x = 's' end x = {} end";
        assert_eq!(types_of(nested, "y"), ["number|string|table"]);
    }

    /// Each pass moves `string` one step down the chain, so this needs more
    /// passes than LOOP_PASSES_BEFORE_WIDENING. The variables still changing are
    /// widened to unknown, which still includes every real type
    #[test]
    fn long_loop_chains_widen() {
        let names: Vec<String> = (0..12).map(|i| format!("v{i}")).collect();
        let mut source = format!("local {} = {} while w do local y = v11 ", names.join(", "), vec!["1"; 12].join(", "));
        for i in (1..12).rev() {
            source += &format!("v{i} = v{} ", i - 1);
        }
        source += "v0 = 's' end";
        let file = analyse(&source);
        let y = file.references.iter().find(|r| file.binding(r).name.as_ref() == "y").unwrap();
        assert!(y.types.set.contains(TypeSet::NUMBER | TypeSet::STRING), "{}", y.types.set);
        assert_eq!(y.types.set, TypeSet::UNKNOWN);
        assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "y").count(), 1);
    }

    #[test]
    fn loop_passes_leave_one_set_of_records() {
        let file = analyse("local x = 1 while c do local y = x x = 's' print(Global) end");
        assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "y").count(), 1);
        assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "Global").count(), 1);
        assert_eq!(file.references.iter().filter(|r| file.binding(r).name.as_ref() == "x").count(), 3);
        assert_eq!(file.references.iter().filter(|r| file.binding(r).name.as_ref() == "print").count(), 1);
    }

    #[test]
    fn locals_inside_branches_replace() {
        assert_eq!(types_of("if c then local x = 1 x = 's' local y = x end", "y"), ["string"]);
    }

    #[test]
    fn upvalues_see_every_assignment() {
        let source = "local x = 1 local function f() return x end x = 's'";
        assert_eq!(types_of(source, "x"), ["number", "number|string", "string"]);
    }

    #[test]
    fn multiple_values() {
        assert_eq!(types_of("local a, b = 1", "b"), ["nil"]);
        assert_eq!(types_of("local a, b = f()", "b"), ["unknown"]);
        assert_eq!(types_of("local a, b = ...", "b"), ["unknown"]);
        assert_eq!(types_of("local a, b = 1, 's'", "b"), ["string"]);
    }

    #[test]
    fn globals() {
        assert_eq!(types_of("Foo = {} local y = Foo", "y"), ["table"]);
        assert_eq!(types_of("local y = Bar", "y"), ["unknown"]);
        assert_eq!(types_of("function Baz() end local y = Baz", "y"), ["function()"]);
        let file = analyse("Foo = 1");
        assert_eq!(file.bindings[0].kind, BindingKind::Global);
    }

    #[test]
    fn loops() {
        assert_eq!(types_of("for i = 1, 3 do local j = i end", "j"), ["number"]);
        assert_eq!(types_of("for k, v in ipairs(t) do end", "k"), ["number"]);
        assert_eq!(types_of("for k, v in ipairs(t) do end", "v"), ["unknown (not nil)"]);
        assert_eq!(types_of("for k, v in next, t do end", "v"), ["unknown"]);
        assert_eq!(types_of("local ipairs = pairs for k in ipairs(t) do end", "k"), ["unknown (not nil)"]);
    }

    #[test]
    fn repeat_condition_sees_block_locals() {
        let file = analyse("repeat local q = 1 until q");
        let qs: Vec<_> = file.references.iter().filter(|r| file.binding(r).name.as_ref() == "q").collect();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].binding, qs[1].binding);
        assert_eq!(file.binding(qs[1]).kind, BindingKind::Local);
    }

    #[test]
    fn parameters_and_self() {
        assert_eq!(types_of("function T:m(a) return self, a end", "self"), ["table"]);
        assert_eq!(types_of("function T:m(a) return self, a end", "a"), ["unknown", "unknown"]);
        assert_eq!(types_of("local function f(a) end", "f"), ["function(a)"]);
    }

    #[test]
    fn field_names_are_not_variables() {
        assert!(types_of("local t = {a = 1}", "a").is_empty());
        assert!(types_of("local t = {} local y = t.x", "x").is_empty());
        assert!(types_of("local t = {} t.x = 1", "x").is_empty());
        assert_eq!(types_of("local k = 1 local t = {[k] = 1}", "k").len(), 2);
        assert_eq!(types_of("local t = {} local y = t[t]", "t").len(), 3);
        assert_eq!(types_of("local t = {} local y = t.x", "t"), ["table", "table"]);
    }

    #[test]
    fn calls_resolve_arguments() {
        assert_eq!(types_of("local a = 1 print(a)", "a"), ["number", "number"]);
        assert_eq!(types_of("local a = 1 local o = {} o:m(a).z(a)", "a").len(), 3);
    }

    #[test]
    fn call_results() {
        assert_eq!(types_of("local function f() return 1 end local x = f()", "x"), ["number"]);
        assert_eq!(types_of("local f = function() return true end local g = f local x = g()", "x"), ["true"]);
        assert_eq!(types_of("function F() return {} end local x = F()", "x"), ["table"]);
        assert_eq!(types_of("local function f() return 1 end local x = f() .. 'a'", "x"), ["string"]);
        assert_eq!(types_of("local function f() return 's' end local x = f() or 1", "x"), ["string"]);
    }

    #[test]
    fn multiple_results() {
        let source = "local function f() return 1, 's' end local a, b, c = f()";
        assert_eq!(types_of(source, "a"), ["number"]);
        assert_eq!(types_of(source, "b"), ["string"]);
        assert_eq!(types_of(source, "c"), ["nil"]);
        // Only the last expression expands
        assert_eq!(types_of("local function f() return 1, 's' end local a, b = f(), true", "b"), ["true"]);
        assert_eq!(types_of("local function f() return 1, 's' end local a, b = (f())", "b"), ["nil"]);
        assert_eq!(types_of("local function f() return 1, 's' end local function g() return f() end local a, b = g()", "b"), ["string"]);
        // `...` is what every call passed: nothing here, so `a` is nil
        assert_eq!(types_of("local function f(...) return ... end local a = f()", "a"), ["nil"]);
        assert_eq!(types_of("local function f(...) return ... end local a, b = f(1, 's')", "b"), ["string"]);
        assert_eq!(types_of("local function f() return 1, ... end local a, b = f()", "b"), ["unknown"]);
    }

    #[test]
    fn returns_from_every_path() {
        assert_eq!(types_of("local function f(x) if x then return 1 end return 's' end local y = f()", "y"), ["number|string"]);
        // Falling off the end returns nothing
        assert_eq!(types_of("local function f(x) if x then return 1 end end local y = f()", "y"), ["number?"]);
        assert_eq!(types_of("local function f(x) if x then return 1 else return 2 end end local y = f()", "y"), ["number"]);
        assert_eq!(types_of("local function f(x) if x then return 1 elseif z then return 2 end end local y = f()", "y"), ["number?"]);
        assert_eq!(types_of("local function f() do return 1 end end local y = f()", "y"), ["number"]);
        assert_eq!(types_of("local function f() while c do return 1 end end local y = f()", "y"), ["number?"]);
        assert_eq!(types_of("local function f() end local y = f()", "y"), ["nil"]);
        assert_eq!(types_of("local function f() return end local y = f()", "y"), ["nil"]);
    }

    #[test]
    fn calls_to_upvalues_and_later_functions() {
        // The usual addon shape: a local helper called from a method
        let source = "local function helper() return {} end function Addon:Load() local t = helper() end";
        assert_eq!(types_of(source, "t"), ["table"]);
        // Defined further down the file
        let source = "local function a() return B() end function B() return 's' end local y = a()";
        assert_eq!(types_of(source, "y"), ["string"]);
        // Reassigned later, so the closure could call either
        let source = "local function g() return 1 end local function f() return g() end g = function() return 's' end local y = f()";
        assert_eq!(types_of(source, "y"), ["unknown"]);
    }

    #[test]
    fn unknown_calls() {
        assert_eq!(types_of("local y = print()", "y"), ["unknown"]);
        assert_eq!(types_of("local function f(g) return g() end local y = f()", "y"), ["unknown"]);
        assert_eq!(types_of("local f = c and function() return 1 end or function() return 2 end local y = f()", "y"), ["unknown"]);
        // Recursion converges on the first walk's unknown
        let source = "local function f(n) if n then return f(n) end return 1 end local y = f()";
        assert!(analyse(source).references.iter().any(|r| r.types.set.contains(TypeSet::NUMBER)));
    }

    #[test]
    fn function_values_through_and_or() {
        // `and` passes a falsy left side through, but calling one of those fails
        assert_eq!(types_of("local function f() return 1 end local g = x and f local y = g()", "y"), ["number"]);
        assert_eq!(types_of("local function f() return 1 end local g = f or nil local y = g()", "y"), ["number"]);
        assert_eq!(types_of("local function f() return 1 end local g = nil local h = g or f local y = h()", "y"), ["number"]);
    }

    #[test]
    fn signatures() {
        assert_eq!(types_of("local function f(a, ...) return 1, 's' end", "f"), ["function(a, ...) -> number, string"]);
        assert_eq!(types_of("local function f() return g() end", "f"), ["function() -> ..."]);
        assert_eq!(types_of("local function f(x) if x then return 1 end end", "f"), ["function(x) -> number?"]);
        assert_eq!(types_of("local function f() return 1 end local g = c and f", "g"), ["false|(function() -> number)|nil"]);
        assert_eq!(types_of("local function f() return f end", "f"), ["function() -> function", "function() -> function"]);
    }

    fn builtins() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{"functions": {
            "tostring": {"parameters": ["value"], "returns": ["string"]},
            "tonumber": {"parameters": ["value", "base: number|nil"], "returns": ["number|nil"]},
            "select": {"parameters": ["index", "..."], "returns": ["unknown"], "rest": "unknown"},
            "floor": {"parameters": ["x: number"], "returns": ["number"]},
            "max": {"parameters": ["x: number", "...: number"], "returns": ["number"]},
            "strlen": {"parameters": ["s: string"], "returns": ["number"]}
        }, "tables": {
            "string": {"functions": {"format": {"parameters": ["format: string", "..."], "returns": ["string"]}}},
            "a": {"tables": {"b": {"functions": {"c": {"returns": ["table"]}}}}}
        }}"#).unwrap())
    }

    fn builtin_types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse_with(source, &builtins()), name)
    }

    #[test]
    fn preset_globals() {
        assert_eq!(builtin_types_of("local x = tostring(1)", "x"), ["string"]);
        assert_eq!(builtin_types_of("local x = tonumber('1')", "x"), ["number?"]);
        assert_eq!(builtin_types_of("local x = tostring", "x"), ["function(value) -> string"]);
        assert_eq!(builtin_types_of("local x, y = select(2, 'a', 'b')", "x"), ["string"]);
        assert_eq!(builtin_types_of("local x, y = select(2, 'a', 'b')", "y"), ["nil"]);
        assert_eq!(builtin_types_of("local x, y = select(2, f())", "y"), ["unknown"]);
        assert_eq!(builtin_types_of("local x = undefined()", "x"), ["unknown"]);
        // Inside functions too, where globals are read like upvalues
        assert_eq!(builtin_types_of("function F() local x = tostring(1) end", "x"), ["string"]);
    }

    #[test]
    fn localised_presets() {
        // The usual WoW idiom of caching globals in locals
        let source = "local tostring, tonumber = tostring, tonumber function F() local x = tostring(1) end";
        assert_eq!(builtin_types_of(source, "x"), ["string"]);
    }

    #[test]
    fn preset_fields() {
        assert_eq!(builtin_types_of("local x = string.format('%d', 1)", "x"), ["string"]);
        assert_eq!(builtin_types_of("local f = string.format local x = f()", "x"), ["string"]);
        assert_eq!(builtin_types_of("local x = a.b.c()", "x"), ["table"]);
        assert_eq!(builtin_types_of("local x = string.nope()", "x"), ["unknown"]);
        // Members are fields of the table, so they work through other variables
        assert_eq!(builtin_types_of("local s = string local x = s.format('%d', 1)", "x"), ["string"]);
        assert_eq!(builtin_types_of("local string = string function F() local x = string.format() end", "x"), ["string"]);
        assert_eq!(builtin_types_of("local b = a.b local x = b.c()", "x"), ["table"]);
        // A namespace is shown as a plain table
        assert_eq!(builtin_types_of("local s = string", "s"), ["table"]);
        assert_eq!(builtin_types_of("local x = string['format']()", "x"), ["string"]);
        // A local named `string` isn't the library
        assert_eq!(builtin_types_of("local string = {} local x = string.format()", "x"), ["unknown"]);
    }

    #[test]
    fn files_can_replace_presets() {
        assert_eq!(builtin_types_of("function tostring() return 1 end local x = tostring()", "x"), ["number"]);
        // Assigned further down still counts, via the second walk
        assert_eq!(builtin_types_of("local x = tostring() tostring = nil", "x"), ["unknown"]);
        assert_eq!(builtin_types_of("local function tostring() return {} end local x = tostring()", "x"), ["table"]);
        assert_eq!(builtin_types_of("string = {} local x = string.format()", "x"), ["unknown"]);
    }

    fn enums() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{
            "types": {"Enum.Quality": {"base": "number"}, "Enum.Layer": {"base": "number"}},
            "tables": {"Enum": {"tables": {
                "Quality": {"values": {"Poor": "Enum.Quality", "Good": "Enum.Quality"}},
                "Layer": {"values": {"Top": "Enum.Layer"}}
            }}},
            "functions": {"GetQuality": {"returns": ["Enum.Quality|nil"]}}
        }"#).unwrap())
    }

    fn enum_types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse_with(source, &enums()), name)
    }

    #[test]
    fn enum_members() {
        assert_eq!(enum_types_of("local q = Enum.Quality.Poor", "q"), ["Enum.Quality"]);
        assert_eq!(enum_types_of("local q = c and Enum.Quality.Poor or Enum.Quality.Good", "q"), ["Enum.Quality"]);
        assert_eq!(enum_types_of("local Q = Enum.Quality local q = Q.Good", "q"), ["Enum.Quality"]);
        assert_eq!(enum_types_of("local q = GetQuality()", "q"), ["Enum.Quality?"]);
        // The enum's own table, and its parent, are plain tables
        assert_eq!(enum_types_of("local Q = Enum.Quality", "Q"), ["table"]);
    }

    #[test]
    fn enums_mix_into_plain_numbers() {
        // Arithmetic gives a plain number
        assert_eq!(enum_types_of("local n = Enum.Quality.Poor + 1", "n"), ["number"]);
        // So does mixing with a plain number, or another enum
        assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or 5", "n"), ["number"]);
        assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or Enum.Layer.Top", "n"), ["number"]);
        // Other types alongside keep the enum
        assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or 's'", "n"), ["Enum.Quality|string"]);
    }

    #[test]
    fn mandated_parameters() {
        assert_eq!(builtin_types_of("local function f(x) return floor(x) end", "x"), ["number", "number"]);
        assert_eq!(builtin_types_of("local function f(s) return string.format(s) end", "s"), ["string", "string"]);
        // Varargs can require a type too
        assert_eq!(builtin_types_of("local function f(a, b) return max(a, b) end", "b"), ["number", "number"]);
        // Globals and methods can be called from anywhere, but still have to obey
        assert_eq!(builtin_types_of("function T:m(x) return strlen(x) end", "x"), ["string", "string"]);
        assert_eq!(builtin_types_of("function F(x) local y = floor(x) end", "x"), ["number", "number"]);
        // What the body requires beats what callers pass
        assert_eq!(builtin_types_of("local function f(x) return floor(x) end f('1')", "x"), ["number", "number"]);
    }

    #[test]
    fn requirements_must_always_run() {
        let unknown = ["unknown", "unknown"];
        assert_eq!(builtin_types_of("function F(x) if c then floor(x) end end", "x"), unknown);
        assert_eq!(builtin_types_of("function F(x) if type(x) == 'number' then return floor(x) end end", "x")[..1], ["unknown"]);
        assert_eq!(builtin_types_of("function F(x) while c do floor(x) end end", "x"), unknown);
        assert_eq!(builtin_types_of("function F(x) return c and floor(x) end", "x"), unknown);
        assert_eq!(builtin_types_of("function F(x) return floor(x) or 1 end", "x"), ["number", "number"]);
        assert_eq!(builtin_types_of("function F(x) if c then elseif floor(x) then end end", "x"), unknown);
        assert_eq!(builtin_types_of("function F(x) if floor(x) then end end", "x"), ["number", "number"]);
        // A closure might never be called
        assert_eq!(builtin_types_of("function F(x) return function() floor(x) end end", "x"), unknown);
        // After reassignment, the call isn't seeing the caller's value
        assert_eq!(builtin_types_of("function F(x) x = x or 1 floor(x) end", "x")[0], "unknown");
        // Optional parameters and ones taking anything don't require a type
        assert_eq!(builtin_types_of("function F(x) tonumber('1', x) tostring(x) end", "x"), ["unknown"; 3]);
    }

    #[test]
    fn conflicting_requirements_are_ignored() {
        assert_eq!(builtin_types_of("function F(x) floor(x) strlen(x) end", "x"), ["unknown"; 3]);
    }

    #[test]
    fn requirements_pass_through_user_functions() {
        let source = "local function g(n) return floor(n) end function F(x) g(x) end";
        assert_eq!(builtin_types_of(source, "x"), ["number", "number"]);
        // Even when the required function is defined further down
        let source = "function F(x) G(x) end function G(n) return floor(n) end";
        assert_eq!(builtin_types_of(source, "x"), ["number", "number"]);
    }

    #[test]
    fn parameters_from_call_sites() {
        assert_eq!(types_of("local function f(x) return x end f(1) f('a')", "x"), ["number|string", "number|string"]);
        assert_eq!(types_of("local function f(x) return x end local y = f(1)", "y"), ["number"]);
        assert_eq!(types_of("local function f(a, b) end f(1)", "b"), ["nil"]);
        assert_eq!(types_of("local f = function(x) end f({})", "x"), ["table"]);
        // Calls from inside other functions count too
        assert_eq!(types_of("local function f(x) end function G() f(true) end", "x"), ["true"]);
        // Never called, so there's nothing to go on
        assert_eq!(types_of("local function f(x) end", "x"), ["unknown"]);
        // Arguments expand like any other expression list
        assert_eq!(types_of("local function g() return 1, 's' end local function f(a, b) end f(g())", "b"), ["string"]);
    }

    #[test]
    fn call_site_types_chain_through_functions() {
        let source = "local function c(z) end local function b(y) c(y) end local function a(x) b(x) end a(1)";
        assert_eq!(types_of(source, "z"), ["number"]);
        let source = "local function f(n) if n > 0 then return f(n - 1) end return 0 end f(10)";
        assert_eq!(types_of(source, "n")[0], "number");
    }

    #[test]
    fn escaping_functions_have_unknown_parameters() {
        // Stored, passed on, returned, or global: other callers could pass anything
        assert_eq!(types_of("local function f(x) end f(1) T.cb = f", "x"), ["unknown"]);
        assert_eq!(types_of("local function f(x) end f(1) pcall(f, 's')", "x"), ["unknown"]);
        assert_eq!(types_of("local function f(x) end f(1) local g = f", "x"), ["unknown"]);
        assert_eq!(types_of("local function f(x) end f(1) return f", "x"), ["unknown"]);
        assert_eq!(types_of("function F(x) end F(1) local g = F", "x"), ["unknown"]);
        assert_eq!(types_of("local t = {} function t.f(x) end t.f(1) pcall(t.f)", "x"), ["unknown"]);
        assert_eq!(types_of("pcall(function(x) end, 1)", "x"), ["unknown"]);
        // Holding two different functions, so a call doesn't say which one ran
        assert_eq!(types_of("local f = function(x) end f = function(y) end f(1)", "x"), ["unknown"]);
    }

    #[test]
    fn typed_signatures() {
        assert_eq!(builtin_types_of("local function f(x, y) return floor(x) end f(1, 's')", "f")[0], "function(x: number, y: string) -> number");
        assert_eq!(builtin_types_of("local f = string.format", "f"), ["function(format: string, ...) -> string"]);
        assert_eq!(builtin_types_of("local f = max", "f"), ["function(x: number, ...: number) -> number"]);
    }

    #[test]
    fn optional_types() {
        assert_eq!(types_of("local x = c and 1 or nil", "x"), ["number?"]);
        // More than one other type keeps `|nil`, as `number|string?` would be ambiguous
        assert_eq!(types_of("local x = c and 1 or (d and 's' or nil)", "x"), ["number|string|nil"]);
        assert_eq!(types_of("local x = nil", "x"), ["nil"]);
        assert_eq!(types_of("local x = c and f or nil local y = x", "y"), ["unknown (not false)"]);
        assert_eq!(types_of("local function f() return 1 end local g = c and f or nil", "g"), ["(function() -> number)?"]);
    }

    fn objects() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{
            "types": {"Frame": {"base": "table"}, "Texture": {"base": "table"}, "Curve": {"base": "userdata"}},
            "functions": {
                "CreateFrame": {"parameters": ["frameType: string"], "returns": ["Frame"]},
                "GetMouseFocus": {"returns": ["Frame|nil"]},
                "CreateCurve": {"returns": ["Curve"]},
                "GetTexture": {"returns": ["Texture"]},
                "SetPortrait": {"parameters": ["texture: Texture"]}
            }
        }"#).unwrap())
    }

    fn object_types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse_with(source, &objects()), name)
    }

    #[test]
    fn named_object_types() {
        assert_eq!(object_types_of("local f = CreateFrame('Frame')", "f"), ["Frame"]);
        assert_eq!(object_types_of("local f = GetMouseFocus()", "f"), ["Frame?"]);
        assert_eq!(object_types_of("local c = CreateCurve()", "c"), ["Curve"]);
        assert_eq!(object_types_of("local f = CreateFrame", "f"), ["function(frameType: string) -> Frame"]);
        // Still a table, so table operations keep it
        assert_eq!(object_types_of("local f = GetMouseFocus() or CreateFrame('Frame')", "f"), ["Frame"]);
    }

    #[test]
    fn mixing_named_types() {
        // Different named types are either, a named type and a plain table just a table
        assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or GetTexture()", "f"), ["Texture|Frame"]);
        assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or {}", "f"), ["table"]);
        assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or 1", "f"), ["number|Frame"]);
        assert_eq!(object_types_of("local f = CreateFrame('Frame') if c then f = CreateFrame('Frame') end local g = f", "g"), ["Frame"]);
    }

    #[test]
    fn named_parameter_requirements() {
        assert_eq!(object_types_of("function F(t) SetPortrait(t) end", "t"), ["Texture", "Texture"]);
        assert_eq!(object_types_of("local function f(t) end f(CreateFrame('Frame'))", "t"), ["Frame"]);
    }

    #[test]
    fn array_entries() {
        assert_eq!(types_of("local t = { 'a', 'b' }", "t"), ["string[]"]);
        assert_eq!(types_of("local t = { 'a', 1 }", "t"), ["(number|string)[]"]);
        assert_eq!(types_of("local t = { 'a' } local a = t[1]", "a"), ["string?"]);
        assert_eq!(types_of("local t = { 'a' } local i = 1 local a = t[i]", "a"), ["string?"]);
        // Nothing known: still unknown rather than nil
        assert_eq!(types_of("local t = {} local a = t[1]", "a"), ["unknown"]);
        // Nested tables show as plain tables
        assert_eq!(types_of("local t = { { 1 } } local a = t[1][1]", "a"), ["number?"]);
        assert_eq!(types_of("local t = { { 1 } }", "t"), ["table[]"]);
        // A table holding itself
        assert_eq!(types_of("local t = {} t[1] = t", "t")[0], "table[]");
        // Unknown values are just unknown, as nothing stored is nil
        assert_eq!(types_of("local function f(x) local t = { x } end", "t"), ["unknown[]"]);
    }

    #[test]
    fn array_items_are_read() {
        // `{ x }` is an item, so `x` is read, not taken as a key
        assert_eq!(types_of("local x = 1 local t = { x }", "x"), ["number", "number"]);
    }

    #[test]
    fn computed_keys() {
        assert_eq!(types_of("local t = {} t[1] = 'a'", "t"), ["string[]", "string[]"]);
        assert_eq!(types_of("local t = { [true] = 1 }", "t"), ["table<true, number>"]);
        assert_eq!(types_of("local k = 'x' .. 1 local t = {} t[k] = 1", "t"), ["table<string, number>", "table<string, number>"]);
        // `t["name"]` is the field `name`
        assert_eq!(types_of("local t = {} t['n'] = 1 local a = t.n", "a"), ["number"]);
        assert_eq!(types_of("local t = { n = 1 } local a = t['n']", "a"), ["number"]);
        // A string key could be any named field
        assert_eq!(types_of("local t = { n = 1 } local k = 'x' .. 1 local a = t[k]", "a"), ["number?"]);
        // Storing nil removes
        assert_eq!(types_of("local t = {} t[1] = nil", "t"), ["table", "table"]);
        // Only named fields: still a plain table
        assert_eq!(types_of("local t = { n = 1 }", "t"), ["table"]);
        // Mixed: named fields are string keys
        assert_eq!(types_of("local t = { n = 1, 'a' }", "t"), ["table<number|string, number|string>"]);
    }

    /// The repository's definitions, for the real Lua and WoW libraries.
    /// Loaded once, as they're large
    fn library() -> Arc<Definitions> {
        static LIBRARY: std::sync::OnceLock<Arc<Definitions>> = std::sync::OnceLock::new();
        LIBRARY.get_or_init(|| Arc::new(crate::definitions::load(None))).clone()
    }

    fn library_types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse_with(source, &library()), name)
    }

    #[test]
    fn table_insert() {
        assert_eq!(library_types_of("local t = {} table.insert(t, 'a')", "t")[0], "string[]");
        assert_eq!(library_types_of("local t = {} table.insert(t, 1, 'a')", "t")[0], "string[]");
        assert_eq!(library_types_of("local t = {} tinsert(t, true)", "t")[0], "true[]");
        // A local named `table` isn't the library
        assert_eq!(library_types_of("local table = { insert = function() end } local t = {} table.insert(t, 'a')", "t")[0], "table");
    }

    #[test]
    fn iterating_contents() {
        let source = "local t = { 'a' } for i, v in ipairs(t) do end";
        assert_eq!(library_types_of(source, "i")[0], "number");
        assert_eq!(library_types_of(source, "v")[0], "string");
        let source = "local t = { n = 1, [true] = 'x' } for k, v in pairs(t) do end";
        assert_eq!(library_types_of(source, "k")[0], "string|true");
        assert_eq!(library_types_of(source, "v")[0], "number|string");
        // ipairs only visits number keys
        let source = "local t = { n = 1 } for i, v in ipairs(t) do end";
        assert_eq!(library_types_of(source, "v")[0], "unknown (not nil)");
        // Unknown contents keep the old guesses
        let source = "local function f(t) for k, v in pairs(t) do end end";
        assert_eq!(library_types_of(source, "k")[0], "unknown (not nil)");
        // Through a parameter, from what calls pass
        let source = "local function f(list) for _, b in ipairs(list) do b:Show() end end f({ CreateFrame('Button') })";
        assert_eq!(library_types_of(source, "b")[0], "Button");
    }

    #[test]
    fn string_methods() {
        assert_eq!(library_types_of("local s = 'x' local u = s:upper()", "u")[0], "string");
        assert_eq!(library_types_of("local u = ('x'):upper():lower()", "u")[0], "string");
        assert_eq!(library_types_of("local u = string.rep('x', 2):upper()", "u")[0], "string");
        assert_eq!(library_types_of("local function f(s) return s:len() end local n = f('x')", "n")[0], "number");
        // A string that may be nil still has them
        assert_eq!(library_types_of("local s if math.random() > 0.5 then s = 'x' end local u = s:upper()", "u")[0], "string");
        // Functions an addon adds to `string`
        assert_eq!(library_types_of("string.ext = function() return 1 end local n = ('x'):ext()", "n")[0], "number");
        // Not a string for sure: nothing to go on
        assert_eq!(library_types_of("local function f(v) return v:upper() end local u = f(1) f('x')", "u")[0], "unknown");
    }

    #[test]
    fn iterators() {
        let source = "for a, b in ('a1'):gmatch('(%a)(%d)') do end";
        assert_eq!(library_types_of(source, "a")[0], "string");
        assert_eq!(library_types_of(source, "b")[0], "string");
        assert_eq!(library_types_of("for w in string.gmatch('a b', '%a+') do end", "w")[0], "string");
        // A function the code defines: the variables are what it returns, the
        // first never nil as the loop stops then
        let source = "local function it() return 1, 'x' end for n, s in it do end";
        assert_eq!(library_types_of(source, "n")[0], "number");
        assert_eq!(library_types_of(source, "s")[0], "string");
        let source = "local function counter() local i = 0 return function() i = i + 1 if i < 3 then return i end end end for n in counter() do end";
        assert_eq!(library_types_of(source, "n")[0], "number");
    }

    #[test]
    fn metatables() {
        // setmetatable gives back the table, with its fields
        assert_eq!(library_types_of("local t = setmetatable({ a = 1 }, {}) local x = t.a", "x")[0], "number");
        // Missing fields come from `__index`
        assert_eq!(library_types_of("local B = { hi = 'x' } local t = setmetatable({}, { __index = B }) local x = t.hi", "x")[0], "string");
        assert_eq!(library_types_of("local t = setmetatable({}, { __index = function(t, k) return 5 end }) local x = t.any", "x")[0], "number");
        // A table's own fields win
        assert_eq!(library_types_of("local B = { v = 'x' } local t = setmetatable({ v = 1 }, { __index = B }) local x = t.v", "x")[0], "number");
        // The class pattern, two levels deep
        let source = "local A = {} A.__index = A function A:speak() return 'x' end
            local D = setmetatable({}, { __index = A }) D.__index = D function D:fetch() return 1 end
            local d = setmetatable({}, D) local s = d:speak() local n = d:fetch()";
        assert_eq!(library_types_of(source, "s")[0], "string");
        assert_eq!(library_types_of(source, "n")[0], "number");
        // A constructor setting fields on the object it returns
        let source = "local C = {} C.__index = C function C.new() local o = setmetatable({}, C) o.name = 'x' return o end
            function C:get() return 1 end local o = C.new() local nm = o.name local g = o:get()";
        assert_eq!(library_types_of(source, "nm")[0], "string");
        assert_eq!(library_types_of(source, "g")[0], "number");
        // `__call` makes the table callable, given itself first
        assert_eq!(library_types_of("local t = setmetatable({}, { __call = function(self, a) return a end }) local x = t('s')", "x")[0], "string");
        // getmetatable gives back what was set
        assert_eq!(library_types_of("local M = { k = 1 } local t = setmetatable({}, M) local x = getmetatable(t).k", "x")[0], "number");
    }

    #[test]
    fn function_varargs() {
        let source = "local function f(fmt, ...) local a, b = ... local n = select('#', ...) local t = { ... } end f('x', 1, 's') f('y', 2)";
        assert_eq!(types_of(source, "a")[0], "number");
        assert_eq!(types_of(source, "b")[0], "string?");
        assert_eq!(library_types_of(source, "n")[0], "number");
        assert_eq!(types_of(source, "t")[0], "(number|string)[]");
        // The signature shows what's passed, without the nil of shorter calls
        assert_eq!(types_of(source, "f")[0], "function(fmt: string, ...: number|string)");
        // Passed straight on
        assert_eq!(types_of("local function g(...) return ... end local function f(...) return g(...) end local x = f(1)", "x"), ["number"]);
        // Unknown arguments are just unknown
        assert_eq!(types_of("local function f(...) end local function g(x) f(x) end local h = g", "f")[0], "function(...)");
        // A method's `...` doesn't include `self`
        assert_eq!(types_of("local o = {} function o:m(...) local a = ... end o:m('s')", "a"), ["string"]);
        // Some calls unseen, as it escapes into another variable: nothing known
        assert_eq!(types_of("local function f(...) local a = ... end local g = f g(1)", "a"), ["unknown"]);
        // A constructor's last call gives every value; others only their first
        assert_eq!(types_of("local function two() return 1, 's' end local t = { two() }", "t"), ["(number|string)[]"]);
        assert_eq!(types_of("local function two() return 1, 's' end local t = { two(), true }", "t"), ["(number|true)[]"]);
    }

    #[test]
    fn globals_table() {
        // `_G.name` and `_G["name"]` are the global, both ways
        assert_eq!(library_types_of("_G.Saved = { n = 1 } local x = Saved", "x")[0], "table");
        assert_eq!(library_types_of("Count = 5 local x = _G.Count", "x")[0], "number");
        assert_eq!(library_types_of("Count = 5 local x = _G['Count']", "x")[0], "number");
        assert_eq!(library_types_of("_G['Name'] = 's' local x = Name", "x")[0], "string");
        assert_eq!(library_types_of("local function f() return 'r' end _G.F = f local x = F()", "x")[0], "string");
        // Hover shows the global
        let analysis = analyse_with("Count = 5 local x = _G.Count", &library());
        let (text, _) = analysis.hover(FileId(0), TextSize::from(24)).unwrap();
        assert_eq!(text, "global Count: number");
        // The global even where a local hides it
        assert_eq!(library_types_of("local Count = 's' local x = _G.Count", "x")[0], "unknown");
        // A local named `_G` isn't the globals table
        assert_eq!(library_types_of("Count = 5 local _G = { Count = 's' } local x = _G.Count", "x")[0], "string");
    }

    #[test]
    fn standard_values() {
        assert_eq!(library_types_of("local p = math.pi", "p")[0], "number");
        assert_eq!(library_types_of("local h = math.huge", "h")[0], "number");
        assert_eq!(library_types_of("local a = math.atan2(1, 2)", "a")[0], "number");
        assert_eq!(library_types_of("local v = _VERSION", "v")[0], "string");
    }

    #[test]
    fn narrowing_by_truth() {
        let maybe = "local v if math.random() > 0.5 then v = 1 end ";
        assert_eq!(library_types_of(&format!("{maybe} if v then local x = v end"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} if not v then local x = v end"), "x")[0], "nil");
        assert_eq!(library_types_of(&format!("{maybe} if v then else local x = v end"), "x")[0], "nil");
        // `elseif` and `else` see the earlier conditions false
        assert_eq!(library_types_of(&format!("{maybe} if v == nil then elseif true then local x = v end"), "x")[0], "number");
        // Afterwards it's as before
        assert_eq!(library_types_of(&format!("{maybe} if v then end local x = v"), "x")[0], "number?");
        // Unless every branch returns, and there's no else
        assert_eq!(library_types_of(&format!("{maybe} if not v then return end local x = v"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} if v == nil then return end local x = v"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} if not v then return else end local x = v"), "x")[0], "number?");
        // Assigned in a branch: that's kept as well
        assert_eq!(library_types_of(&format!("{maybe} if v then v = 's' end local x = v"), "x")[0], "number|string|nil");
        // `and` and `or`
        assert_eq!(library_types_of(&format!("{maybe} local x = v and v + 1"), "x")[0], "number?");
        assert_eq!(library_types_of(&format!("{maybe} if v and v > 0 then local x = v end"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} if not v or v > 0 then else local x = v end"), "x")[0], "number");
        let t = "local t if math.random() > 0.5 then t = { n = 1 } end ";
        assert_eq!(library_types_of(&format!("{t} local x = t and t.n"), "x")[0], "number?");
        // `while`
        assert_eq!(library_types_of(&format!("{maybe} while v do local x = v v = nil end"), "x")[0], "number");
        // Upvalues aren't narrowed: a closure can run at any time
        assert_eq!(library_types_of(&format!("{maybe} if v then local f = function() local x = v end end"), "x")[0], "number?");
    }

    #[test]
    fn narrowing_by_type() {
        let source = "local function f(v) if type(v) == 'string' then local s = v elseif type(v) == 'number' then local n = v else local o = v end end f(1) f('a') f(true)";
        assert_eq!(library_types_of(source, "s")[0], "string");
        assert_eq!(library_types_of(source, "n")[0], "number");
        assert_eq!(library_types_of(source, "o")[0], "true");
        assert_eq!(library_types_of("local function f(v) if 'table' ~= type(v) then return end local t = v end f({}) f(1)", "t")[0], "table");
        // Unknown narrowed to what's tested
        assert_eq!(library_types_of("function F(v) if type(v) == 'function' then local x = v end end", "x")[0], "function");
        // A local named `type` isn't the library's
        assert_eq!(library_types_of("local type = function() return 'x' end local function f(v) if type(v) == 'string' then local s = v end end f(1)", "s")[0], "number");
    }

    #[test]
    fn library_returns_from_arguments() {
        let maybe = "local v if math.random() > 0.5 then v = 1 end ";
        assert_eq!(library_types_of(&format!("{maybe} local x, m = assert(v, 'msg')"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} local x, m = assert(v, 'msg')"), "m")[0], "string");
        // As a statement, what it checks holds afterwards
        assert_eq!(library_types_of(&format!("{maybe} assert(v) local x = v"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} assert(type(v) == 'number') local x = v"), "x")[0], "number");
        // pcall: success, then the returns or the error message
        let source = "local ok, r, s = pcall(function() return 1, 'x' end)";
        assert_eq!(library_types_of(source, "ok")[0], "boolean");
        assert_eq!(library_types_of(source, "r")[0], "number|string");
        assert_eq!(library_types_of(source, "s")[0], "string?");
        assert_eq!(library_types_of("local ok, r = xpcall(function() return 1 end, function() return true end)", "r")[0], "number|true");
        // From the table's array
        assert_eq!(library_types_of("local a, b = unpack({ 'x', 'y' })", "b")[0], "string?");
        assert_eq!(library_types_of("local t = { 1 } local x = table.remove(t)", "x")[0], "number?");
        assert_eq!(library_types_of("local t = { 1 } local x = tremove(t)", "x")[0], "number?");
        // rawget skips the metatable
        assert_eq!(library_types_of("local t = { a = 1 } local x = rawget(t, 'a')", "x")[0], "number");
        assert_eq!(library_types_of("local B = { b = 1 } local t = setmetatable({}, { __index = B }) local x = rawget(t, 'b')", "x")[0], "unknown");
        let source = "local k, v = next({ a = 1 })";
        assert_eq!(library_types_of(source, "k")[0], "string?");
        assert_eq!(library_types_of(source, "v")[0], "number?");
    }

    #[test]
    fn passed_on_parameters_settle_in_one_go() {
        // Defined deepest first, so each body is walked before its caller:
        // before, each link took another walk, and a cap left the end unknown
        let depth = 60;
        let mut source = format!("local function f{depth}(x) return x end ");
        for i in (1..depth).rev() {
            source += &format!("local function f{i}(x) return f{}(x) end ", i + 1);
        }
        source += "local result = f1('s')";
        assert_eq!(types_of(&source, "result"), ["string"]);
        let analysis = analyse(&source);
        assert!(analysis.walks <= 4, "{} walks", analysis.walks);
        // The same for `...`
        let mut source = format!("local function g{depth}(...) return ... end ");
        for i in (1..depth).rev() {
            source += &format!("local function g{i}(...) return g{}(...) end ", i + 1);
        }
        source += "local result = g1(1)";
        assert_eq!(types_of(&source, "result"), ["number"]);
    }

    #[test]
    fn passed_on_parameters() {
        // Narrowed where it's passed on: only that part goes
        let source = "local function g(s) return s end local function f(v) if type(v) == 'string' then g(v) end end f(1) f('s')";
        assert_eq!(library_types_of(source, "s")[0], "string");
        // Reassigned in a loop after being passed on: what it became goes too
        let source = "local function g(y) return y end local function f(x) for i = 1, 2 do g(x) x = true end end f(1)";
        assert_eq!(types_of(source, "y")[0], "number|true");
        // Passed back to itself: only what's really passed
        let source = "local function f(x, n) if n > 0 then return f(x, n - 1) end return x end local r = f('s', 3)";
        assert_eq!(types_of(source, "x")[0], "string");
        // To a method, with `self` separate
        let source = "local o = {} function o:m(a) return a end local function g(y) return o:m(y) end g(1)";
        assert_eq!(types_of(source, "a")[0], "number");
        let source = "local o = {} function o.m(self, a) return a end local function g(y) return o:m(y) end g(1)";
        assert_eq!(types_of(source, "a")[0], "number");
        // Before more arguments of its own
        let source = "local function g(a, b, ...) local c = ... end local function f(...) g('s', ...) end f(1, true)";
        assert_eq!(types_of(source, "a")[0], "string");
        assert_eq!(types_of(source, "b")[0], "number");
        assert_eq!(types_of(source, "c")[0], "true");
        // From an enclosing function, through a closure
        let source = "local function g(y) return y end local function f(x) local h = function() return g(x) end h() end f('s')";
        assert_eq!(types_of(source, "y")[0], "string");
    }

    /// Everything an analysis shows, to compare two
    fn everything_shown(analysis: &Analysis) -> Vec<String> {
        let mut shown = Vec::new();
        for r in &analysis.references {
            let binding = analysis.binding(r);
            shown.push(format!("{:?} {:?} {} {} {} {}", r.file, r.range, binding.kind, binding.name, r.write, analysis.describe_callee(r.types, r.call_returns.as_ref())));
        }
        for f in &analysis.field_references {
            shown.push(format!("{:?} {:?} {} {} {}", f.file, f.range, analysis.field_path(f), f.write, analysis.describe_callee(f.types, f.call_returns.as_ref())));
        }
        for d in &analysis.diagnostics {
            shown.push(format!("{d:?}"));
        }
        shown.push(format!("{} walks", analysis.walks));
        shown
    }

    /// Replaying a function body whose inputs haven't changed must give
    /// exactly what walking it again would, for every file here
    #[test]
    fn replaying_matches_walking() {
        let parse = |path: &std::path::Path, in_addon: bool| {
            let text = std::fs::read_to_string(path).unwrap();
            SourceFile { root: SyntaxNode::new_root(Generator::new(&text).process_all()), addon: in_addon.then_some(0) }
        };
        let mut programs: Vec<(String, Vec<SourceFile>)> = Vec::new();
        for entry in std::fs::read_dir("tests").unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "lua") {
                programs.push((path.display().to_string(), vec![parse(&path, false)]));
            }
        }
        let toc = std::path::Path::new("tests/addon/TestAddon/TestAddon.toc");
        let files = crate::project::Addon::load(toc).unwrap().files.iter()
            .map(|file| parse(file, true))
            .collect();
        programs.push((toc.display().to_string(), files));
        assert!(programs.len() > 10);
        let mut replays = 0;
        for (name, files) in &programs {
            let replaying = Analysis::analyse_replaying(files, &library(), Arc::default(), true);
            let walking = Analysis::analyse_replaying(files, &library(), Arc::default(), false);
            replays += replaying.replayed;
            assert_eq!(walking.replayed, 0);
            assert_eq!(everything_shown(&replaying), everything_shown(&walking), "{name}");
        }
        // Or this would prove nothing
        assert!(replays > 100, "{replays} replays");
    }

    #[test]
    fn constructor_fields() {
        assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.count", "a"), ["number"]);
        assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.name", "a"), ["string"]);
        assert_eq!(types_of("local t = { f = function() return true end } local a = t.f()", "a"), ["true"]);
        // A string literal key is the field it names; positional entries aren't fields
        assert_eq!(types_of("local t = { 1, ['k'] = 2 } local a = t.k", "a"), ["number"]);
        assert_eq!(types_of("local t = { 1 } local a = t.k", "a"), ["unknown"]);
        assert_eq!(types_of("local t = {} local a = t.missing", "a"), ["unknown"]);
        // Tables never show their identity
        assert_eq!(types_of("local t = { count = 1 }", "t"), ["table"]);
    }

    #[test]
    fn assigned_fields() {
        assert_eq!(types_of("local t = {} t.extra = true local a = t.extra", "a"), ["true"]);
        // Every assignment counts, wherever it is
        assert_eq!(types_of("local t = { n = 1 } if c then t.n = 's' end local a = t.n", "a"), ["number|string"]);
        assert_eq!(types_of("local t = {} local function set() t.x = 1 end local a = t.x", "a"), ["number"]);
        // Including after the read
        assert_eq!(types_of("local t = {} local a = t.x t.x = {}", "a"), ["table"]);
        // Nested tables
        assert_eq!(types_of("local t = { inner = { x = 1 } } local a = t.inner.x", "a"), ["number"]);
        assert_eq!(types_of("local t = {} t.inner = {} t.inner.x = 's' local a = t.inner.x", "a"), ["string"]);
    }

    #[test]
    fn fields_follow_the_table_not_the_variable() {
        assert_eq!(types_of("local t = {} local u = t u.x = 1 local a = t.x", "a"), ["number"]);
        assert_eq!(types_of("local t, u = {}, {} t.x = 1 u.x = 's' local a = t.x", "a"), ["number"]);
        // A function given the table writes to it too
        assert_eq!(types_of("local function init(tbl) tbl.x = 1 end local t = {} init(t) local a = t.x", "a"), ["number"]);
    }

    #[test]
    fn methods() {
        let source = "local Addon = {} function Addon:Name() return 's' end function Addon.Count() return 1 end
            local a = Addon:Name() local b = Addon.Count()";
        assert_eq!(types_of(source, "a"), ["string"]);
        assert_eq!(types_of(source, "b"), ["number"]);
        assert_eq!(types_of("local Addon = {} function Addon:Name() end", "Name"), Vec::<String>::new());
        // `self` is the table, so fields set in one method are seen in another
        let source = "local Addon = {} function Addon:Init() self.count = 0 end function Addon:Get() local c = self.count end";
        assert_eq!(types_of(source, "c"), ["number"]);
        let source = "local Addon = {} function Addon:Init() self.count = 0 end local c = Addon.count";
        assert_eq!(types_of(source, "c"), ["number"]);
        // Nested owners
        let source = "local A = { B = {} } function A.B:M() return 1 end local x = A.B:M()";
        assert_eq!(types_of(source, "x"), ["number"]);
        // Globals made in the file work too
        let source = "MyAddon = {} function MyAddon:M() return {} end local function f() local x = MyAddon:M() end";
        assert_eq!(types_of(source, "x"), ["table"]);
    }

    #[test]
    fn pseudo_identities() {
        // The usual addon namespace, which comes from outside the file
        let source = "local _, ns = ... ns.Version = '1.0' function ns:Get() return 1 end local a, b = ns.Version, ns:Get()";
        assert_eq!(types_of(source, "a"), ["string"]);
        assert_eq!(types_of(source, "b"), ["number"]);
        // But it's still unknown what the variable itself is
        assert_eq!(types_of("local _, ns = ...", "ns"), ["unknown"]);
        // Parameters too
        assert_eq!(types_of("local function f(opts) opts.size = 1 return opts.size end local a = f({})", "a"), ["number"]);
    }

    #[test]
    fn fields_of_unidentified_tables() {
        // Writing through an expression that could be either table keeps the write by name
        let source = "local u = {}; (c and {} or u).x = 1 local a = u.x";
        assert_eq!(types_of(source, "a"), ["number"]);
        // A variable holding either one gets a pseudo identity of its own
        let source = "local t = c and {} or {} t.x = 1 local a = t.x local u = {} local b = u.x";
        assert_eq!(types_of(source, "a"), ["number"]);
        assert_eq!(types_of(source, "b"), ["unknown"]);
        // WoW objects share fields by name
        assert_eq!(object_types_of("local f = CreateFrame('Frame') f.value = 5 local a = f.value", "a"), ["number"]);
        assert_eq!(object_types_of("local f = CreateFrame('Frame') function f:OnEvent() return 1 end local a = f:OnEvent()", "a"), ["number"]);
        // Reading from something unknown stays unknown
        assert_eq!(types_of("local t = {} t.x = 1 local a = (c and {} or t).x", "a"), ["unknown"]);
    }

    /// Files analysed together, in order, as one addon (`in_addon`) or just
    /// sharing globals
    fn analyse_files(sources: &[&str], in_addon: bool) -> Analysis {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: in_addon.then_some(0) })
            .collect();
        Analysis::analyse(&files, &Arc::new(Definitions::default()))
    }

    /// Type at each use of `name` in file `file`
    fn types_in(analysis: &Analysis, file: u32, name: &str) -> Vec<String> {
        analysis.references.iter()
            .filter(|r| r.file == FileId(file) && analysis.binding(r).name.as_ref() == name)
            .map(|r| analysis.describe(r.types))
            .collect()
    }

    #[test]
    fn globals_carry_across_files() {
        let analysis = analyse_files(&["Foo = 1", "local x = Foo"], false);
        assert_eq!(types_in(&analysis, 1, "x"), ["number"]);
        // Files run in order, so a global set by a later file isn't set yet
        let analysis = analyse_files(&["local x = Late", "Late = 's'"], false);
        assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
        // But a function reading it can run later
        let analysis = analyse_files(&["function F() return Late end", "Late = 's' local x = F()"], false);
        assert_eq!(types_in(&analysis, 1, "x"), ["string"]);
    }

    #[test]
    fn functions_across_files() {
        let analysis = analyse_files(&["function Helper() return {} end", "local t = Helper()"], false);
        assert_eq!(types_in(&analysis, 1, "t"), ["table"]);
        let analysis = analyse_files(&["Lib = {} function Lib:Get() return 1 end", "local n = Lib:Get()"], false);
        assert_eq!(types_in(&analysis, 1, "n"), ["number"]);
    }

    #[test]
    fn locals_stay_in_their_file() {
        let analysis = analyse_files(&["local x = 1", "local y = x"], false);
        assert_eq!(types_in(&analysis, 1, "y"), ["unknown"]);
        assert_eq!(analysis.binding(analysis.references.iter().find(|r| r.file == FileId(1) && !r.write).unwrap()).kind, BindingKind::Global);
    }

    #[test]
    fn addon_namespace_is_shared() {
        let analysis = analyse_files(&[
            "local name, ns = ... ns.Version = '1.0' function ns:GetName() return name end",
            "local _, ns = ... function ns.Round(x) return 1 end",
            "local _, ns = ... local v, n, r = ns.Version, ns:GetName(), ns.Round(1)",
        ], true);
        assert_eq!(types_in(&analysis, 0, "name"), ["string", "string"]);
        assert_eq!(types_in(&analysis, 2, "v"), ["string"]);
        assert_eq!(types_in(&analysis, 2, "n"), ["string"]);
        assert_eq!(types_in(&analysis, 2, "r"), ["number"]);
        // A method defined in a later file, called by an earlier one
        let analysis = analyse_files(&[
            "local _, ns = ... function F() return ns:Later() end",
            "local _, ns = ... function ns:Later() return true end local x = F()",
        ], true);
        assert_eq!(types_in(&analysis, 1, "x"), ["true"]);
    }

    #[test]
    fn select_from_varargs() {
        let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
            "select": {"parameters": ["index: number|string", "..."], "returns": ["unknown"], "rest": "unknown"}
        }}"#).unwrap());
        let analyse = |sources: &[&str]| {
            let files: Vec<SourceFile> = sources.iter()
                .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0) })
                .collect();
            Analysis::analyse(&files, &definitions)
        };
        // The usual way to get the namespace without naming the addon
        let analysis = analyse(&["local ns = select(2, ...) ns.x = 1", "local ns = select(2, ...) local y = ns.x"]);
        assert_eq!(types_in(&analysis, 1, "y"), ["number"]);
        let analysis = analyse(&["local name = select(1, ...)", "local n = select('#', ...)", "local a, b = select(2, 1, 's', true)"]);
        assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
        assert_eq!(types_in(&analysis, 1, "n"), ["number"]);
        assert_eq!(types_in(&analysis, 2, "a"), ["string"]);
        assert_eq!(types_in(&analysis, 2, "b"), ["true"]);
        // An index that isn't a literal, or a shadowed `select`, isn't followed
        assert_eq!(types_in(&analyse(&["local i = 2 local ns = select(i, ...)"]), 0, "ns")[0], "unknown");
        assert_eq!(types_in(&analyse(&["local function select() return 1 end local ns = select(2, ...)"]), 0, "ns"), ["number"]);
    }

    #[test]
    fn parameters_from_calls_in_other_files() {
        let analysis = analyse_files(&["function Id(x) return x end", "local v = Id(1)"], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
        assert_eq!(types_in(&analysis, 1, "v"), ["number"]);
        let analysis = analyse_files(&[
            "local _, ns = ... function ns.Id(x) return x end function ns:Add(n) return n end",
            "local _, ns = ... local a, b = ns.Id(1), ns:Add('s')",
        ], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
        assert_eq!(types_in(&analysis, 0, "n"), ["string", "string"]);
        assert_eq!(types_in(&analysis, 1, "a"), ["number"]);
        assert_eq!(types_in(&analysis, 1, "b"), ["string"]);
        // From calls in several files
        let analysis = analyse_files(&["local _, ns = ... function ns.F(x) end", "local _, ns = ... ns.F(1)", "local _, ns = ... ns.F('s')"], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["number|string"]);
    }

    #[test]
    fn methods_called_either_way() {
        // `self` is passed explicitly with `.`, so the arguments line up after it
        let analysis = analyse_files(&["local _, ns = ... function ns:Add(n) end ns.Add(ns, 1)"], true);
        assert_eq!(types_in(&analysis, 0, "n"), ["number"]);
        let analysis = analyse_files(&["local _, ns = ... function ns.Add(t, n) end ns:Add(1)"], true);
        assert_eq!(types_in(&analysis, 0, "n"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "t"), ["table"]);
    }

    #[test]
    fn shared_functions_that_escape() {
        // Handed to something else, which could call it with anything
        let analysis = analyse_files(&["local _, ns = ... function ns.OnEvent(x) end", "local _, ns = ... ns.OnEvent(1) frame:SetScript('OnEvent', ns.OnEvent)"], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
        let analysis = analyse_files(&["function Handler(x) end", "Handler(1) register(Handler)"], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
        // Copied onto something else (a mixin), whose calls aren't known to be it
        let analysis = analyse_files(&["local M = {} function M:Init(x) end M:Init(1)", "local f = Mixin(CreateFrame('Frame'), M) f:Init('s')"], true);
        assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
    }

    fn mixin_definitions() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{"functions": {
            "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
            "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]}
        }}"#).unwrap())
    }

    fn analyse_mixins(sources: &[&str]) -> Analysis {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0) })
            .collect();
        Analysis::analyse(&files, &mixin_definitions())
    }

    #[test]
    fn create_from_mixins() {
        let analysis = analyse_mixins(&[
            "local _, ns = ... ns.Base = {} function ns.Base:Name() return 's' end",
            "local _, ns = ... ns.Bar = CreateFromMixins(ns.Base) function ns.Bar:Size() return 1 end",
            "local _, ns = ... local a, b, c = ns.Bar:Name(), ns.Bar:Size(), ns.Bar",
        ]);
        assert_eq!(types_in(&analysis, 2, "a"), ["string"]);
        assert_eq!(types_in(&analysis, 2, "b"), ["number"]);
        assert_eq!(types_in(&analysis, 2, "c"), ["table"]);
        // Its own fields win, and so do later mixins
        let analysis = analyse_mixins(&["local A = { x = 1 } local B = { x = 's' } local C = CreateFromMixins(A, B) local D = CreateFromMixins(A) D.x = true
            local c, d = C.x, D.x"]);
        assert_eq!(types_in(&analysis, 0, "c"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "d"), ["true"]);
        // Through an alias too
        let analysis = analyse_mixins(&["local Create = CreateFromMixins local A = { x = 1 } local y = Create(A).x"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
    }

    #[test]
    fn mixin_into_objects() {
        let analysis = analyse_mixins(&["local M = {} function M:Init(x) end M:Init(1) local t = Mixin({}, M) t:Init('s')"]);
        // Both calls are known to be M.Init
        assert_eq!(types_in(&analysis, 0, "x"), ["number|string"]);
        let analysis = analyse_mixins(&["local M = { v = 1 } local t = {} Mixin(t, M) local y = t.v"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
    }

    fn analyse_files_with(sources: &[&str], definitions: &Arc<Definitions>) -> Analysis {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0) })
            .collect();
        Analysis::analyse(&files, definitions)
    }

    fn analyse_objects(sources: &[&str]) -> Analysis {
        let definitions = Arc::new(Definitions::from_json(r#"{
            "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}},
            "method_tables": {"FrameAPI": {"functions": {"Show": {}, "GetParent": {"returns": ["Frame"]}}}},
            "values": {"UIParent": "Frame"},
            "functions": {
                "CreateFrame": {"parameters": ["frameType: string", "name: string|nil", "parent: Frame|nil"], "returns": ["Frame"]},
                "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
                "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]}
            }
        }"#).unwrap());
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0) })
            .collect();
        Analysis::analyse(&files, &definitions)
    }

    #[test]
    fn functions_mixing_in_parameters() {
        // Coolinator's InitFrameWithMixin
        let analysis = analyse_objects(&[
            "local _, ns = ... function ns.Init(parent, mixin) local f = CreateFrame('Frame', nil, parent) Mixin(f, mixin) f:OnLoad() return f end",
            "local _, ns = ... ns.A = {} function ns.A:OnLoad() end function ns.A:Count() return 1 end
             ns.B = {} function ns.B:OnLoad() end function ns.B:Name() return 's' end",
            "local _, ns = ... local a, b = ns.Init(UIParent, ns.A), ns.Init(UIParent, ns.B)
             local x, y, z = a:Count(), b:Name(), a:Name()
             local p = a:GetParent() a:Show()",
        ]);
        assert_eq!(types_in(&analysis, 2, "a")[0], "Frame & ns.A");
        assert_eq!(types_in(&analysis, 2, "x"), ["number"]);
        assert_eq!(types_in(&analysis, 2, "y"), ["string"]);
        // `a` was made with A, not B
        assert_eq!(types_in(&analysis, 2, "z"), ["unknown"]);
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
    }

    #[test]
    fn each_call_returns_a_new_object() {
        let analysis = analyse_objects(&["local function Make() local t = { kind = 1 } return t end
            local a, b = Make(), Make() a.v = 1 b.v = 's' local x, y, k = a.v, b.v, a.kind"]);
        assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["string"]);
        // Fields set where it was made still count
        assert_eq!(types_in(&analysis, 0, "k"), ["number"]);
        // Something not made by the function is the same table every call
        let analysis = analyse_objects(&["local shared = {} local function Get() return shared end
            local a, b = Get(), Get() a.v = 1 local y = b.v"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
    }

    #[test]
    fn varargs_outside_addon_files() {
        // Not part of an addon, so nothing is known about `...`
        assert_eq!(types_in(&analyse_files(&["local name, ns = ..."], false), 0, "name"), ["unknown"]);
        // A function's own `...` is its arguments, even in an addon
        assert_eq!(types_in(&analyse_files(&["local function f(...) local a = ... end"], true), 0, "a"), ["unknown"]);
    }

    #[test]
    fn references_are_per_file() {
        let analysis = analyse_files(&["local a = 1", "local b = 's'"], false);
        let first = analysis.reference_at(FileId(0), TextSize::from(6)).unwrap();
        let second = analysis.reference_at(FileId(1), TextSize::from(6)).unwrap();
        assert_eq!(&*analysis.binding(first).name, "a");
        assert_eq!(&*analysis.binding(second).name, "b");
        assert!(analysis.reference_at(FileId(2), TextSize::from(6)).is_none());
    }

    /// Hover text for each use of the field reached by `path`, in source order
    fn field_hovers(analysis: &Analysis, path: &str) -> Vec<String> {
        analysis.field_references.iter()
            .filter(|f| analysis.field_path(f) == path)
            .map(|f| analysis.hover(f.file, f.range.start()).unwrap().0)
            .collect()
    }

    #[test]
    fn field_names_have_types() {
        // The value assigned, and then the field's type wherever it's read
        let source = "local addonTable = {} local hidden = {} addonTable.hiddenTable = hidden local x = addonTable.hiddenTable";
        let analysis = analyse(source);
        assert_eq!(field_hovers(&analysis, "addonTable.hiddenTable"),
            ["field addonTable.hiddenTable: table", "field addonTable.hiddenTable: table"]);
        let analysis = analyse_with("local t = {} local hidden = CreateFrame('Frame') t.hidden = hidden local x = t.hidden", &objects());
        assert_eq!(field_hovers(&analysis, "t.hidden"), ["field t.hidden: Frame", "field t.hidden: Frame"]);
        // Through a string key too
        let analysis = analyse("local t = {} t['v'] = 1 local x = t['v']");
        assert_eq!(field_hovers(&analysis, "t['v']"), ["field t['v']: number", "field t['v']: number"]);
        // A write shows what that write assigned; reads show every assignment
        let analysis = analyse("local t = {} t.v = 1 t.v = 's' local x = t.v");
        assert_eq!(field_hovers(&analysis, "t.v"), ["field t.v: number", "field t.v: string", "field t.v: number|string"]);
    }

    #[test]
    fn field_names_in_every_position() {
        let analysis = analyse("local t = { count = 1, a = { b = {} } } function t.a.b:Get() return 's' end local x = t:Get() local y = t.a.b:Get()");
        assert_eq!(field_hovers(&analysis, "count"), ["field count: number"]);
        assert_eq!(field_hovers(&analysis, "t.a"), ["field t.a: table", "field t.a: table"]);
        assert_eq!(field_hovers(&analysis, "t.a.b:Get"), ["field t.a.b:Get: function() -> string", "field t.a.b:Get: function() -> string"]);
        // `Get` is on `t.a.b`, not `t`
        assert_eq!(field_hovers(&analysis, "t:Get"), ["field t:Get: unknown"]);
        // Whitespace and comments aren't part of the path
        let analysis = analyse("local t = {} t . --[[c]] x = 1");
        assert_eq!(field_hovers(&analysis, "t.x"), ["field t.x: number"]);
    }

    #[test]
    fn field_names_in_loops_are_recorded_once() {
        let analysis = analyse("local t = {} while c do t.x = 1 local y = t.x end");
        assert_eq!(field_hovers(&analysis, "t.x").len(), 2);
    }

    fn widgets() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{
            "types": {
                "Region": {"base": "table", "method_tables": ["RegionAPI"]},
                "Frame": {"base": "table", "inherits": ["Region"], "method_tables": ["FrameAPI"]},
                "Button": {"base": "table", "inherits": ["Frame"], "method_tables": ["ButtonAPI"]},
                "Texture": {"base": "table", "inherits": ["Region"], "method_tables": []},
                "FontString": {"base": "table", "inherits": ["Region"], "method_tables": ["FontStringAPI"]},
                "ObjectPool": {"base": "table", "method_tables": ["ObjectPoolAPI"]},
                "ItemKey": {"base": "table"}
            },
            "method_tables": {
                "RegionAPI": {"functions": {"SetPoint": {}, "Show": {}, "GetParent": {"returns": ["Region"]}, "SetParent": {"parameters": ["parent: Frame|nil"]}}},
                "FrameAPI": {"functions": {
                    "CreateTexture": {"returns": ["Texture"]},
                    "CreateFontString": {"parameters": ["name: string|nil", "templateName: string|nil"], "returns": ["Texture"]},
                    "GetChild": {"returns": ["Frame|nil"]}
                }},
                "ButtonAPI": {"functions": {"Click": {}, "GetText": {"returns": ["string"]}}},
                "FontStringAPI": {"functions": {"SetText": {}}},
                "ObjectPoolAPI": {"functions": {"Acquire": {"returns": ["unknown", "boolean"]}, "Release": {"parameters": ["object"]},
                    "EnumerateActive": {"returns": ["function", "table"]}, "GetNextActive": {"returns": ["unknown"]}}}
            },
            "elements": {"Frame": "Frame", "Button": "Button", "Texture": "Texture", "FontString": "FontString", "Font": "Font",
                "Odd": {"type": "Frame", "extended": true}},
            "mixins": {
                "BaseMixin": {"functions": {"Base": {}, "Value": {"returns": ["string"]}}},
                "ButtonMixin": {"functions": {"Init": {}, "Value": {"returns": ["number"]}}},
                "OpenMixin": {"open": true, "functions": {"Known": {}}}
            },
            "templates": {
                "BaseT": {"type": "Button", "mixins": ["BaseMixin"]},
                "ButtonT": {"type": "Button", "inherits": ["BaseT"], "mixins": ["ButtonMixin"], "values": {"flag": "boolean"},
                    "children": {"Label": {"type": "FontString", "inherits": ["GameFont"]}, "Icon": {"type": "Texture"}},
                    "arrays": {"Tabs": [{"type": "Button"}]}},
                "OpenT": {"type": "Frame", "mixins": ["OpenMixin"]},
                "MissingT": {"type": "Frame", "mixins": ["Nowhere"]},
                "TextureT": {"type": "Texture", "values": {"v": "number"}}
            },
            "frames": {
                "GameFont": {"type": "Font"},
                "Plain": {"type": "Frame", "children": {"Inner": {"type": "Button", "mixins": ["ButtonMixin"]}}},
                "Opened": {"type": "Frame", "inherits": ["OpenT"]},
                "UIParent": {"type": "Button"}
            },
            "values": {"UIParent": "Frame"},
            "tables": {
                "RegistryMixin": {"functions": {"Register": {}, "Count": {"returns": ["number"]}}},
                "OpenRegistryMixin": {"open": true, "functions": {"Register": {}}}
            },
            "functions": {
                "CreateFrame": {"parameters": ["frameType: string", "name: string|nil", "parent: Frame|nil", "template: string|nil"], "returns": ["Frame"]},
                "GetItemKey": {"returns": ["ItemKey"]},
                "GetFrame": {"returns": ["Frame"]},
                "Anchor": {"parameters": ["region: Region"]},
                "Parent": {"parameters": ["frame: Frame"]},
                "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
                "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
                "CreateFramePool": {"parameters": ["frameType: string|nil", "parent: Frame|nil", "frameTemplate: string|nil", "resetFunc: function|nil", "forbidden: boolean|nil", "frameInitFunc: function|nil"], "returns": ["ObjectPool"]},
                "CreateFontStringPool": {"parameters": ["parent: Frame|nil", "layer: string|nil", "subLayer: number|nil", "fontStringTemplate: string|nil", "resetFunc: function|nil"], "returns": ["ObjectPool"]},
                "CreateObjectPool": {"parameters": ["createFunc: function|nil", "resetFunc: function|nil"], "returns": ["ObjectPool"]}
            }
        }"#).unwrap())
    }

    fn widget_types_of(source: &str, name: &str) -> Vec<String> {
        describe_uses(&analyse_with(source, &widgets()), name)
    }

    fn warnings(source: &str) -> Vec<String> {
        analyse_with(source, &widgets()).diagnostics.iter().map(|d| d.message.clone()).collect()
    }

    #[test]
    fn create_frame_types() {
        assert_eq!(widget_types_of("local f = CreateFrame('Button')", "f"), ["Button"]);
        assert_eq!(widget_types_of("local f = CreateFrame(\"button\", 'Name')", "f"), ["Button"]);
        // Not a kind of frame, or not a literal
        assert_eq!(widget_types_of("local f = CreateFrame('Texture')", "f"), ["Frame"]);
        assert_eq!(widget_types_of("local f = CreateFrame(kind)", "f"), ["Frame"]);
        // Templates still give the type
        assert_eq!(widget_types_of("local f = CreateFrame('Button', nil, nil, 'Template')", "f"), ["Button"]);
    }

    #[test]
    fn methods_of_wow_types() {
        assert_eq!(widget_types_of("local f = CreateFrame('Button') local t = f:CreateTexture()", "t"), ["Texture"]);
        assert_eq!(widget_types_of("local f = CreateFrame('Button') local t = f:GetText()", "t"), ["string"]);
        assert_eq!(widget_types_of("local f = CreateFrame('Frame') local c = f:GetChild()", "c"), ["Frame?"]);
        // Methods are fields too
        assert_eq!(widget_types_of("local f = CreateFrame('Frame') local m = f.Show", "m"), ["function()"]);
    }

    #[test]
    fn missing_methods() {
        assert_eq!(warnings("local f = CreateFrame('Frame') f:SetPiont()"), ["Frame has no method `SetPiont`"]);
        assert_eq!(warnings("local f = CreateFrame('Frame') f:Click()"), ["Frame has no method `Click`"]);
        assert_eq!(warnings("local f = CreateFrame('Frame') local c = f:GetChild() c:Nope()"), ["Frame has no method `Nope`"]);
        assert_eq!(warnings("local t = CreateFrame('Frame'):CreateTexture() t:Nope()"), ["Texture has no method `Nope`"]);
        // Known, inherited, or added by the addon
        assert!(warnings("local f = CreateFrame('Button') f:SetPoint() f:Click() f:CreateTexture()").is_empty());
        assert!(warnings("local f = CreateFrame('Frame') function f:OnEvent() end f:OnEvent()").is_empty());
        assert!(warnings("local M = {} function M:Extra() end local f = CreateFrame('Frame') Mixin(f, M) f:Extra()").is_empty());
        assert!(warnings("local f = CreateFrame('Frame') f:Later() function SomeTable:Later() end").is_empty());
    }

    #[test]
    fn requirements_include_subtypes() {
        // Required to be a Region, which a Frame is, so it could have Frame methods
        let source = "local function Row(frame) Anchor(frame) frame:CreateTexture() frame:Anything() end";
        assert!(warnings(source).is_empty());
        assert_eq!(widget_types_of(source, "frame")[0], "Region");
        // But a documented return is exactly what it says
        assert_eq!(warnings("local p = GetFrame() p:Click()"), ["Frame has no method `Click`"]);
        assert_eq!(warnings("local t = GetFrame():CreateTexture() t:Nope()"), ["Texture has no method `Nope`"]);
        // Two requirements that agree give the narrower
        let source = "local function Row(frame) Anchor(frame) Parent(frame) end";
        assert_eq!(widget_types_of(source, "frame")[0], "Frame");
    }

    #[test]
    fn parents_from_creation() {
        assert_eq!(widget_types_of("local p = CreateFrame('Frame', nil, UIParent):GetParent()", "p"), ["Frame"]);
        let source = "local b = CreateFrame('Button') local t = b:CreateTexture() local p = t:GetParent()";
        assert_eq!(widget_types_of(source, "p"), ["Button"]);
        // So methods of the real parent are known
        assert!(warnings("local b = CreateFrame('Button') local t = b:CreateTexture() t:GetParent():Click()").is_empty());
        assert_eq!(warnings("local f = CreateFrame('Frame') local t = f:CreateTexture() t:GetParent():Click()"), ["Frame has no method `Click`"]);
        // Made without one
        assert_eq!(widget_types_of("local p = CreateFrame('Frame'):GetParent()", "p"), ["nil"]);
        assert_eq!(widget_types_of("local p = CreateFrame('Frame', nil, nil):GetParent()", "p"), ["nil"]);
    }

    #[test]
    fn parents_from_set_parent() {
        // Anywhere in the files, as it could run before GetParent does
        let source = "local f = CreateFrame('Frame') local p = f:GetParent() f:SetParent(UIParent)";
        assert_eq!(widget_types_of(source, "p"), ["Frame?"]);
        // A Button or UIParent: either, with the methods both have
        let source = "local b = CreateFrame('Button', nil, UIParent) b:SetParent(CreateFrame('Button')) local p = b:GetParent()";
        assert_eq!(widget_types_of(source, "p"), ["Frame|Button"]);
        assert!(warnings(&format!("{source} p:Show()")).is_empty());
        assert_eq!(warnings(&format!("{source} p:Click()")), ["This may be a Frame, which has no method `Click`"]);
    }

    #[test]
    fn several_objects_kept_apart() {
        let made = "local holder = CreateFrame('Frame') local other = CreateFrame('Button')
            local a = CreateFrame('Button', nil, holder) local b = CreateFrame('Button', nil, other, 'ButtonT') local x = c and a or b ";
        // Each with its own parent
        assert_eq!(widget_types_of(&format!("{made} local p = x:GetParent()"), "p"), ["Button|Frame"]);
        // A field set on it may be on either; one only one has isn't known
        assert_eq!(widget_types_of(&format!("{made} x.label = 's' local l = a.label"), "l"), ["string"]);
        assert_eq!(widget_types_of(&format!("{made} a.only = 1 local o = x.only"), "o")[0], "unknown");
        assert_eq!(widget_types_of(&format!("{made} a.n = 1 b.n = 2 local n = x.n"), "n"), ["number"]);
        // Mixed into whichever it is
        let mixed = format!("{made} local M = {{}} function M:Go() return 1 end Mixin(x, M) local g = a:Go()");
        assert_eq!(widget_types_of(&mixed, "g"), ["number"]);
        // A parent set on whichever it is
        assert_eq!(widget_types_of(&format!("{made} x:SetParent(UIParent) local p = a:GetParent()"), "p"), ["Frame"]);
        // Plain objects of one type are still kept apart, but shown once
        assert_eq!(widget_types_of("local f = c and CreateFrame('Frame') or CreateFrame('Frame')", "f"), ["Frame"]);
        // Past the most kept apart: the type they all are, which may have more
        let many: String = (0..9).map(|i| format!("if c{i} then f = CreateFrame('Button', nil, nil, 'T{i}') end ")).collect();
        let source = format!("local f = CreateFrame('Button') {many} local g = f g:Nope()");
        assert_eq!(widget_types_of(&source, "g")[0], "Button");
        assert!(warnings(&source).is_empty());
    }

    #[test]
    fn mixins_the_definitions_give() {
        // Blizzard's Lua mixins, as tables in the definitions
        let made = "local f = CreateFrame('Frame') Mixin(f, RegistryMixin) local n = f:Count() ";
        assert_eq!(widget_types_of(made, "f")[0], "Frame & RegistryMixin");
        assert_eq!(widget_types_of(made, "n"), ["number"]);
        assert!(warnings(&format!("{made} f:Register() f:Show()")).is_empty());
        assert_eq!(warnings(&format!("{made} f:Nope()")), ["Frame has no method `Nope`"]);
        let table = "local t = CreateFromMixins(RegistryMixin) ";
        assert!(warnings(&format!("{table} t:Register()")).is_empty());
        assert_eq!(warnings(&format!("{table} t:Nope()")), ["No function `Nope` is defined for this table"]);
        // One that may have more, under computed keys
        assert!(warnings("local f = CreateFrame('Frame') Mixin(f, OpenRegistryMixin) f:Nope()").is_empty());
        assert!(warnings("local t = CreateFromMixins(OpenRegistryMixin) t:Nope()").is_empty());
    }

    #[test]
    fn objects_not_made_in_the_files() {
        // The documented parent type
        assert_eq!(widget_types_of("local p = UIParent:GetParent()", "p"), ["Region"]);
        assert_eq!(widget_types_of("local f = GetFrame() local p = f:GetParent()", "p"), ["Region"]);
    }

    #[test]
    fn objects_have_their_own_fields() {
        let source = "local a = CreateFrame('Frame') local b = CreateFrame('Frame') a.x = 1 b.x = 's' local y = a.x";
        assert_eq!(widget_types_of(source, "y"), ["number"]);
        let source = "local f = CreateFrame('Frame') function f:OnEvent() return true end local y = f:OnEvent()";
        assert_eq!(widget_types_of(source, "y"), ["true"]);
        // Still shown as just their type
        assert_eq!(widget_types_of("local f = CreateFrame('Frame')", "f"), ["Frame"]);
    }

    #[test]
    fn mixins_are_what_they_are_mixed_into() {
        // `self` in a mixin's methods is the frame it's mixed into, with the mixin's fields
        let source = "local M = {} function M:OnLoad() local s, t, n = self, self:GetText(), self:Count() end function M:Count() return 1 end
            local b = CreateFrame('Button') Mixin(b, M)";
        assert_eq!(widget_types_of(source, "s"), ["Button & M"]);
        assert_eq!(widget_types_of(source, "t"), ["string"]);
        assert_eq!(widget_types_of(source, "n"), ["number"]);
        assert!(warnings(source).is_empty(), "{:?}", warnings(source));
        // A missing method is still reported
        assert_eq!(warnings("local M = {} function M:OnLoad() self:Nope() end Mixin(CreateFrame('Button'), M)"), ["Button has no method `Nope`"]);
        // Mixed into a Button and a Frame, so it's only known to be a Frame
        assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end Mixin(CreateFrame('Button'), M) Mixin(CreateFrame('Frame'), M)", "s"), ["Frame & M"]);
        // Through a table made from it
        assert_eq!(widget_types_of("local Base = {} function Base:OnLoad() local s = self end local M = CreateFromMixins(Base)
            Mixin(CreateFrame('Button'), M)", "s"), ["Button & Base"]);
        // Mixed into a plain table too, so it isn't only frames
        assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end Mixin(CreateFrame('Button'), M) Mixin({}, M)", "s"), ["table"]);
        // Never mixed into anything: still a plain table
        assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end", "s"), ["table"]);
        // A template can add methods, so they aren't reported
        assert!(warnings("local M = {} function M:OnLoad() self:Nope() end Mixin(CreateFrame('Button', nil, nil, 'T'), M)").is_empty());
    }

    #[test]
    fn mixins_shown_on_frames() {
        let source = "local A, B = {}, {} local f = CreateFrame('Button') Mixin(f, A, B) local g = f
            local h = Mixin(CreateFrame('Frame'), CreateFromMixins(A, B)) local plain = CreateFrame('Frame')
            local maybe = c and f or nil";
        assert_eq!(widget_types_of(source, "g"), ["Button & A & B"]);
        // An inline table of mixins shows what it's made from
        assert_eq!(widget_types_of(source, "h"), ["Frame & A & B"]);
        assert_eq!(widget_types_of(source, "plain"), ["Frame"]);
        assert_eq!(widget_types_of(source, "maybe"), ["(Button & A & B)?"]);
    }

    #[test]
    fn pools_give_their_objects() {
        let source = "local M = {} function M:Count() return 1 end
            local pool = CreateFramePool('Button', UIParent, nil, function(p, frame) local r = frame end, false, function(frame) Mixin(frame, M) frame.label = 's' end)
            local got, new = pool:Acquire()
            local n, label, t = got:Count(), got.label, got:GetText()
            for active in pool:EnumerateActive() do local e = active end
            local next = pool:GetNextActive()
            local p = pool";
        assert_eq!(widget_types_of(source, "got")[0], "Button & M");
        assert_eq!(widget_types_of(source, "new"), ["boolean"]);
        // The init function's mixins and fields, and the type's own methods
        assert_eq!(widget_types_of(source, "n"), ["number"]);
        assert_eq!(widget_types_of(source, "label"), ["string"]);
        assert_eq!(widget_types_of(source, "t"), ["string"]);
        // The reset function is given the pool, then the object
        assert_eq!(widget_types_of(source, "r"), ["Button & M"]);
        assert_eq!(widget_types_of(source, "e"), ["Button & M"]);
        assert_eq!(widget_types_of(source, "next"), ["(Button & M)?"]);
        // The reset function's pool parameter, then the pool itself
        assert_eq!(widget_types_of(source, "p"), ["ObjectPool<Button & M>", "ObjectPool<Button & M>"]);
        // `self` in the mixin is the pool's frames
        assert_eq!(widget_types_of("local M = {} function M:Init() local s = self end
            local pool = CreateFramePool('Button', nil, nil, nil, false, function(f) Mixin(f, M) end)", "s"), ["Button & M"]);
        assert!(warnings(source).is_empty(), "{:?}", warnings(source));
    }

    #[test]
    fn pool_callbacks_held_in_variables() {
        // Named init and reset functions count as called by the pool too
        let source = "local function Reset(pool, fs) local r = fs end
            local pool = CreateFontStringPool(UIParent, 'OVERLAY', 0, nil, Reset) local s = pool:Acquire()";
        assert_eq!(widget_types_of(source, "r"), ["FontString"]);
        assert_eq!(widget_types_of(source, "s"), ["FontString"]);
        // Object pools hold what their create function returns
        let source = "local M = { v = 1 } local pool = CreateObjectPool(function(p) local f = CreateFrame('Frame') Mixin(f, M) return f end)
            local o = pool:Acquire() local v = o.v";
        assert_eq!(widget_types_of(source, "o")[0], "Frame & M");
        assert_eq!(widget_types_of(source, "v"), ["number"]);
        // A template can add members
        assert!(warnings("local pool = CreateFramePool('Button', nil, 'T') pool:Acquire():Anything()").is_empty());
        assert_eq!(warnings("local pool = CreateFramePool('Button') pool:Acquire():Anything()"), ["Button has no method `Anything`"]);
    }

    #[test]
    fn pools_made_by_functions() {
        // Coolinator's GeneratePool: the init function mixes in a parameter of
        // the function making the pool, so each call's pool has its own mixin
        let analysis = analyse_files_with(&[
            "local _, ns = ... function ns.GeneratePool(mixin, template)
                return CreateFramePool('Frame', UIParent, template or 'T', function(_, frame) frame:Hide() end, false, function(frame)
                    Mixin(frame, mixin) frame:OnLoad() end)
            end",
            "local _, ns = ... ns.A = {} function ns.A:OnLoad() local sa = self end function ns.A:Count() return 1 end
             ns.B = {} function ns.B:OnLoad() end function ns.B:Name() return 's' end",
            "local _, ns = ... local pools = { a = ns.GeneratePool(ns.A), b = ns.GeneratePool(ns.B) }
             local pa, pb = pools.a, pools.b
             local a, b = pools.a:Acquire(), pools.b:Acquire()
             local x, y = a:Count(), b:Name()",
        ], &widgets());
        assert_eq!(types_in(&analysis, 2, "pa"), ["ObjectPool<Frame & ns.A>"]);
        assert_eq!(types_in(&analysis, 2, "pb"), ["ObjectPool<Frame & ns.B>"]);
        assert_eq!(types_in(&analysis, 2, "a"), ["Frame & ns.A", "Frame & ns.A"]);
        assert_eq!(types_in(&analysis, 2, "b"), ["Frame & ns.B", "Frame & ns.B"]);
        assert_eq!(types_in(&analysis, 2, "x"), ["number"]);
        assert_eq!(types_in(&analysis, 2, "y"), ["string"]);
        assert_eq!(types_in(&analysis, 1, "sa"), ["Frame & ns.A"]);
    }

    #[test]
    fn hovering_calls_shows_what_they_return() {
        let source = "local M = {} local pool = CreateFramePool('Button', nil, nil, nil, false, function(f) Mixin(f, M) end)
            local got = pool:Acquire() local b = CreateFrame('Button')";
        let analysis = analyse_with(source, &widgets());
        let hover = |text: &str| analysis.hover(FileId(0), TextSize::new(source.find(text).unwrap() as u32)).unwrap().0;
        assert_eq!(hover("Acquire"), "field pool:Acquire: function() -> Button & M, boolean");
        assert_eq!(hover("CreateFrame('Button')"), "global CreateFrame: function(frameType: string, name: string?, parent: Frame?, template: string?) -> Button");
    }

    #[test]
    fn own_fields_win_over_unknown_tables() {
        // Another table of unknown identity using the same field name doesn't
        // blur what this table's field holds
        let source = "local t = { cooldown = CreateFramePool('Frame') }
            local function f(entry) entry.texts.cooldown = { scale = 1 } end
            local p = t.cooldown local o = {} local v = o.cooldown";
        assert_eq!(widget_types_of(source, "p"), ["ObjectPool<Frame>"]);
        // With no field of its own, it could be the unknown table
        assert_eq!(widget_types_of(source, "v"), ["table"]);
    }

    #[test]
    fn mixins_given_to_functions_making_frames() {
        // Coolinator's InitFrameWithMixin, with the frame made in another file
        let analysis = analyse_objects(&[
            "local _, ns = ... function ns.Init(parent, mixin) local f = CreateFrame('Frame', nil, parent) Mixin(f, mixin) return f end",
            "local _, ns = ... ns.A = {} function ns.A:OnLoad() local s = self self:Show() end",
            "local _, ns = ... local a = ns.Init(UIParent, ns.A)",
        ]);
        assert_eq!(types_in(&analysis, 1, "s"), ["Frame & ns.A"]);
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
    }

    #[test]
    fn templates_on_created_children() {
        assert!(warnings("local t = CreateFrame('Frame'):CreateFontString(nil, 'Template') t:Anything()").is_empty());
        assert_eq!(warnings("local t = CreateFrame('Frame'):CreateFontString(nil, nil) t:Anything()"), ["Texture has no method `Anything`"]);
    }

    #[test]
    fn frames_made_from_templates() {
        let made = "local f = CreateFrame('Button', nil, nil, 'ButtonT') ";
        assert_eq!(widget_types_of(made, "f"), ["ButtonT"]);
        // Its own methods, its mixins', those of what it inherits, and its children
        assert!(warnings(&format!("{made} f:Init() f:Base() f:Click() f.Label:SetText() f.Icon:Show()")).is_empty());
        assert_eq!(warnings(&format!("{made} f:Nope()")), ["ButtonT has no method `Nope`"]);
        // Its own mixin's method wins over what it inherits
        assert_eq!(widget_types_of(&format!("{made} local v = f:Value()"), "v"), ["number"]);
        assert_eq!(widget_types_of(&format!("{made} local v = f.flag"), "v"), ["boolean"]);
        assert_eq!(widget_types_of(&format!("{made} local v = f.Tabs"), "v"), ["table"]);
        // A font string inheriting a font only takes its settings
        assert_eq!(widget_types_of(&format!("{made} local l = f.Label"), "l"), ["FontString"]);
        assert_eq!(warnings(&format!("{made} f.Label:Nope()")), ["FontString has no method `Nope`"]);
        // From a pool, or made by a frame
        assert_eq!(widget_types_of("local p = CreateFramePool('Button', nil, 'ButtonT') local f = p:Acquire()", "f"), ["ButtonT"]);
        assert_eq!(widget_types_of("local t = UIParent:CreateFontString(nil, 'TextureT') local v = t.v", "v"), ["number"]);
        // Either a frame made from it or a plain one: the methods both have
        let either = "local f = c and CreateFrame('Button', nil, nil, 'ButtonT') or CreateFrame('Button') ";
        assert_eq!(widget_types_of(either, "f"), ["Button|ButtonT"]);
        assert!(warnings(&format!("{either} f:Click() f:Show()")).is_empty());
        assert_eq!(warnings(&format!("{either} f:Init()")), ["This may be a Button, which has no method `Init`"]);
        assert_eq!(warnings(&format!("{either} f:Nope()")), ["Neither Button nor ButtonT has a method `Nope`"]);
    }

    #[test]
    fn templates_that_cant_be_used() {
        // For another kind of frame, several at once, or not known: the kind
        // of frame made, which the template can add methods to
        assert_eq!(widget_types_of("local f = CreateFrame('Frame', nil, nil, 'ButtonT')", "f"), ["Frame"]);
        assert!(warnings("CreateFrame('Frame', nil, nil, 'ButtonT'):Nope()").is_empty());
        assert!(warnings("CreateFrame('Button', nil, nil, 'ButtonT, BaseT'):Nope()").is_empty());
        assert!(warnings("CreateFrame('Button', nil, nil, name):Nope()").is_empty());
        // With mixins whose methods aren't all known
        assert!(warnings("CreateFrame('Frame', nil, nil, 'OpenT'):Nope()").is_empty());
        assert!(warnings("CreateFrame('Frame', nil, nil, 'MissingT'):Nope()").is_empty());
    }

    #[test]
    fn named_frames() {
        assert_eq!(widget_types_of("local f = Plain", "f"), ["Frame"]);
        assert_eq!(warnings("Plain.Inner:Init() Plain.Inner:Click() Plain.Inner:Nope()"), ["Button has no method `Nope`"]);
        assert!(warnings("Opened:Nope()").is_empty());
        // One the definitions give otherwise keeps that type
        assert!(warnings("UIParent:Click()").len() == 1);
    }

    /// The widgets' definitions with the templates and frames of XML file `xml`
    fn with_xml(xml: &str) -> Arc<Definitions> {
        let definitions = widgets();
        let document = crate::xml::parse(xml).unwrap();
        let objects = crate::xml::objects(&document, &|tag: &str| definitions.element(tag).is_some());
        Arc::new(definitions.with_xml(&objects))
    }

    const ADDON_XML: &str = r#"<Ui>
        <Button name="AddonT" virtual="true" inherits="ButtonT" mixin="AddonMixin">
            <KeyValues><KeyValue key="size" value="2" type="number"/></KeyValues>
            <Layers><Layer><Texture parentKey="Glow"/></Layer></Layers>
        </Button>
        <Frame name="AddonFrame" mixin="FrameMixin">
            <Frames><Button name="$parentClose" parentKey="Close" inherits="AddonT"/></Frames>
        </Frame>
        <Frame name="UnknownT" virtual="true" mixin="NowhereMixin"/>
        <Frame name="LoneT" virtual="true" mixin="LoneMixin"/>
        <Frame name="Placed" parent="UIParent"><Frames><Button parentKey="Inner" mixin="InnerMixin"/></Frames></Frame>
        <Odd name="OddT" virtual="true"/>
    </Ui>"#;

    #[test]
    fn addon_templates() {
        let definitions = with_xml(ADDON_XML);
        let warnings = |source: &str| -> Vec<String> { analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect() };
        let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
        let mixin = "AddonMixin = {count = 1} function AddonMixin:Extra() end ";
        // Made from one: its XML, what it inherits, and its Lua mixin's fields
        let made = format!("{mixin} local f = CreateFrame('Button', nil, nil, 'AddonT') local n, s = f.count, f.size ");
        assert_eq!(types_of(&made, "f")[0], "AddonT & AddonMixin");
        assert_eq!(types_of(&made, "n")[0], "number");
        assert_eq!(types_of(&made, "s")[0], "number");
        assert!(warnings(&format!("{made} f:Extra() f:Init() f:Click() f.Glow:Show()")).is_empty());
        assert_eq!(warnings(&format!("{made} f:Nope()")), ["AddonT has no method `Nope`"]);
        // In the mixin's methods, `self` is what XML mixes it into
        let own = format!("{mixin} function AddonMixin:OnLoad() local s = self self:Init() self:Extra() self.Glow:Show() end");
        assert_eq!(types_of(&own, "s")[0], "AddonT & AddonMixin");
        assert!(warnings(&own).is_empty());
        assert_eq!(warnings(&format!("{mixin} function AddonMixin:OnLoad() self:Nope() end")), ["AddonT has no method `Nope`"]);
        // A named frame, and its children by key and name
        let frame = format!("{mixin} FrameMixin = {{}} function FrameMixin:OnLoad() self.Close:Extra() end local a, c = AddonFrame, AddonFrameClose");
        assert_eq!(types_of(&frame, "a")[0], "Frame");
        // Of its own type, as it's an object of its own, shown as its kind
        assert_eq!(types_of(&frame, "c")[0], "Button");
        assert!(warnings(&format!("{frame} AddonFrame.Close:Extra() c:Click()")).is_empty());
        // Without its mixin's table, its methods aren't known
        assert!(warnings("local f = CreateFrame('Button', nil, nil, 'AddonT') f:Nope()").is_empty());
        assert!(warnings("CreateFrame('Frame', nil, nil, 'UnknownT'):Nope()").is_empty());
        // A kind of frame wow_ls doesn't know may have more methods
        assert!(warnings("CreateFrame('Frame', nil, nil, 'OddT'):Nope()").is_empty());
    }

    #[test]
    fn parents_of_addon_mixins() {
        let definitions = with_xml(ADDON_XML);
        let warnings = |source: &str| -> Vec<String> { analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect() };
        let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
        // What its frames are made in: by `CreateFrame` with its template, and
        // inside other objects in XML
        let lone = "LoneMixin = {} function LoneMixin:OnLoad() local p = self:GetParent() end ";
        assert_eq!(types_of(&format!("{lone} CreateFrame('Frame', nil, CreateFrame('Button'), 'LoneT')"), "p")[0], "Button");
        // That Button, and `AddonFrame`, whose `Close` inherits the template
        let made = "AddonMixin = {} function AddonMixin:OnLoad() local p = self:GetParent() end ";
        assert_eq!(types_of(&format!("{made} CreateFrame('Button', nil, CreateFrame('Button'), 'AddonT')"), "p")[0], "Frame|Button");
        let inner = "InnerMixin = {} function InnerMixin:OnLoad() local p = self:GetParent() end";
        assert_eq!(types_of(inner, "p")[0], "Frame");
        assert!(warnings(&format!("{inner} function InnerMixin:Go() self:GetParent():Show() end")).is_empty());
        // Its frames may be made where the analysis can't see, so the parent
        // isn't one to report missing methods by
        assert!(warnings(&format!("{made} CreateFrame('Button', nil, CreateFrame('Button'), 'AddonT') function AddonMixin:Go() self:GetParent():Nope() end")).is_empty());
        // Made nowhere it can see: the documented type
        let lone = "LoneMixin = {} function LoneMixin:OnLoad() local p = self:GetParent() p:Click() end";
        assert_eq!(types_of(lone, "p")[0], "Region");
        assert!(warnings(lone).is_empty());
    }

    #[test]
    fn missing_methods_only_when_sure() {
        // Made from a template, which can add methods
        assert!(warnings("local f = CreateFrame('Frame', nil, nil, 'BackdropTemplate') f:SetBackdrop()").is_empty());
        // Not one known WoW type
        assert!(warnings("local f = c and CreateFrame('Frame') or 's' f:Nope()").is_empty());
        assert!(warnings("local f = c and CreateFrame('Frame') or {} f:Nope()").is_empty());
        assert!(warnings("unknown:Nope()").is_empty());
        // A type without known methods, like a data structure
        assert!(warnings("local k = GetItemKey() k:Nope()").is_empty());
        // Recorded once, even in loops
        assert_eq!(warnings("local f = CreateFrame('Frame') while c do f:Nope() end").len(), 1);
    }

    #[test]
    fn undefined_table_methods() {
        let warnings = |source: &str| -> Vec<String> {
            analyse_with(source, &library()).diagnostics.iter().map(|d| d.message.clone()).collect()
        };
        let missing = |name: &str| vec![format!("No function `{name}` is defined for this table")];
        assert_eq!(warnings("local t = {} t:Tree()"), missing("Tree"));
        assert_eq!(warnings("local t = {} function t:Grow() self:Tree() end t:Grow()"), missing("Tree"));
        assert_eq!(warnings("local t = {} while c do t:Tree() end"), missing("Tree"));
        // A parameter given that table at every call
        assert_eq!(warnings("local function f(t) t:Tree() end f({})"), missing("Tree"));
        // Through `__index` to a table that doesn't have it either
        assert_eq!(warnings("local C = {} C.__index = C function C:Grow() end local o = setmetatable({}, C) o:Grow() o:Tree()"), missing("Tree"));
        // Defined on this table, later on, or on any other table
        assert!(warnings("local t = {} t:Tree() function t:Tree() end").is_empty());
        assert!(warnings("local t = {} t:Tree() local u = {} u.Tree = function() end").is_empty());
        assert!(warnings("local t = {} t:Tree() function SomeGlobal:Tree() end").is_empty());
        // A field that isn't a function is another problem
        assert!(warnings("local t = { Tree = 1 } t:Tree()").is_empty());
        // A WoW object's method, so it may be a mixin for one
        assert!(warnings("local M = {} function M:OnLoad() self:SetScript() end").is_empty());
        // Somewhere the analysis can't see into
        assert!(warnings("local t = {} Mixin(t, UnknownMixin) t:Tree()").is_empty());
        assert!(warnings("local t = setmetatable({}, { __index = function() end }) t:Tree()").is_empty());
        assert!(warnings("local t = setmetatable({}, { __index = Unknown }) t:Tree()").is_empty());
        assert!(warnings("local t = setmetatable({}, Unknown) t:Tree()").is_empty());
        assert!(warnings("local t = {} t[name] = function() end t:Tree()").is_empty());
        // Tables the analysis can't pin down: parameters, and what calls it can't see into give
        assert!(warnings("function F(t) t:Tree() end").is_empty());
        assert!(warnings("local t = c and {} or {} t:Tree()").is_empty());
        assert!(warnings("local _, ns = ... local t = ns.Make() t:Tree()").is_empty());
    }

    #[test]
    fn reference_lookup() {
        let source = "local x = 5\nlocal y = x";
        let file = analyse(source);
        let r = file.reference_at(FileId(0), TextSize::from(22)).expect("reference at x");
        assert_eq!(&*file.binding(r).name, "x");
        assert!(!r.write);
        assert!(file.reference_at(FileId(0), TextSize::from(3)).is_none());
    }

    /// Go to definition from the `@` in one of `sources` (analysed together
    /// as an addon), as the file and the text from each site to the end of
    /// its line
    fn definitions_at(sources: &[&str]) -> Vec<(u32, String)> {
        let (file, offset) = sources.iter().enumerate()
            .find_map(|(i, s)| s.find('@').map(|at| (i, at)))
            .expect("a cursor");
        let sources: Vec<String> = sources.iter().map(|s| s.replace('@', "")).collect();
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0) })
            .collect();
        let analysis = Analysis::analyse(&files, &library());
        analysis.definition(FileId(file as u32), TextSize::from(offset as u32)).iter().map(|site| {
            let rest = &sources[site.file.0 as usize][usize::from(site.range.start())..];
            (site.file.0, rest.lines().next().unwrap_or_default().to_string())
        }).collect()
    }

    fn at(file: u32, text: &str) -> (u32, String) {
        (file, text.to_string())
    }

    #[test]
    fn definitions_of_variables() {
        // A local's or parameter's declaration, not its later assignments
        assert_eq!(definitions_at(&["local x = 1\nx = 2\nprint(@x)"]), [at(0, "x = 1")]);
        assert_eq!(definitions_at(&["local function f(a, b)\nreturn @b end"]), [at(0, "b)")]);
        assert_eq!(definitions_at(&["local function f()\nend\n@f()"]), [at(0, "f()")]);
        // The innermost of shadowed ones
        assert_eq!(definitions_at(&["local x = 1\ndo local x = 2\nprint(@x) end"]), [at(0, "x = 2")]);
        // Every assignment to a global, in any file
        assert_eq!(definitions_at(&["Count = 0", "function Count()\nend\nprint(@Count)"]), [at(0, "Count = 0"), at(1, "Count()")]);
        // Nothing for one the files never assign
        assert!(definitions_at(&["print(@Unknown)"]).is_empty());
        // `self` is declared by the method's name
        assert_eq!(definitions_at(&["local M = {}\nfunction M.Inner:Go()\nprint(@self) end"]), [at(0, "Go()")]);
        assert!(definitions_at(&["local M = {}\nfunction M.Go()\nprint(@self) end"]).is_empty());
    }

    #[test]
    fn definitions_of_fields() {
        // Every assignment to the field, on the table it's read from
        assert_eq!(definitions_at(&["local t = { v = 1 }\nt.v = 2\nlocal y = t.@v"]), [at(0, "v = 1 }"), at(0, "v = 2")]);
        // Not another table's field of the same name
        assert_eq!(definitions_at(&["local a, b = {}, {}\na.v = 1\nb.v = 2\nlocal y = b.@v"]), [at(0, "v = 2")]);
        // Methods, across files through the addon's namespace
        assert_eq!(definitions_at(&["local _, ns = ...\nfunction ns:Init()\nend", "local _, ns = ...\nns:@Init()"]), [at(0, "Init()")]);
        // Dotted names along the way
        assert_eq!(definitions_at(&["local A = { B = {} }\nfunction A.B.Go()\nend\nA.@B.Go()"]), [at(0, "B = {} }")]);
        // From a mixin, or from `__index`
        assert_eq!(definitions_at(&["local M = {}\nfunction M:Init()\nend\nlocal t = Mixin({}, M)\nt:@Init()"]), [at(0, "Init()")]);
        assert_eq!(definitions_at(&["local M = {}\nfunction M:Init()\nend\nlocal f = Mixin(CreateFrame('Frame'), M)\nf:@Init()"]), [at(0, "Init()")]);
        assert_eq!(definitions_at(&["local B = {}\nfunction B:Go()\nend\nlocal t = setmetatable({}, { __index = B })\nt:@Go()"]), [at(0, "Go()")]);
        // `self` in a method is the table it's defined on
        assert_eq!(definitions_at(&["local M = {}\nfunction M:A()\nend\nfunction M:B() self:@A() end"]), [at(0, "A()")]);
        // Fields of tables whose identity isn't known (like globals no file
        // assigns), by name, as reading them falls back on those
        assert_eq!(definitions_at(&["SomeFrame.label = 1\nlocal y = OtherFrame.@label"]), [at(0, "label = 1")]);
        // Each frame made in the files is its own table
        assert!(definitions_at(&["local f = CreateFrame('Frame')\nf.label = 1\nlocal g = CreateFrame('Frame')\nlocal y = g.@label"]).is_empty());
        // String keys are the field of that name, both ways round
        assert_eq!(definitions_at(&["local t = {}\nt['v'] = 1\nlocal y = t.@v"]), [at(0, "'v'] = 1")]);
        assert_eq!(definitions_at(&["local t = { ['v'] = 1 }\nt.v = 2\nlocal y = t[@'v']"]), [at(0, "'v'] = 1 }"), at(0, "v = 2")]);
        // Nothing for a WoW object's own methods, which the files don't define
        assert!(definitions_at(&["local f = CreateFrame('Frame')\nf:@SetPoint('TOP')"]).is_empty());
    }
}
