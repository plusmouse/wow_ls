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
//!   any time. Unless no assignment can come after the closure is made (every
//!   one is earlier in the declaring function, with no loop around the closure
//!   but not the declaration): then it's the type where the closure is made.
//! * Tables the game saves between sessions (`## SavedVariables`, and the
//!   tables stored in them) hold what the addon writes, not tables of unknown
//!   shape it stores there from outside, like an import.
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
//! * A call is recursive when its caller and callee are in one cycle of
//!   calls, found from every function's calls once a walk is done, for the
//!   next walk. So a call depends only on whether the two are in one cycle,
//!   not on everything each function it reaches calls.
//! * Each record also notes, for each key the body influences (each write,
//!   each local it declares, its signature), what that depended on: what
//!   the statement doing it read, and what went into the locals it used,
//!   however indirectly. What a call passes depends on its callee and
//!   arguments, not on what the callee returns, and which functions a body
//!   calls on how each callee was reached. These say what an edit can
//!   change (see `reach`), and change nothing a walk finds
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

/// Added to the slot of a table a deep copy makes inside the copy a
/// `CopyTable` call makes, with the rest of the slot a hash of where in it
/// that table is, so it's the same on every walk
const COPY: u32 = 1 << 29;

/// Added to the slot of a table one of the definitions' functions returns new
/// at a call (see `FreshObject`), with the function's id above `FAMILY_SHIFT`
/// and the return's slot below. Every table that function makes at that
/// return is of one family, which stands for any of them (see `family`)
const FAMILY_MEMBER: u32 = 1 << 28;
const FAMILY_SHIFT: u32 = 8;
/// The most functions of the definitions whose tables have a family
const MOST_FAMILIES: u32 = 1 << 20;

/// The pseudo file of families' identities: past any file's and those of
/// dependencies (`LAYER_FILES`), and before those of namespaces
const FAMILIES: FileId = FileId(u32::MAX - (1 << 20));

/// The pseudo file of stand-ins: one identity for each place that holds
/// more tables made in the files than are kept apart (`MOST_OBJECTS`), its
/// slot the place's number (see `stand_in`). What it stands for, its
/// members, is in `Analysis::stand_ins`. Just before families'
const STAND_INS: FileId = FileId(u32::MAX - (1 << 20) - 1);

/// Somewhere a value is kept between statements or walks, which a set of
/// tables too large to keep apart is replaced in by its stand-in
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Place {
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
static PLACES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<Place, u32>>> = std::sync::LazyLock::new(Default::default);

/// The stand-in for the tables at `place`
fn stand_in(place: Place) -> TableId {
    let mut places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
    let next = places.len() as u32;
    let slot = *places.entry(place).or_insert(next);
    Location { file: STAND_INS, offset: TextSize::new(0), slot }
}

/// The stand-in for the tables at `place`, if it has one yet
fn existing_stand_in(place: Place) -> Option<TableId> {
    let places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
    places.get(&place).map(|&slot| Location { file: STAND_INS, offset: TextSize::new(0), slot })
}

fn is_stand_in(at: TableId) -> bool {
    at.file == STAND_INS
}

/// `ty`, to be kept at `place`: if it's more tables than are kept apart,
/// the place's stand-in, noted in `stand_ins` as standing for them. With
/// the stand-in and the tables it stands for that it didn't before
fn kept_at(stand_ins: &mut HashMap<TableId, Vec<TableId>>, place: Place, ty: Type) -> (Type, Option<(TableId, Vec<TableId>)>) {
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
fn union_all(types: impl IntoIterator<Item = Type>) -> Option<Type> {
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
        joined = joined | Type { set: TypeSet::NEVER, function: FunctionRef::None, named, numeric: NamedRef::None };
    }
    Some(joined)
}

/// How many tables a type holds, without looking past a few
fn many_tables(named: NamedRef) -> bool {
    match named {
        NamedRef::Tables(id) => object_set(id).len() > MOST_OBJECTS,
        _ => false,
    }
}

/// A call whose callee's summaries are being applied (see `ForwardedCall`,
/// `FieldWrite`): what it passes, and where it came from in the function
/// making it, for passing the summaries on as that function's own
struct Passing {
    /// By the callee's parameter positions
    passed: Values,
    /// For each position, the calling function's parameter it passes on, if
    /// it does
    origins: Vec<Option<usize>>,
    /// Whether those past the callee's named parameters are the calling
    /// function's `...`
    rest_origin: bool,
    /// How many parameters the callee names
    named: usize,
    /// The types of the callee's parameters, as its body sees them
    parameters: Vec<Type>,
    self_value: Option<Type>,
    /// What `self` is in the calling function's terms, if a summary can be
    /// on it there (see `Walker::write_target`)
    self_origin: Option<Result<On, TableId>>,
    /// What the call returns
    returns: Values,
}

impl Passing {
    /// The calling function's parameter passed on at `position`, if it is one
    fn origin(&self, position: usize) -> Option<usize> {
        self.origins.get(position).copied().flatten()
    }

    /// What's passed at `position`, kept to the parameter's type, as the
    /// callee's body sees it, which what it's passed to may narrow: the
    /// parameter's type where nothing's known of what's passed
    fn argument(&self, position: usize) -> Type {
        let passed = self.passed.get(position);
        let Some(&parameter) = self.parameters.get(position) else { return passed };
        match says_nothing(passed) {
            true => parameter,
            false => narrowed_to(passed, parameter),
        }
    }

    /// `part` of a summary of the callee, in the calling function's terms:
    /// what it passes on of its own, or else the value passed, as `argument`
    /// gives it
    fn in_caller_terms(&self, part: Forwarded, argument: &dyn Fn(usize) -> Type) -> Vec<Forwarded> {
        match part {
            Forwarded::Parameter(i) => vec![self.origin(i).map_or_else(|| Forwarded::Value(argument(i)), Forwarded::Parameter)],
            Forwarded::Truthy(i) => vec![self.origin(i).map_or_else(|| Forwarded::Value(argument(i).truthy()), Forwarded::Truthy)],
            Forwarded::Rest if self.rest_origin => vec![Forwarded::Rest],
            Forwarded::Rest => self.passed.values.iter().skip(self.named).map(|&ty| Forwarded::Value(ty)).collect(),
            value => vec![value],
        }
    }
}

/// Whether nothing's known of a value: it may be any value that's true (and
/// perhaps false or nil), as `unknown`, `unknown (not nil)` and `unknown
/// (truthy)` are
fn says_nothing(ty: Type) -> bool {
    ty.set.contains(TypeSet::UNKNOWN.without(TypeSet::NIL | TypeSet::FALSE))
}

/// Another handle on the same code
fn copy_expression(expression: &Expression) -> Option<Expression> {
    Expression::cast(expression.syntax().clone())
}

/// What a call returns for a field (`ReturnedField`), as the function's body
/// returns `body`: that field's type for the call, unless nothing is known
/// of it, kept to what the body could return
fn narrowed_to(field: Type, body: Type) -> Type {
    if field.set == TypeSet::UNKNOWN || field.set == TypeSet::NEVER {
        return body
    }
    if body.set == TypeSet::UNKNOWN {
        return field
    }
    match field.set.intersection(body.set) {
        TypeSet::NEVER => body,
        set => Type { set, ..field },
    }
}

/// The identity of a new table the definitions' function `function` returns
/// at a call at `at`, in return position `slot`, if it can have one
fn family_member(at: Location, function: BuiltinId, slot: usize) -> Option<Location> {
    let function = u32::try_from(function).ok().filter(|&f| f < MOST_FAMILIES)?;
    let slot = u32::try_from(slot).ok().filter(|&s| s + 1 < 1 << FAMILY_SHIFT)?;
    Some(Location { slot: FAMILY_MEMBER | function << FAMILY_SHIFT | (slot + 1), ..at })
}

/// For a table one of the definitions' functions made, or that function's
/// family itself, the family: one identity for every table it makes there,
/// which a union of several of them is, holding what each was given. Other
/// tables have none
fn family(at: Location) -> Option<Location> {
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
const ELEMENT: u32 = 1 << 27;
/// The slot of the identity standing for every element of one constructor
const ELEMENTS: u32 = ELEMENT | (ELEMENT - 1);
/// The slot of the identity holding what's written through `ELEMENTS`'s,
/// which each element may then have
const THROUGH_ELEMENTS: u32 = ELEMENT | (ELEMENT - 2);

/// For a table written directly inside a constructor, the identity of all
/// that constructor's elements: one table for any of them, holding what
/// each was given. A union of more tables than are kept apart is it, so
/// a loop over a long list of records still sees their fields
fn elements_of(at: Location) -> Option<Location> {
    let flags = at.slot & (POOL_OBJECT | PSEUDO | COPY | FAMILY_MEMBER);
    if flags != 0 || at.slot & ELEMENT == 0 || at.slot == ELEMENTS || at.slot == THROUGH_ELEMENTS {
        return None
    }
    Some(Location { offset: TextSize::new(at.slot & (ELEMENT - 1)), slot: ELEMENTS, ..at })
}

/// For the identity of a constructor's elements, the one holding what's
/// written through it (see `THROUGH_ELEMENTS`)
fn through_elements(at: Location) -> Option<Location> {
    (at.slot == ELEMENTS && at.slot & (POOL_OBJECT | PSEUDO | COPY | FAMILY_MEMBER) == 0)
        .then_some(Location { slot: THROUGH_ELEMENTS, ..at })
}

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
                    _ => NamedRef::Unknown,
                },
            },
        }
    }

    /// The tables made in the files this may be, if it's only those
    fn tables(self) -> Option<Vec<TableId>> {
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
    fn several_tables(tables: Vec<TableId>) -> NamedRef {
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
    fn in_family(self) -> Option<NamedRef> {
        match self {
            NamedRef::Table(at) => Some(NamedRef::Table(family(at)?)),
            // Whether one may have members beyond its type's isn't up to the
            // call that made it
            NamedRef::Instance { named, at, extended } => Some(NamedRef::Instance { named, at: family(at)?, extended }),
            _ => None,
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
        // Several of a family are its identity
        let families: Vec<Option<NamedRef>> = objects.iter().map(|o| o.in_family()).collect();
        for (i, object) in objects.iter_mut().enumerate() {
            if let Some(family) = families[i] {
                if families.iter().enumerate().any(|(j, f)| j != i && *f == Some(family)) {
                    *object = family;
                }
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
    /// and parent each call's gets), and the parameters it mixes in. For a
    /// function of the definitions, they're the call's arguments by position
    pub fresh_returns: Vec<Option<FreshObject>>,
    /// The functions whose results its body uses, by calling them, so calls
    /// that come back round to it are known to be recursive
    pub calls: Vec<FunctionId>,
    /// The methods it calls whose function depends on its arguments, for each
    /// call to it to call with its own
    pub forwarded_calls: Vec<ForwardedCall>,
    /// The fields it gives what it's passed, for each call to it to give with
    /// its own
    pub field_writes: Vec<FieldWrite>,
    /// For each return, the field of `self` or a parameter it returns, if it
    /// always returns one: what each call returns is that field of its own
    pub returned_fields: Vec<Option<ReturnedField>>,
    /// What a `for` loop over what it returns gets, when every return is a
    /// call a loop knows that of, like `return ipairs(self.list)`: so
    /// `for _, v in object:Enumerate()` gets what the list holds
    pub iterated: Option<Values>,
    /// For one of the definitions' that only some game clients have, which
    /// (`Classic`, `Classic and Forever`), shown in its signature
    pub only: Option<String>,
}

/// What a `FieldWrite` or `ReturnedField` is on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum On {
    /// A method's `self`
    SelfValue,
    /// What it's passed for a parameter, by position
    Parameter(usize),
    /// The table it returns new at each call (see `FreshObject`), by return position
    Returned(usize),
}

/// A field a function gives what it's passed, as `ColorMixin:SetRGBA(r, g,
/// b, a)` gives `self.r` its `r`, and `CreateColor(r, g, b, a)` the `r` of
/// the table it returns, through the methods it calls
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldWrite {
    pub on: On,
    pub field: String,
    /// What it's given: each of these, together
    pub value: Vec<Forwarded>,
}

/// A field a function returns, of `self` or a parameter, as `ColorMixin:GetRGB()`
/// returns `self.r`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReturnedField {
    pub on: On,
    pub field: String,
}

/// A method a function calls on what a parameter is, or on a table it makes
/// with parameters mixed in, as `CreateAndInitFromMixin(mixin, ...)` calls
/// `Init` on its new table: which function that is is up to each call's
/// arguments, so each call to the function calls it, with its own
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForwardedCall {
    /// What may hold the method: `self`, or the arguments of parameters, the
    /// last first
    pub on: Vec<On>,
    pub method: String,
    pub arguments: Vec<Forwarded>,
}

/// An argument of a `ForwardedCall`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Forwarded {
    /// What the function was passed for a parameter, by position
    Parameter(usize),
    /// Its `...`: what it was passed beyond its parameters. Only last
    Rest,
    /// What it was passed for a parameter, if it's true: `x` in `x or 1`
    Truthy(usize),
    Value(Type),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FreshObject {
    /// The table or object it makes, if it's made in the files analysed or
    /// those of their dependencies (not for a function of the definitions)
    pub made: Option<TableId>,
    pub mixed_parameters: Vec<usize>,
    /// When it's a pool, the parameters mixed into its objects (by its init
    /// or reset function)
    pub pool_mixed_parameters: Option<Vec<usize>>,
    /// For a function of the definitions, the globals mixed in besides the
    /// parameters, by name. One made in the files has them through `made`
    pub mixin_globals: Vec<String>,
    /// For a frame `CreateFrame` makes with a kind the function is passed,
    /// the parameter it's passed in: a call passing a literal kind
    /// (`"Button"`) gets a frame of that kind
    pub kind_parameter: Option<usize>,
    /// For a function of the definitions, the fields the table is given
    /// after it's made, as Blizzard's code sets them where it uses it: each
    /// call's has them. One made in the files has them through `made`
    pub fields: Vec<(String, Type)>,
}

/// What calling a function reads of it: all but what it calls, which only
/// finding its cycle of calls reads (`Read::CallCycle`), and its parameters'
/// types, which `Read::ParameterTypes` does. Those settle walk by walk, and only its own body reads them
struct Signature<'a>(&'a FunctionInfo);

impl PartialEq for Signature<'_> {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0, other.0);
        a.method == b.method && a.varargs == b.varargs && a.returns == b.returns && a.fresh_returns == b.fresh_returns
            && a.parameters.len() == b.parameters.len()
            && a.parameters.iter().zip(&b.parameters).all(|(a, b)| a.name == b.name && a.mandated == b.mandated)
            && a.forwarded_calls == b.forwarded_calls && a.field_writes == b.field_writes && a.returned_fields == b.returned_fields
            && a.iterated == b.iterated
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
    pub name: Arc<str>,
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
    /// How many loops were being walked where it was declared
    loop_depth: usize,
    /// The furthest place the function declaring it writes it, its
    /// declaration included
    last_write: TextSize,
    /// Whether a function inside the one declaring it writes it
    closure_written: bool,
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
    /// What the field is on, for go to definition into Blizzard's code
    pub base: NamedRef,
    pub write: bool,
    /// A read that's directly called: `f` in `t.f(x)` or `t:f(x)`
    pub callee: bool,
    /// For a callee, what this call returns when that's more than the function
    /// says (a pool's `Acquire` giving the pool's object)
    pub call_returns: Option<Values>,
    /// For a method a `ForwardedCall` stands for, the function calling it,
    /// each of whose calls resolves it: not a read of a function it can't
    /// tell, if every call to that is seen
    pub forwarded: Option<FunctionId>,
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
    /// `root` is its file's tree
    fn path(&self, root: Option<&SyntaxNode>) -> String {
        let Some(access) = root.and_then(|root| self.access.try_to_node(root)) else {
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
    when_true: Vec<(Target, Type)>,
    when_false: Vec<(Target, Type)>,
}

/// What a condition narrows: a local of the function being walked, or a
/// field reached from one by names (`self.margin`, `info.a.b`)
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Local(BindingId),
    Field(BindingId, Vec<String>),
}

impl Narrowing {
    /// Without the fields it narrows, for when something may change them
    /// before the narrowing would apply
    fn locals_only(self) -> Narrowing {
        let locals = |narrowed: Vec<(Target, Type)>| narrowed.into_iter().filter(|(t, _)| matches!(t, Target::Local(_))).collect();
        Narrowing { when_true: locals(self.when_true), when_false: locals(self.when_false) }
    }
}

impl Narrowing {
    fn negated(self) -> Narrowing {
        Narrowing { when_true: self.when_false, when_false: self.when_true }
    }
}

/// Both of two sets of narrowed types holding: a local in both is the
/// intersection
fn both(mut a: Vec<(Target, Type)>, b: Vec<(Target, Type)>) -> Vec<(Target, Type)> {
    for (target, ty) in b {
        match a.iter_mut().find(|(other, _)| *other == target) {
            Some((_, existing)) => *existing = intersect(*existing, ty.set),
            None => a.push((target, ty)),
        }
    }
    a
}

/// Whether evaluating `node` may call a function, which could change a
/// table's fields. `type(x)`, the usual test, can't
fn may_call(walker: &Walker, node: &SyntaxNode) -> bool {
    node.descendants()
        .filter_map(FunctionCall::cast)
        .any(|call| !walker.calls_library_global(&call, "type"))
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

impl Operation {
    /// The expressions it's made of
    fn syntax_nodes(&self) -> Vec<SyntaxNode> {
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
    fn range(&self) -> TextRange {
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
fn uncallable(ty: Type) -> bool {
    ty.set.intersection(TypeSet::FUNCTION | OBJECTS) == TypeSet::NEVER
        && ty.set.intersection(TypeSet::BOOLEAN | TypeSet::NUMBER | TypeSet::STRING) != TypeSet::NEVER
}

/// The kinds of Lua value `set` may be, as Lua names them: `boolean or function`
fn kind_names(set: TypeSet) -> String {
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
fn single_kind(set: TypeSet) -> Option<TypeSet> {
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

/// Something code assigns that other files see: a global, or a field of a
/// table (see `Analysis::writes`)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Written {
    Global(String),
    Field(TableId, String),
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
        NamedRef::Tables(_) => NamedRef::several_tables(named.identities().into_iter().map(|at| renumber(at, file)).collect()),
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
    }
}

fn renumber_forwarded(forwarded: Forwarded, file: &impl Fn(FileId) -> FileId) -> Forwarded {
    match forwarded {
        Forwarded::Value(ty) => Forwarded::Value(renumber_type(ty, file)),
        other => other,
    }
}

thread_local! {
    static MADE_WITH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While one is held, objects are described with the template and mixins
/// they're made with (`ButtonT & M`) rather than just their native type
/// (`Button`), as hover shows them: for looking into the analysis
pub struct MadeWith(bool);

impl MadeWith {
    pub fn shown() -> MadeWith {
        MadeWith(MADE_WITH.replace(true))
    }
}

impl Drop for MadeWith {
    fn drop(&mut self) {
        MADE_WITH.set(self.0);
    }
}

/// A file to analyse
pub struct SourceFile {
    pub root: SyntaxNode,
    /// The addon the file is part of, by its place in load order, so `...` at
    /// its top is that addon's name and namespace. Addons analysed together
    /// (one and those it depends on) each have their own namespace
    pub addon: Option<u32>,
    /// For a script handler written inline in XML, which the game makes a
    /// function of: what it's called with
    pub handler: Option<Handler>,
}

/// What an inline XML script handler is called with: `self`, the object the
/// XML makes, and the arguments the game passes that kind of handler, by the
/// names it gives them
#[derive(Debug, Clone)]
pub struct Handler {
    pub self_type: Type,
    /// Each with the type the game passes
    pub parameters: Vec<(String, Type)>,
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
    /// Data types (see `Definitions::has_fixed_fields`) a value of which was
    /// given fields the analysis can't name: under a computed key, by
    /// `rawset`, from a mixin or through a metatable
    pub opened_types: HashSet<NamedId>,
    /// What each table made in the files holds beyond its named fields
    pub entries: HashMap<TableId, Entries>,
    /// Each table's metatable, from `setmetatable`
    pub metatables: HashMap<TableId, Type>,
    /// What `...` is in each function taking it, when every call is seen: the
    /// arguments past its named parameters
    pub vararg_values: HashMap<FunctionId, Values>,
    /// For each function in a cycle of calls (one calling itself, or calling
    /// one that comes back round to it), the cycle's first function: a call
    /// between two with the same is recursive. Found from what each function
    /// calls once a walk is done, for the next walk
    pub call_cycles: HashMap<FunctionId, FunctionId>,
    /// How many walks it took to settle
    pub walks: usize,
    /// Each file's tree, for what field references point to. The green tree,
    /// which can go to another thread, unlike a `SyntaxNode`
    roots: Vec<rowan::GreenNode>,
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
    /// For each callback registry made in the files (a table with
    /// `CallbackRegistryMixin`) and event, by its name's hash, what its
    /// `TriggerEvent` calls pass after the event's name
    pub triggered: HashMap<(TableId, u64), Values>,
    /// The tables each table or object made in the files had mixed into it (by
    /// `Mixin` or `CreateFromMixins`), whose fields it has too
    pub mixins: HashMap<TableId, Vec<Type>>,
    /// For each table mixed into WoW objects (directly, or through tables made
    /// from it with `CreateFromMixins`), the one type those objects all are,
    /// and whether they may have members beyond that type's. What `self` is in
    /// the table's methods
    pub mixed_into: HashMap<TableId, (NamedId, bool)>,
    /// The tables each mixin was mixed into, as `CreateFromMixins(M)` makes
    /// one: an object `M` reaches through one of them has its fields too,
    /// so `self` in `M`'s methods may have them
    pub mixin_copies: HashMap<TableId, Vec<TableId>>,
    /// Whether the walk took something not known yet to be nothing (see
    /// `Walker::provisional`)
    pub provisional: bool,
    /// The table each table `CopyTable` made is a copy of
    pub copies: HashMap<TableId, TableId>,
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
    /// The tables the game saves between sessions (see `tables_saved_in`)
    saved_tables: HashSet<TableId>,
    /// For each stand-in, the tables it stands for: those ever kept at its
    /// place, only growing (see `Walker::stored`)
    pub stand_ins: HashMap<TableId, Vec<TableId>>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// One file on its own
    pub fn analyse_file(root: &SyntaxNode, definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse(&[SourceFile { root: root.clone(), addon: None, handler: None }], definitions)
    }

    /// Files in the order they load, as one program. The `FileId`s in the
    /// result are positions in `files`
    pub fn analyse(files: &[SourceFile], definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse_over(files, definitions, Arc::default(), &[])
    }

    /// `analyse`, for files loaded after the addons `layer` comes from
    /// `saved` names the globals the game saves between sessions
    pub fn analyse_over(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>, saved: &[String]) -> Analysis {
        // Replaying is only ever faster; turning it off is for checking it
        let replay = std::env::var_os("WOW_LS_NO_REPLAY").is_none();
        Analysis::analyse_replaying(files, definitions, layer, saved, replay)
    }

    /// `analyse`, replaying function bodies whose inputs haven't changed
    /// since the walk before, or walking every one every time
    fn analyse_replaying(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>, saved: &[String], replay: bool) -> Analysis {
        let _analysing = Analysing::with(definitions);
        let mut result = Walker::new(None, definitions, &layer, Default::default(), replay).walk(files);
        result.saved_tables = result.tables_saved_in(saved);
        // Nothing to compare the first walk with, but no body it recorded can
        // be replayed anyway, as it walked without a previous walk
        let mut changed = HashSet::default();
        let mut seen = HashSet::from_iter([result.fingerprint()]);
        // Whether walks have started to cycle, so each is combined with the
        // one before
        let mut combining = false;
        // Whether walks have settled once, and go on with what tables nothing's
        // been seen stored in hold unknown (see `Walker::settled`)
        let mut unstored_unknown = false;
        for walks in 2.. {
            let mut walker = Walker::new(Some(&result), definitions, &layer, changed, replay);
            walker.settled = unstored_unknown;
            let mut next = walker.walk(files);
            next.walks = walks;
            next.replayed += result.replayed;
            next.walked += result.walked;
            if combining {
                next.combine_inputs(&result);
                next.find_call_cycles();
            }
            // Once found, still saved: leaving out a write can take away the
            // field a table was found through, and keeping it again would put
            // it back, forever
            next.saved_tables = next.tables_saved_in(saved);
            next.saved_tables.extend(result.saved_tables.iter().copied());
            changed = next.changed_reads(&result);
            // Nothing the next walk would read has changed
            let settled = changed.is_empty();
            // Walks are deterministic, so a state coming round again would
            // cycle forever (a field and parameter feeding each other can
            // swap between two types). From then on each walk keeps what
            // the one before had too: both sides of the cycle, which only
            // grows, so it settles
            let repeat = !seen.insert(next.fingerprint());
            if repeat && !combining && std::env::var_os("SHOW_COMBINING").is_some() {
                eprintln!("COMBINING starts at walk {walks} (phase {}), {} keys changed:", if unstored_unknown { 2 } else { 1 }, changed.len());
                let at = |l: &Location| -> String {
                    let text = files.get(l.file.0 as usize).map(|f| f.root.text().to_string()).unwrap_or_default();
                    let o = usize::from(l.offset).min(text.len());
                    let line = text[..o].matches('\n').count() + 1;
                    let start = text[..o].rfind('\n').map_or(0, |i| i + 1);
                    let end = text[o..].find('\n').map_or(text.len(), |i| o + i);
                    format!("f{}:{} slot {:x} `{}`", l.file.0, line, l.slot, text[start..end].trim())
                };
                let name_of = |h: u64| -> String {
                    result.fields.values().flat_map(|m| m.keys()).chain(result.any_table_fields.keys()).chain(result.function_fields.iter())
                        .find(|n| name_key(n) == h).cloned()
                        .or_else(|| next.fields.values().flat_map(|m| m.keys()).find(|n| name_key(n) == h).cloned())
                        .or_else(|| next.field_references.iter().map(|f| f.name()).find(|n| name_key(n) == h).map(str::to_string))
                        .unwrap_or_else(|| format!("#{h:x}"))
                };
                let describe = |r: &Read| -> String { match r {
                    Read::Binding(id) => { let b = &next.bindings[*id]; format!("Binding {} {:?} f{} decl {:?}: {} -> {}  [{:?} -> {:?}]", b.name, b.kind, b.file.0, b.declaration.map(|d| at(&Location { file: b.file, offset: d.start(), slot: 0 })), result.bindings.get(*id).map(|o| next.describe(o.types)).unwrap_or_default(), next.describe(b.types), result.bindings.get(*id).map(|o| o.types), b.types) }
                    Read::Field(l, h) => { let n = name_of(*h); let o = result.fields.get(l).and_then(|m| m.get(&n)).copied(); let v = next.fields.get(l).and_then(|m| m.get(&n)).copied(); format!("Field {} of {}: {} -> {}  [{:?} -> {:?}]", n, at(l), o.map(|t| next.describe(t)).unwrap_or_default(), v.map(|t| next.describe(t)).unwrap_or_default(), o, v) }
                    Read::AnyTableField(h) => format!("AnyTableField {}", name_of(*h)),
                    Read::FunctionField(h) => format!("FunctionField {}", name_of(*h)),
                    Read::Fields(l) => format!("Fields of {}", at(l)),
                    Read::Entries(l) => format!("Entries of {}", at(l)),
                    Read::Parent(l) => format!("Parent of {}", at(l)),
                    Read::Function(l) => format!("Function {}", at(l)),
                    Read::ParameterTypes(l) => format!("ParameterTypes {}", at(l)),
                    other => format!("{other:?}"),
                } };
                let mut keys: Vec<String> = changed.iter().map(describe).collect();
                keys.sort();
                for k in keys.iter().take(40) { eprintln!("    {k}"); }
            }
            combining |= repeat;
            result = next;
            if !settled {
                continue
            }
            if unstored_unknown {
                break
            }
            // What isn't known yet is unknown from here on, until walks
            // settle again (see `Walker::settled`). Starting from where they
            // settled, a table filled from itself already holds what it's
            // given, and a function called round a loop is passed it. Bodies
            // reading a table nothing's been seen stored in, or a field
            // written nowhere seen, are walked again;
            // parameters change at the end of this walk, which the next sees
            let written: HashSet<u64> = result.any_table_fields.keys().chain(layer.any_table_fields.keys()).map(|name| name_key(name)).collect();
            changed = result.records.values()
                .flat_map(|record| record.reads.iter())
                .filter(|read| match read {
                    Read::Entries(at) => !result.entries.contains_key(at) && !layer.entries.contains_key(at),
                    Read::AnyTableField(name) => !written.contains(name),
                    _ => false,
                })
                .cloned()
                .collect();
            if changed.is_empty() && !result.provisional {
                break
            }
            unstored_unknown = true;
            seen.clear();
            seen.insert(result.fingerprint());
            combining = false;
        }
        result.complete();
        result
    }

    /// What only the result shows, not needed between walks: done once, for
    /// the last walk
    fn complete(&mut self) {
        // Only the next walk needed them, unless they're asked for
        if !KEEP_RECORDS.with(|keep| keep.get()) {
            self.records = HashMap::default();
            self.pending_calls = Vec::new();
        }
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
        let roots = self.syntax_roots();
        let fields = self.field_references.iter().filter(|f| f.write && is_table(f.types))
            .map(|f| (f.file, f.range.start(), f.types, f.path(roots.get(f.file.0 as usize))));
        let mut named: Vec<_> = variables.chain(fields).collect();
        named.sort_by_key(|&(file, start, ..)| (file, start));
        for (_, _, types, name) in named {
            if let NamedRef::Table(at) = types.named {
                self.table_names.entry(at).or_insert(name);
            }
        }
    }

    /// The tables the globals named `saved` hold, and those stored in them,
    /// however deep: what the game saves between sessions
    fn tables_saved_in(&self, saved: &[String]) -> HashSet<TableId> {
        let mut found = HashSet::default();
        let mut pending: Vec<TableId> = self.bindings.iter()
            .filter(|b| b.kind == BindingKind::Global && saved.iter().any(|name| **name == *b.name))
            .flat_map(|b| b.types.named.identities())
            .collect();
        while let Some(at) = pending.pop() {
            if !found.insert(at) {
                continue
            }
            let fields = self.fields.get(&at).into_iter().flat_map(|fields| fields.values());
            let entries = self.entries.get(&at).map(|entries| entries.values);
            for value in fields.copied().chain(entries) {
                pending.extend(value.named.identities());
            }
        }
        found
    }

    /// Finds `call_cycles` from what each function calls: the groups of
    /// functions that can each come round to calling the others
    fn find_call_cycles(&mut self) {
        let mut ids: Vec<FunctionId> = self.functions.keys().copied().collect();
        ids.sort();
        let index: HashMap<FunctionId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let calls: Vec<Vec<usize>> = ids.iter()
            .map(|id| self.functions[id].calls.iter().filter_map(|callee| index.get(callee).copied()).collect())
            .collect();
        let mut called_by: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
        for (caller, callees) in calls.iter().enumerate() {
            for &callee in callees {
                called_by[callee].push(caller);
            }
        }
        // Kosaraju: the order calls finish in, then the groups reached
        // backwards from the last to finish
        let mut order = Vec::with_capacity(ids.len());
        let mut visited = vec![false; ids.len()];
        for start in 0..ids.len() {
            if visited[start] {
                continue
            }
            visited[start] = true;
            let mut stack = vec![(start, 0)];
            while let Some((at, next)) = stack.pop() {
                if let Some(&callee) = calls[at].get(next) {
                    stack.push((at, next + 1));
                    if !visited[callee] {
                        visited[callee] = true;
                        stack.push((callee, 0));
                    }
                } else {
                    order.push(at);
                }
            }
        }
        let mut group = vec![usize::MAX; ids.len()];
        let mut cycles = HashMap::default();
        for &start in order.iter().rev() {
            if group[start] != usize::MAX {
                continue
            }
            group[start] = start;
            let mut members = vec![start];
            let mut pending = vec![start];
            while let Some(at) = pending.pop() {
                for &caller in &called_by[at] {
                    if group[caller] == usize::MAX {
                        group[caller] = start;
                        members.push(caller);
                        pending.push(caller);
                    }
                }
            }
            // One function on its own is a cycle only if it calls itself
            if members.len() > 1 || calls[start].contains(&start) {
                let first = members.iter().map(|&i| ids[i]).min().expect("at least one");
                for &member in &members {
                    cycles.insert(ids[member], first);
                }
            }
        }
        self.call_cycles = cycles;
    }

    /// The tables stand-in `at` stands for, here and in what this addon depends
    /// on. None for any other table
    pub fn stand_in_members(&self, at: TableId) -> Vec<TableId> {
        if !is_stand_in(at) {
            return Vec::new()
        }
        let mut members: Vec<TableId> = Vec::new();
        for list in self.stand_ins.get(&at).into_iter().chain(self.layer.stand_ins.get(&at)) {
            for &member in list {
                if !members.contains(&member) {
                    members.push(member);
                }
            }
        }
        members
    }

    /// What a walk from `self` would read differently from one from `older`.
    /// Empty when the walks have settled
    fn changed_reads(&self, older: &Analysis) -> HashSet<Read> {
        let mut changed = HashSet::default();
        for &at in self.saved_tables.symmetric_difference(&older.saved_tables) {
            changed.insert(Read::Saved(at));
        }
        fn keys<K: std::hash::Hash + Eq + Copy, V>(a: &HashMap<K, V>, b: &HashMap<K, V>) -> HashSet<K> {
            a.keys().chain(b.keys()).copied().collect()
        }
        let longest = self.bindings.len().max(older.bindings.len());
        for id in 0..longest {
            let same = match (self.bindings.get(id), older.bindings.get(id)) {
                (Some(a), Some(b)) => a.name == b.name && a.types == b.types && a.assigned == b.assigned
                    && a.last_write == b.last_write && a.closure_written == b.closure_written,
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
            if self.call_cycles.get(&id) != older.call_cycles.get(&id) {
                changed.insert(Read::CallCycle(id));
            }
            if a.map(|f| f.method) != b.map(|f| f.method) {
                changed.insert(Read::Method(id));
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
        for &id in self.opened_types.symmetric_difference(&older.opened_types) {
            changed.insert(Read::Opened(id));
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
        for at in keys(&self.mixin_copies, &older.mixin_copies) {
            if self.mixin_copies.get(&at) != older.mixin_copies.get(&at) {
                changed.insert(Read::MixedInto(at));
            }
        }
        for at in keys(&self.parents, &older.parents) {
            if self.parents.get(&at) != older.parents.get(&at) {
                changed.insert(Read::Parent(at));
            }
        }
        for (at, event) in keys(&self.triggered, &older.triggered) {
            if self.triggered.get(&(at, event)) != older.triggered.get(&(at, event)) {
                changed.insert(Read::Triggered(at, event));
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
        for at in keys(&self.stand_ins, &older.stand_ins) {
            if self.stand_ins.get(&at) != older.stand_ins.get(&at) {
                changed.insert(Read::StandIn(at));
            }
        }
        changed
    }

    /// Adds what `other` had to everything a walk reads from this one
    fn combine_inputs(&mut self, other: &Analysis) {
        for (at, members) in &other.stand_ins {
            let mine = self.stand_ins.entry(*at).or_default();
            for &member in members {
                if !mine.contains(&member) {
                    mine.push(member);
                }
            }
            mine.sort();
        }
        for (at, fields) in &other.fields {
            let mine = self.fields.entry(*at).or_default();
            for (name, &ty) in fields {
                *mine.entry(name.clone()).or_default() |= ty;
            }
        }
        for (at, &ty) in &other.parents {
            *self.parents.entry(*at).or_default() |= ty;
        }
        for (key, values) in &other.triggered {
            self.triggered.entry(*key)
                .and_modify(|all| *all = all.union(values))
                .or_insert_with(|| values.clone());
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
        for (at, tables) in &other.mixin_copies {
            let mine = self.mixin_copies.entry(*at).or_default();
            for table in tables {
                if !mine.contains(table) {
                    mine.push(*table);
                }
            }
            mine.sort();
        }
        for (at, &ty) in &other.pool_objects {
            *self.pool_objects.entry(*at).or_default() |= ty;
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
        let opened_types = self.opened_types.iter().fold(0u64, |sum, id| sum.wrapping_add(mixed((11u8, id))));
        let bindings = mixed(self.bindings.iter().map(|b| (&b.name, b.types, b.assigned)).collect::<Vec<_>>());
        mixed([
            fields,
            function_fields,
            bindings,
            map(2, &self.parents),
            map(12, &self.triggered),
            map(3, &self.mixins),
            map(4, &self.mixed_into),
            map(11, &self.mixin_copies),
            map(5, &self.pool_objects),
            map(6, &self.any_table_fields),
            map(7, &self.entries),
            map(8, &self.metatables),
            map(9, &self.vararg_values),
            map(10, &self.functions),
            map(13, &self.stand_ins),
            opened_types,
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
                    format!("{}<{}>", self.display_name(ty), self.describe_shallow(object))
                }
                // By its native type alone, not the template or mixins it's
                // made with, unless those are being shown
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] => {
                    let mixins = match MADE_WITH.get() {
                        true => ty.named.identity().map(|at| self.object_mixins(at)).unwrap_or_default(),
                        false => Vec::new(),
                    };
                    match mixins.is_empty() {
                        true => self.display_name(ty),
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

    /// How an object's type is shown: by its native type, or while
    /// `MadeWith` is held, by the template it's made from
    fn display_name(&self, ty: Type) -> String {
        let Some(id) = ty.named.named_id() else { return String::new() };
        match MADE_WITH.get() {
            true => self.definitions.named_type(id).map(|named| named.name.clone()),
            false => self.definitions.display_name(id).map(str::to_string),
        }.unwrap_or_default()
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
        if let Some(only) = &function.only {
            signature += &format!(" ({only} only)");
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
        if let (true, FunctionRef::One(id)) = (sites.is_empty(), field.types.function) {
            sites.push(Site { file: id.file, range: TextRange::empty(id.offset) });
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
    fn external_member(&self, object: NamedRef, name: &str, depth: usize) -> Option<(std::path::PathBuf, u32)> {
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
        // A field `CopyTable` gave a copy is where the original's is set
        if sites.is_empty() {
            if let Some(source) = holder.and_then(|at| self.copies.get(&at).or_else(|| self.layer.copies.get(&at))) {
                if Some(*source) != table {
                    return self.field_sites(Some(*source), name)
                }
            }
        }
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
    fn syntax_roots(&self) -> Vec<SyntaxNode> {
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

struct Scope {
    names: HashMap<String, BindingId>,
    /// Code in this scope might not run (or might run many times)
    conditional: bool,
    /// A branch of an `if`, which joins what its branches leave (see
    /// `Join`), so assignments in it replace what a variable held
    joined: bool,
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
    /// Whether walks have settled once, so what isn't known yet is taken to
    /// be unknown. Until then, what a table made in the files that nothing's
    /// been seen stored in holds is nothing yet, as are the parameters of a
    /// function with no calls seen yet: so values passed round a loop (a
    /// pool's frames, released into its list of free ones and acquired from
    /// it again) grow from what's given them, rather than from unknown,
    /// which they'd never be rid of. Once walks settle, such a table holds
    /// unknown, as it may be filled where that can't be seen, and a function
    /// that may be called where it isn't seen takes anything, and walks go
    /// on until they settle again
    settled: bool,
    /// Whether this walk took something not known yet to be nothing (see
    /// `settled`), outside any body it replayed (whose record notes what it
    /// read), so walks go on once they settle
    provisional: std::cell::Cell<bool>,
    /// The function bodies being walked, innermost last, and what each has read
    frames: Vec<Frame>,
    reads: std::cell::RefCell<Vec<HashSet<Read>>>,
    /// What each body's writes depend on, innermost body last
    deps: std::cell::RefCell<Vec<BodyDeps>>,
    /// For each call being walked, the depth of its level in the innermost
    /// body's `levels` (see `Walker::call`), and what was read finding its
    /// callee, which is all which function it calls depends on
    call_scopes: Vec<CallScope>,
    /// Where what finding the callee of the call whose returns are being
    /// read is in its statement's logs, for `returns_of`: the level, then
    /// the stretches of its reads and locals
    callee_found: Option<(usize, std::ops::Range<usize>, std::ops::Range<usize>)>,
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
    /// The `if` statements being walked, innermost last
    joins: Vec<Join>,
    /// Where the constructor is that the table constructor about to be
    /// walked is written directly inside (see `ELEMENT`)
    element_of: Option<TextSize>,
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
    /// Fields narrowed here (see `Target::Field`), by their local and names.
    /// Any call or field write may change a field, through another name for
    /// its table too, so either forgets them all, as entering a loop does
    narrowed_fields: Vec<(BindingId, Vec<String>, Type)>,
    /// Counts each time `narrowed_fields` is forgotten, so what's restored
    /// after a branch isn't what the branch made stale
    field_epoch: usize,
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
    /// Data types given fields that can't be named, seen on this walk
    opened_types: HashSet<NamedId>,
    /// Entries seen on this walk. Reads use the previous walk's, like fields
    entries: HashMap<TableId, Entries>,
    /// For each stand-in, the tables it stands for, so far on this walk.
    /// Reads add to it too (see `Walker::read_through`)
    stand_ins: std::cell::RefCell<HashMap<TableId, Vec<TableId>>>,
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
    /// What callback registries' events passed on this walk (see
    /// `Analysis::triggered`). Reads use the previous walk's
    triggered: HashMap<(TableId, u64), Values>,
    /// The field names each function was stored under, in any table
    stored_in_fields: HashMap<FunctionId, HashSet<String>>,
    /// Mixins seen on this walk. Reads use the previous walk's, like fields
    mixins: HashMap<TableId, Vec<Type>>,
    /// What each table made in the files was mixed into on this walk
    mixed_into: HashMap<TableId, Vec<NamedRef>>,
    /// The table each copy `CopyTable` made is of
    copies: HashMap<TableId, TableId>,
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
    /// How many loops were being walked where each function in
    /// `function_ranges` is
    closure_loop_depths: Vec<usize>,
    /// Per function being walked: what each return position returns new, so
    /// far (None once returns disagree)
    fresh: Vec<Option<Vec<Option<FreshObject>>>>,
    /// Per function being walked: the tables and objects it has stored in a
    /// variable outside it (an upvalue or a global), which it may return
    /// again from another call, as a cache is, so aren't new each call
    kept_outside: Vec<HashSet<TableId>>,
    /// Per function being walked: the methods it calls that `ForwardedCall`s
    /// stand for
    forwarded: Vec<Vec<ForwardedCall>>,
    /// Per function being walked: the fields it gives what it's passed, on
    /// `self`, a parameter, or a table it makes (which `FieldWrite`s name by
    /// the return that gives it, once that's known)
    field_writes: Vec<Vec<(Result<On, TableId>, String, Vec<Forwarded>)>>,
    /// Per function being walked: for each return, the field of `self` or a
    /// parameter it returns, so far (None once returns disagree)
    returned_fields: Vec<Option<Vec<Option<ReturnedField>>>>,
    /// Per function being walked: what a loop over what it returns gets, so
    /// far (Some(None) once a return gives none)
    iterating: Vec<Option<Option<Values>>>,
    /// Per function being walked: its `self`, if it's a method
    self_bindings: Vec<Option<BindingId>>,
    /// Which parameter of its function each parameter binding is
    parameter_index: HashMap<BindingId, usize>,
    /// For each frame made with a kind its function is passed, which of the
    /// function's parameters that is (see `FreshObject::kind_parameter`)
    kind_parameters: HashMap<TableId, usize>,
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
    /// A data type given fields that can't be named
    Opened(NamedId),
    StoredInField(FunctionId, String),
    Entry(TableId, Type, Type),
    Metatable(TableId, Type),
    CallArguments(FunctionId, Values),
    MixinsReplaced(TableId, Vec<Type>),
    MixinAdded(TableId, Type),
    MixedInto(TableId, NamedRef),
    /// A table `CopyTable` made, and the table it's a copy of
    CopiedFrom(TableId, TableId),
    PoolCallback(FunctionId),
    AssignedTo(FunctionId, BindingId),
    ParameterMixin(TableId, usize),
    Parent(TableId, Type),
    /// A callback registry's event, by its name's hash, triggered with these
    Triggered(TableId, u64, Values),
    PoolObject(TableId, Type),
    Function(FunctionId, FunctionInfo),
    /// A variable declared outside the body was assigned this
    Binding(BindingId, Type),
    /// A stand-in stands for these tables too
    StandIn(TableId, Vec<TableId>),
}

impl Write {
    /// The keys whose value this changes, as reading them sees it
    fn keys(&self) -> Vec<Read> {
        match self {
            Write::Field(at, name, _) => vec![Read::Field(*at, name_key(name)), Read::Fields(*at)],
            Write::AnyTableField(name, _) => vec![Read::AnyTableField(name_key(name))],
            Write::Opened(id) => vec![Read::Opened(*id)],
            Write::StoredInField(_, name) => vec![Read::FunctionField(name_key(name))],
            Write::Entry(at, _, _) => vec![Read::Entries(*at)],
            Write::Metatable(at, _) => vec![Read::Metatable(*at)],
            Write::CallArguments(function, _) => vec![Read::ParameterTypes(*function), Read::Varargs(*function)],
            Write::MixinsReplaced(at, _) | Write::MixinAdded(at, _) | Write::ParameterMixin(at, _) => vec![Read::Mixins(*at)],
            Write::MixedInto(at, _) => vec![Read::MixedInto(*at)],
            Write::CopiedFrom(copy, _) => vec![Read::Fields(*copy), Read::Entries(*copy), Read::MixedInto(*copy)],
            Write::PoolCallback(function) => vec![Read::ParameterTypes(*function)],
            Write::AssignedTo(function, _) => vec![Read::Function(*function)],
            Write::Parent(at, _) => vec![Read::Parent(*at)],
            Write::Triggered(at, event, _) => vec![Read::Triggered(*at, *event)],
            Write::PoolObject(at, _) => vec![Read::PoolObject(*at)],
            Write::Function(id, _) => vec![Read::Function(*id), Read::CallCycle(*id), Read::ParameterTypes(*id), Read::Method(*id)],
            Write::Binding(id, _) => vec![Read::Binding(*id)],
            Write::StandIn(at, _) => vec![Read::StandIn(*at)],
        }
    }
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
    Opened(NamedId),
    FunctionField(u64),
    Entries(TableId),
    Metatable(TableId),
    Mixins(TableId),
    MixedInto(TableId),
    Parent(TableId),
    Triggered(TableId, u64),
    PoolObject(TableId),
    Varargs(FunctionId),
    /// Which cycle of calls a function is in, if any, which finding
    /// recursion reads (see `Analysis::call_cycles`)
    CallCycle(FunctionId),
    /// Only whether a function is a method: all of its signature what a call
    /// passes it depends on. Never read by a body, only depended on (see
    /// `Walker::call`)
    Method(FunctionId),
    /// What a method's `self` is, which the statement defining it decides.
    /// Never read by a body, only depended on
    SelfType(FunctionId),
    /// The tables a stand-in stands for
    StandIn(TableId),
    /// Only the types of a function's parameters, as the previous walk settled them
    ParameterTypes(FunctionId),
    /// What the globals XML names as mixins are
    LuaMixins,
    /// Whether a table is one the game saves between sessions
    Saved(TableId),
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
    /// Upvalues read as they were where the body is, which must be the same
    /// to replay it
    upvalues: Arc<[(BindingId, Type)]>,
    /// The function it's in, if it's not at a file's top level
    parent: Option<FunctionId>,
    /// What it wrote itself, not counting the functions inside it, which
    /// `writes` includes as replaying it does them too. What it read itself
    /// is in `own_influences`
    own_writes: Arc<[Arc<Write>]>,
    /// Each key its own writes, the bindings it declares and the calls
    /// passing its parameters on influence, with the reads that went into
    /// it: those of the statement doing it, and of the locals it used
    own_influences: Arc<BodyInfluences>,
}

/// What a body did and read as it was walked, from which what each key it
/// influences depended on is worked out when asked (see
/// `BodyInfluences::get`): only the last walk's are ever asked for
struct BodyInfluences {
    function: FunctionId,
    body: BodyDeps,
    /// What it read outside any statement
    top: Dep,
    own_writes: Arc<[Arc<Write>]>,
    /// The locals it declares itself, not those of functions inside it
    own_bindings: Vec<BindingId>,
    /// The function each call passing its parameters on calls, and what the
    /// call read
    pending: Vec<(FunctionId, Arc<Dep>)>,
    found: std::sync::OnceLock<Arc<[(Read, Arc<[Read]>)]>>,
}

impl std::fmt::Debug for BodyInfluences {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "BodyInfluences({:?})", self.function)
    }
}

impl BodyInfluences {
    /// Each key it influences: each write, each local it declares, its
    /// signature, what it passes on. With what each depended on: what the
    /// statement doing it read, and what the locals it used read
    fn get(&self) -> &[(Read, Arc<[Read]>)] {
        self.found.get_or_init(|| self.work_out())
    }

    /// What it read itself, not counting the functions inside it
    fn own_reads(&self) -> HashSet<Read> {
        self.body.own_reads()
    }

    fn work_out(&self) -> Arc<[(Read, Arc<[Read]>)]> {
        let (body, id) = (&self.body, self.function);
        let closed = body.closed();
        // What reaches these from its top (its parameters) does so through
        // the locals the statements read, not wholesale
        let requirements = body.final_reads(&body.requirements, &closed);
        let mut outside = Dep::default();
        outside.extend(&body.signature);
        outside.extend(&body.calls);
        outside.extend(&self.top);
        let (signature, calls, outside) = (body.final_reads(&body.signature, &closed), body.final_reads(&body.calls, &closed), body.final_reads(&outside, &closed));
        let mut influences: Vec<(Read, Arc<[Read]>)> = Vec::new();
        // Writes of one statement share its reads: found once
        let mut found: HashMap<*const Dep, Arc<[Read]>> = HashMap::default();
        let mut reads_of = |dep: &Arc<Dep>| found.entry(Arc::as_ptr(dep)).or_insert_with(|| body.final_reads(dep, &closed)).clone();
        for (index, write) in self.own_writes.iter().enumerate() {
            // Its own signature: what its returns, requirements and calls read
            if let Write::Function(written, _) = &**write {
                if *written == id {
                    influences.push((Read::Function(id), signature.clone()));
                    // Its parameters' types: what calls pass (their own
                    // influence) and what the body requires of them
                    influences.push((Read::ParameterTypes(id), requirements.clone()));
                    influences.push((Read::CallCycle(id), calls.clone()));
                    // Whether it's a method is how it's written, which reads nothing
                    influences.push((Read::Method(id), Arc::from([])));
                    continue
                }
            }
            let deps = match body.write_deps.get(index) {
                Some(Some(dep)) => reads_of(dep),
                // Outside any statement: as the signature and calls do
                _ => outside.clone(),
            };
            for key in write.keys() {
                influences.push((key, deps.clone()));
            }
        }
        for &binding in &self.own_bindings {
            let deps: Arc<[Read]> = body.local_numbers.get(&binding)
                .and_then(|&number| closed.get(number as usize))
                .map(|reads| reads.iter().map(|number| body.keys[number as usize].clone()).collect())
                .unwrap_or_default();
            influences.push((Read::Binding(binding), deps));
        }
        for (function, dep) in &body.self_type_deps {
            influences.push((Read::SelfType(*function), reads_of(dep)));
        }
        for (callee, dep) in &self.pending {
            let deps = reads_of(dep);
            influences.push((Read::ParameterTypes(*callee), deps.clone()));
            influences.push((Read::Varargs(*callee), deps));
        }
        influences.into()
    }
}

/// A set of a body's numbers for keys or locals (see `BodyDeps`): a bit
/// each, as a body reads few
#[derive(Default, Clone, Debug, PartialEq)]
struct Bits(Vec<u64>);

impl Bits {
    fn of(numbers: &[u32]) -> Bits {
        let mut bits = Bits::default();
        for &number in numbers {
            bits.insert(number);
        }
        bits
    }

    fn insert(&mut self, number: u32) {
        let word = number as usize / 64;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (number % 64);
    }

    /// Takes it out, returning whether it was there
    fn remove(&mut self, number: u32) -> bool {
        let Some(word) = self.0.get_mut(number as usize / 64) else { return false };
        let bit = 1 << (number % 64);
        let there = *word & bit != 0;
        *word &= !bit;
        there
    }

    /// Adds `other`'s, returning whether that added any
    fn union(&mut self, other: &Bits) -> bool {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        let mut grew = false;
        for (mine, theirs) in self.0.iter_mut().zip(&other.0) {
            let joined = *mine | theirs;
            grew |= joined != *mine;
            *mine = joined;
        }
        grew
    }

    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            (0..64u32).filter(move |bit| bits & (1 << bit) != 0).map(move |bit| word as u32 * 64 + bit)
        })
    }
}

/// What a statement read: the keys, and the body's own locals, whose own
/// reads are added once the body is done (see `BodyDeps::closed`), as a
/// local can be assigned after it's read, round a loop. By their numbers in
/// the body
#[derive(Default, Debug, Clone)]
struct Dep {
    reads: Bits,
    locals: Bits,
}

impl Dep {
    /// From stretches of a statement's logs
    fn of(reads: &[u32], locals: &[u32]) -> Dep {
        Dep { reads: Bits::of(reads), locals: Bits::of(locals) }
    }

    fn extend(&mut self, other: &Dep) {
        self.reads.union(&other.reads);
        self.locals.union(&other.locals);
    }
}

/// A statement being walked: what it read, and what it did
#[derive(Default)]
struct DepLevel {
    /// The numbers of the keys it read, in order, repeats and all: a call's
    /// reads are a stretch of it (see `CallScope`)
    reads: Vec<u32>,
    /// The numbers of the body's own locals it read, likewise
    locals: Vec<u32>,
    /// Positions in `BodyDeps::write_deps`
    writes: Vec<usize>,
    /// The body's own locals it assigned or narrowed
    bindings: Vec<BindingId>,
    /// Positions in the walk's pending calls it made
    pending: Vec<usize>,
    /// Methods it defines, whose `self` it decides
    self_types: Vec<FunctionId>,
    /// Whether it gives the body's signature something: a return, a
    /// requirement of a parameter, a field given what's passed, a call
    /// passing a method on
    signature: bool,
    /// Whether it calls a function, which finding cycles of calls reads
    calls: bool,
    /// Whether it requires a type of a parameter of the body, which its
    /// parameters' types depend on (as well as what calls pass)
    requirement: bool,
}

/// What each thing a body does depends on, as it's walked (see
/// `Record::own_influences`). The keys it reads and its own locals are
/// numbered in the order it first meets them, so what a statement read is
/// a few bits
#[derive(Default)]
struct BodyDeps {
    bindings_start: usize,
    /// The body's top, then each statement being walked, innermost last
    levels: Vec<DepLevel>,
    keys: Vec<Read>,
    key_numbers: HashMap<Read, u32>,
    /// The keys it read, which replaying it checks: not `Read::Method`,
    /// which is only depended on
    noted: Bits,
    locals: Vec<BindingId>,
    local_numbers: HashMap<BindingId, u32>,
    /// Each write's statement's reads, once the statement is done
    write_deps: Vec<Option<Arc<Dep>>>,
    /// The reads of each statement assigning each local, by its number,
    /// joined once the body is done
    binding_deps: HashMap<u32, Vec<Arc<Dep>>>,
    pending_deps: Vec<(usize, Arc<Dep>)>,
    self_type_deps: Vec<(FunctionId, Arc<Dep>)>,
    signature: Dep,
    calls: Dep,
    requirements: Dep,
}

impl BodyDeps {
    fn key(&mut self, read: &Read) -> u32 {
        if let Some(&number) = self.key_numbers.get(read) {
            return number
        }
        let number = self.keys.len() as u32;
        self.keys.push(read.clone());
        self.key_numbers.insert(read.clone(), number);
        number
    }

    fn local(&mut self, id: BindingId) -> u32 {
        if let Some(&number) = self.local_numbers.get(&id) {
            return number
        }
        let number = self.locals.len() as u32;
        self.locals.push(id);
        self.local_numbers.insert(id, number);
        number
    }

    /// A statement is done: what it did depends on all it read
    fn resolve(&mut self, level: DepLevel) {
        // Most statements did nothing that depends on what they read
        if level.writes.is_empty() && level.bindings.is_empty() && level.pending.is_empty() && level.self_types.is_empty()
            && !level.signature && !level.calls && !level.requirement {
            return
        }
        let dep = Arc::new(Dep::of(&level.reads, &level.locals));
        for write in level.writes {
            self.write_deps[write] = Some(dep.clone());
        }
        for binding in level.bindings {
            let number = self.local(binding);
            let deps = self.binding_deps.entry(number).or_default();
            // Assigned again in the same statement, or narrowed and joined
            if !deps.last().is_some_and(|last| Arc::ptr_eq(last, &dep)) {
                deps.push(dep.clone());
            }
        }
        for pending in level.pending {
            self.pending_deps.push((pending, dep.clone()));
        }
        for function in level.self_types {
            self.self_type_deps.push((function, dep.clone()));
        }
        if level.signature {
            self.signature.extend(&dep);
        }
        if level.calls {
            self.calls.extend(&dep);
        }
        if level.requirement {
            self.requirements.extend(&dep);
        }
    }

    /// Each of the body's locals' keys, by its number, with those of the
    /// locals that went into it, however indirectly
    fn closed(&self) -> Vec<Bits> {
        let mut direct: Vec<Dep> = vec![Dep::default(); self.locals.len()];
        for (&number, deps) in &self.binding_deps {
            for dep in deps {
                direct[number as usize].extend(dep);
            }
        }
        let mut closed: Vec<Bits> = direct.iter().map(|dep| dep.reads.clone()).collect();
        loop {
            let mut grew = false;
            for (number, dep) in direct.iter().enumerate() {
                for local in dep.locals.iter().filter(|&local| local as usize != number) {
                    let theirs = closed[local as usize].clone();
                    grew |= closed[number].union(&theirs);
                }
            }
            if !grew {
                return closed
            }
        }
    }

    /// The keys `dep` reads, with those its locals read
    fn final_reads(&self, dep: &Dep, closed: &[Bits]) -> Arc<[Read]> {
        let mut reads = dep.reads.clone();
        for local in dep.locals.iter() {
            if let Some(theirs) = closed.get(local as usize) {
                reads.union(theirs);
            }
        }
        reads.iter().map(|number| self.keys[number as usize].clone()).collect()
    }

    /// The keys it read
    fn own_reads(&self) -> HashSet<Read> {
        self.noted.iter().map(|number| self.keys[number as usize].clone()).collect()
    }
}

/// A call being walked: where its stretch of its statement's logs (and
/// what the statement did) starts, and where finding its callee ended
struct CallScope {
    level: usize,
    reads: usize,
    locals: usize,
    writes: usize,
    pending: usize,
    callee: Option<(usize, usize)>,
    ended: bool,
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
    /// The body's own, not those of the functions inside it
    own_writes: Vec<Arc<Write>>,
    /// Upvalues read as they are where the body is (see `upvalue_here`)
    upvalues: Vec<(BindingId, Type)>,
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

thread_local! {
    /// Whether analyses keep the last walk's records, for `reach`
    static KEEP_RECORDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with analyses keeping the last walk's records
pub fn keeping_records<R>(f: impl FnOnce() -> R) -> R {
    KEEP_RECORDS.with(|keep| keep.set(true));
    let result = f();
    KEEP_RECORDS.with(|keep| keep.set(false));
    result
}

mod reach;
mod shrink;
pub use reach::print_reach;
pub use shrink::{shrinkage, span};

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
const BUILTIN_PATHS: [&str; 23] = [
    "assert", "getmetatable", "gmatch", "ipairs", "next", "pairs", "pcall", "rawget", "setmetatable", "unpack", "xpcall",
    "string.gmatch", "table.create", "table.insert", "table.remove", "tinsert", "tremove",
    "CreateFontStringPool", "CreateFramePool", "CreateObjectPool", "CreateTexturePool",
    "CallbackRegistryMixin.RegisterCallback", "CallbackRegistryMixin.TriggerEvent",
];

/// Passes after which a loop that still hasn't settled is widened: any variable
/// whose type is still changing becomes unknown. Without this, a loop already
/// terminates (each pass adds at least one bit to some variable's TypeSet), but
/// nested loops multiply their passes. Realistic code settles in 2-3 passes
const LOOP_PASSES_BEFORE_WIDENING: usize = 8;

impl<'a> Walker<'a> {
    fn new(prior: Option<&'a Analysis>, definitions: &Arc<Definitions>, layer: &Arc<Layer>, changed: HashSet<Read>, replay: bool) -> Walker<'a> {
        let builtins = BUILTIN_PATHS.iter().map(|&path| (path, definitions.at_path(path))).collect();
        // Stand-ins only grow: this walk starts with all the walk before found
        let stand_ins = std::cell::RefCell::new(prior.map(|prior| prior.stand_ins.clone()).unwrap_or_default());
        Walker { prior, changed, replay, layer: layer.clone(), definitions: definitions.clone(), builtins, stand_ins, ..Walker::default() }
    }

    /// A global's preset type: what a dependency gives it, else the
    /// definitions'
    fn preset_global(&self, name: &str) -> Option<Type> {
        self.layer.globals.get(name).copied().or_else(|| self.definitions.global(name))
    }

    /// Notes a read for each function body being walked
    fn note(&self, read: Read) {
        // Only the innermost: each body's reads join its parent's when it ends
        if let Some(body) = self.deps.borrow_mut().last_mut() {
            let number = body.key(&read);
            body.noted.insert(number);
            // One of its own locals: what went into it, once that's all known
            let local = match read {
                Read::Binding(id) if id >= body.bindings_start && self.bindings.get(id).is_some_and(|b| b.kind != BindingKind::Global) => Some(body.local(id)),
                _ => None,
            };
            if let Some(level) = body.levels.last_mut() {
                match local {
                    Some(local) => level.locals.push(local),
                    None => level.reads.push(number),
                }
            }
        }
        // Outside any body: only a body's reads are recorded. A body's are
        // its numbered keys, joined with those of the functions inside it
        // once it's done (see `function_body`)
        if self.deps.borrow().is_empty() {
            if let Some(innermost) = self.reads.borrow_mut().last_mut() {
                innermost.insert(read);
            }
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
                if let Some(handler) = &file.handler {
                    self.declare_handler(handler, file.root.text_range());
                }
                self.statements(&block);
                self.pop_scope();
            }
        }
        let prior = self.prior;
        let mut analysis = self.finish();
        // The same calls as the walk before, so the same cycles
        let same_calls = prior.is_some_and(|prior| prior.functions.len() == analysis.functions.len()
            && analysis.functions.iter().all(|(id, f)| prior.functions.get(id).is_some_and(|p| p.calls == f.calls)));
        match prior {
            Some(prior) if same_calls => analysis.call_cycles = prior.call_cycles.clone(),
            _ => analysis.find_call_cycles(),
        }
        analysis
    }

    /// `self` and the other arguments of an inline XML handler, which its
    /// body (the file's top level) sees as the game's function's parameters
    fn declare_handler(&mut self, handler: &Handler, range: TextRange) {
        let depth = self.scopes.len() - 1;
        let parameters = std::iter::once(("self", handler.self_type)).chain(handler.parameters.iter().map(|(name, types)| (name.as_str(), *types)));
        for (name, types) in parameters {
            let id = self.new_binding(name, BindingKind::Parameter, None, types, MAIN_CHUNK, depth);
            self.bindings[id].assigned = true;
            self.bindings[id].visible = Some(range);
            self.scopes[depth].names.insert(name.to_string(), id);
        }
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
        // The tables among what each mixin was mixed into, in a fixed order
        let mixin_copies = self.mixed_into.iter()
            .filter_map(|(&mixin, objects)| {
                let mut tables: Vec<TableId> = objects.iter().flat_map(|object| match object {
                    NamedRef::Table(_) | NamedRef::Tables(_) => object.identities(),
                    _ => Vec::new(),
                }).collect();
                tables.sort();
                tables.dedup();
                (!tables.is_empty()).then_some((mixin, tables))
            })
            .collect();
        Analysis {
            function_fields,
            mixed_into,
            mixin_copies,
            provisional: self.provisional.get(),
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
            triggered: self.triggered,
            mixins: self.mixins,
            copies: self.copies,
            diagnostics: self.diagnostics,
            bindings: self.bindings,
            references: self.references,
            field_references: self.field_references,
            functions: self.functions,
            definitions: self.definitions,
            fields: self.fields,
            any_table_fields: self.any_table_fields,
            opened_types: self.opened_types,
            entries: self.entries,
            metatables: self.metatables,
            vararg_values: self.vararg_values,
            call_cycles: HashMap::default(),
            walks: 1,
            roots: self.roots.iter().map(|root| root.green().into_owned()).collect(),
            layer: self.layer,
            // Found once the walk is done
            saved_tables: HashSet::default(),
            // With those of the walk before, which it started with
            stand_ins: self.stand_ins.into_inner(),
        }
    }

    /// Each parameter's type for the next walk: what the body requires, else the
    /// arguments from every call when every call is visible, else unknown
    /// For each function, whether every call to it is one the walk saw: it
    /// was assigned straight to a variable (local or global) only ever called,
    /// which never holds another function, and any field it was stored in is
    /// only ever called, with no reads of a field by that name that could be
    /// it (`unresolved`)
    fn all_calls_seen(&self, only_called: &[bool], escaped: &HashSet<FunctionId>, unresolved: &HashSet<&str>) -> HashMap<FunctionId, bool> {
        let mut all_seen = HashMap::default();
        for id in self.functions.keys() {
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
        all_seen
    }

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
        // Methods `ForwardedCall`s stand for: resolved by each call to the
        // function calling them, if every one is seen
        let mut forwarded: Vec<(&str, FunctionId)> = Vec::new();
        for field in self.field_references.iter().filter(|f| !f.write) {
            match (field.types.function, field.forwarded) {
                (FunctionRef::One(function), _) if !field.callee => { escaped.insert(function); }
                (FunctionRef::Unknown, Some(caller)) => forwarded.push((field.name(), caller)),
                (FunctionRef::Unknown, None) => { unresolved.insert(field.name()); }
                _ => {}
            }
        }
        // Which functions have every call seen depends on which field names
        // are read unresolved, which in turn depends on that: until it
        // settles, which it does, as it only ever loses functions
        let mut all_seen;
        loop {
            all_seen = self.all_calls_seen(&only_called, &escaped, &unresolved);
            let before = unresolved.len();
            for &(name, caller) in &forwarded {
                if !all_seen.get(&caller).copied().unwrap_or(false) {
                    unresolved.insert(name);
                }
            }
            if unresolved.len() == before {
                break
            }
        }
        let definitions = &self.definitions;
        // A table or object made in the files that meets the requirement is
        // kept, so what it holds and its fields aren't lost to a plain `table`
        // Any of several objects meets it when each does
        let meets = |passed: Type, mandated: Type| {
            let objects = match passed.named {
                NamedRef::Several(_) => passed.named.alternatives(),
                // Any of several tables made in the files, each of which meets it
                NamedRef::Tables(_) => passed.named.identities().into_iter().map(NamedRef::Table).collect(),
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
        // Until walks settle, which calls are seen grows as callees are found
        // (finding a method reads a field, which only the previous walk
        // knows), so a function is taken to have the calls seen so far: a
        // parameter with none is nothing yet. Taking it to be unknown as soon
        // as a call might not be seen would put unknown in what's passed
        // round a loop of calls (a pool's frames, released and acquired
        // again), which it would never be rid of. Once walks settle, a
        // function that may be called where it isn't seen takes anything
        // (see `Walker::settled`)
        let trusted: HashMap<FunctionId, bool> = ids.iter().map(|id| (*id, all_seen[id] || !self.settled)).collect();
        // A function that may be called where it isn't seen, taking only the
        // calls seen, is provisional too, as are parameters taking nothing
        // below
        if !self.settled && ids.iter().any(|id| !all_seen[id] && (!self.functions[id].parameters.is_empty() || self.functions[id].varargs.is_some())) {
            self.provisional.set(true);
        }
        let open = if self.settled { Type::UNKNOWN } else { Type::default() };
        let mut varargs: HashMap<FunctionId, Values> = ids.iter()
            .map(|id| (*id, match self.functions[id].varargs.is_some() && trusted[id] {
                true => Values { values: Vec::new(), rest: Type::default() },
                false => Values { values: Vec::new(), rest: open },
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
                        // What the calls seen pass, as any parameter takes
                        // until walks settle, when it meets the requirement
                        (Some(mandated), Some(passed)) if trusted[id] && meets(passed.get(i), mandated) => passed.get(i),
                        // Nothing passed yet: nothing yet, as the requirement
                        // (a plain `table`) would stand for a table of unknown
                        // origin, which may hold anything, round a loop that's
                        // never rid of it (see `Walker::settled`)
                        (Some(_), passed) if !self.settled && passed.as_ref().is_none_or(|p| p.get(i).set == TypeSet::NEVER) => {
                            self.provisional.set(true);
                            Type::default()
                        }
                        (Some(mandated), _) => mandated,
                        (None, Some(passed)) if trusted[id] => passed.get(i),
                        _ => {
                            if !self.settled {
                                self.provisional.set(true);
                            }
                            open
                        }
                    };
                    let slot = &mut parameters.get_mut(id).expect("every function")[i];
                    if *slot | ty != *slot {
                        *slot |= ty;
                        changed = true;
                    }
                }
                if let (true, true, Some(passed)) = (function.varargs.is_some(), trusted[id], &passed) {
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
        // Kept as their types: more tables than are kept apart are a stand-in
        let mut kept: HashMap<FunctionId, Vec<Type>> = HashMap::default();
        for (id, types) in &parameters {
            let types = types.iter().enumerate().map(|(i, &ty)| kept_at(self.stand_ins.get_mut(), Place::Parameter(*id, i), ty).0).collect();
            kept.insert(*id, types);
        }
        let parameters = kept;
        for (id, function) in self.functions.iter_mut() {
            for (parameter, &ty) in function.parameters.iter_mut().zip(&parameters[id]) {
                parameter.types = ty;
            }
            let has_calls = self.call_arguments.contains_key(id) || pending.contains_key(id);
            if let (Some(shown), true, true) = (&mut function.varargs, trusted[id], has_calls) {
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
        // A pass may change fields the next one reads
        self.forget_fields();
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
            self.forget_fields();
        }
    }

    /// Sets a binding's current type, noting the type before when in a loop
    fn set_current(&mut self, id: BindingId, types: Type) {
        let types = self.stored(self.binding_place(id), types, true);
        if let Some(body) = self.deps.get_mut().last_mut() {
            if id >= body.bindings_start && self.bindings[id].kind != BindingKind::Global {
                if let Some(level) = body.levels.last_mut() {
                    level.bindings.push(id);
                }
            }
        }
        if self.loop_depth > 0 && self.current[id] != types {
            self.loop_log.push((id, self.current[id]));
        }
        // What it held before the `if` being walked changed it
        if let Some(join) = self.joins.last_mut() {
            if id < join.bindings && !join.before.contains_key(&id) {
                join.before.insert(id, (self.current[id], join.fell_through));
            }
        }
        self.current[id] = types;
    }

    fn push_scope(&mut self, conditional: bool, end: TextSize) {
        self.scopes.push(Scope { names: HashMap::default(), conditional, joined: false, end });
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
            loop_depth: self.loop_depth,
            last_write: declaration.map_or(TextSize::from(0), |range| range.start()),
            closure_written: false,
        });
        self.current.push(types);
        // A local starts with what its declaring statement read
        if kind != BindingKind::Global {
            if let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
                level.bindings.push(id);
            }
        }
        id
    }

    fn declare(&mut self, token: &SyntaxToken, kind: BindingKind, types: Type) -> BindingId {
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
    fn global_before_assigned(&self, id: BindingId) -> Type {
        let Some(prior) = self.prior else { return Type::default() };
        self.note(Read::Binding(id));
        let name = &self.bindings[id].name;
        match prior.bindings.get(id).filter(|p| &p.name == name) {
            Some(p) if p.assigned => self.preset_global(name).unwrap_or(Type::NIL),
            _ => Type::UNKNOWN,
        }
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
    fn upvalue_here(&mut self, id: BindingId) -> Option<Type> {
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
        let tracked = !self.deps.get_mut().is_empty();
        if tracked {
            if let Some(body) = self.deps.get_mut().last_mut() {
                body.levels.push(DepLevel::default());
            }
        }
        self.statement_walked(statement);
        if tracked {
            if let Some(body) = self.deps.get_mut().last_mut() {
                if let Some(mut level) = body.levels.pop() {
                    level.signature |= matches!(statement, Statement::Return(_));
                    body.resolve(level);
                }
            }
        }
    }

    fn statement_walked(&mut self, statement: &Statement) {
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
                            Some((base_expression, field)) => {
                                let base = self.expression(&base_expression);
                                self.forget_fields();
                                // Only a value of its own, not one of several a call returns
                                let value = (expressions.len() == targets.len()).then(|| expressions.get(i)).flatten();
                                self.note_field_write(&base_expression, base, field.text(), value);
                                self.field_write(base, field.text(), types);
                                self.field_reference(target.syntax(), &field, base, types, true);
                            }
                            None => match index_access(target.syntax()) {
                                // `t[k] = v`
                                Some((base, key)) => {
                                    let base = self.expression(&base);
                                    let key = { let key = self.index_key(&key); self.forget_fields(); key };
                                    match key {
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
                        for (target, ty) in self.narrowing(condition).when_true {
                            self.narrow(&target, ty);
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
                let mut earlier_false: Vec<(Target, Type)> = Vec::new();
                // Without an `else`, code after an `if` whose branches all
                // return runs only when every condition was false
                let mut all_return = s.else_branch().is_none();
                self.joins.push(Join { bindings: self.bindings.len(), ..Join::default() });
                // What each variable holds at the end of the branches that go
                // on past the `if`
                let mut ends: HashMap<BindingId, Type> = HashMap::default();
                for (i, branch) in s.if_branches().iter().enumerate() {
                    let block = branch.block();
                    let returns = block.as_ref().is_some_and(always_returns);
                    all_return &= returns;
                    // An `elseif` condition that may call a function may change
                    // the fields the conditions before it proved things of
                    if branch.expression().is_some_and(|condition| may_call(self, condition.syntax())) {
                        earlier_false.retain(|(target, _)| matches!(target, Target::Local(_)));
                    }
                    self.narrow_locals(&earlier_false);
                    let proved = self.with_narrowing(&fields_only(&earlier_false), |w| {
                        let condition = branch.expression()?;
                        // An `elseif` condition only runs if the ones before failed
                        w.short_circuit_depth += usize::from(i > 0);
                        w.expression(&condition);
                        w.short_circuit_depth -= usize::from(i > 0);
                        w.conditions_walked();
                        let proved = w.narrowing(&condition);
                        let taken = both(earlier_false.clone(), proved.when_true.clone());
                        w.narrow_locals(&taken);
                        w.with_narrowing(&fields_only(&taken), |w| w.branch_block(block));
                        Some(proved)
                    });
                    match proved {
                        Some(proved) => earlier_false = both(earlier_false, proved.when_false),
                        None => self.branch_block(branch.block()),
                    }
                    self.end_branch(&mut ends, !returns);
                }
                if let Some(branch) = s.else_branch() {
                    let returns = branch.block().as_ref().is_some_and(always_returns);
                    self.narrow_locals(&earlier_false);
                    self.with_narrowing(&fields_only(&earlier_false), |w| w.branch_block(branch.block()));
                    self.end_branch(&mut ends, !returns);
                }
                let join = self.joins.pop().unwrap_or_default();
                // Code after it may also run with every condition false
                let skipped = s.else_branch().is_none();
                let mut ids: Vec<BindingId> = join.before.keys().copied().collect();
                ids.extend(earlier_false.iter().filter_map(|(target, _)| match target {
                    Target::Local(id) if *id < join.bindings && !join.before.contains_key(id) => Some(*id),
                    _ => None,
                }));
                ids.sort();
                ids.dedup();
                // Changes the `if` made are changes the one around it made
                if let Some(outer) = self.joins.last_mut() {
                    for (&id, &(before, _)) in &join.before {
                        if id < outer.bindings {
                            outer.before.entry(id).or_insert((before, outer.fell_through));
                        }
                    }
                }
                for id in ids {
                    let (before, fell_through) = join.before.get(&id).copied().unwrap_or((self.current[id], 0));
                    // With every condition walked, as the code after it is
                    // when none was true
                    let conditioned = join.after_conditions.get(&id).copied().unwrap_or(before);
                    let mut joined = ends.get(&id).copied().unwrap_or_default();
                    // The branches that went on before it was changed left it as it was
                    if fell_through > 0 {
                        joined |= before;
                    }
                    if skipped {
                        let narrowed = earlier_false.iter().find(|(target, _)| *target == Target::Local(id)).map(|(_, ty)| *ty);
                        joined |= narrowed.unwrap_or(conditioned);
                    }
                    // Nothing goes on past it: nothing after it runs
                    if join.fell_through == 0 && !skipped {
                        joined = conditioned;
                    }
                    self.set_current(id, joined);
                }
                if all_return {
                    for (target, ty) in earlier_false {
                        if let Target::Field(..) = target {
                            self.narrow(&target, ty);
                        }
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
                                self.forget_fields();
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
                let returned_fields: Vec<Option<ReturnedField>> = s.expressions().iter().map(|e| self.returned_field(e)).collect();
                if let Some(fields) = self.returned_fields.last_mut() {
                    *fields = Some(match fields.take() {
                        None => returned_fields,
                        // Every return has to agree
                        Some(earlier) => (0..earlier.len().max(returned_fields.len()))
                            .map(|i| match (earlier.get(i), returned_fields.get(i)) {
                                (Some(Some(a)), Some(Some(b))) if a == b => Some(a.clone()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
                let values = self.expression_values(&s.expressions());
                if !self.fresh.is_empty() {
                    let made: Vec<Option<FreshObject>> = values.values.iter()
                        .map(|value| {
                            let made = value.named.identity()
                                .filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at)))?;
                            let mixed_parameters = self.parameter_mixins.get(&made).cloned().unwrap_or_default();
                            let pool_mixed_parameters = { self.note(Read::PoolObject(made)); self.pool_objects.get(&made) }.and_then(|object| object.named.identity())
                                .map(|object| self.parameter_mixins.get(&object).cloned().unwrap_or_default());
                            Some(FreshObject { made: Some(made), mixed_parameters, pool_mixed_parameters, mixin_globals: Vec::new(), kind_parameter: self.kind_parameters.get(&made).copied(), fields: Vec::new() })
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
                // `return ipairs(t)`: what a loop over it gets, for a loop
                // over a call to this function
                let iterated = match s.expressions().as_slice() {
                    [Expression::FunctionCall(call)] => self.iterated.get(&self.location(call.syntax().text_range().start())).cloned(),
                    _ => None,
                };
                if let Some(iterating) = self.iterating.last_mut() {
                    *iterating = Some(match (iterating.take(), iterated) {
                        (None, iterated) => iterated,
                        (Some(Some(earlier)), Some(iterated)) => Some(earlier.union(&iterated)),
                        _ => None,
                    });
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
        self.field_references.push(FieldReference { file: self.file, range: name.text_range(), access: crate::syntax::SyntaxNodePtr::new(access), token: name.green().to_owned(), types, table, base: base.named, write, callee: false, call_returns: None, forwarded: None });
    }

    /// The field read just recorded is the function being called
    fn last_field_is_callee(&mut self) {
        if let Some(field) = self.field_references.last_mut() {
            field.callee = true;
        }
    }

    /// The type of field `name` of a value of type `base`
    fn field_read(&self, base: Type, name: &str) -> Type {
        // Nothing known of it yet, like a parameter no call is seen passing
        // yet: so nothing known of its field. Unknown would stay in what it
        // flows into, as for a call of nothing
        if base.set == TypeSet::NEVER {
            return Type::default()
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
            if declared.set == TypeSet::UNKNOWN {
                if let (Some(prior), Some(at)) = (self.prior, base.named.identity()) {
                    if let Some(own) = self.own_field(prior, at, name, 0) {
                        return own
                    }
                }
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
                    (Some(own), Some(any)) => Some(own | Type { named: any.named.loosened(), ..any }),
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
                self.entries_of(prior, id)
                    .filter(|entries| entries.keys.set.contains(TypeSet::STRING) && entries.values.set != TypeSet::NEVER)
                    .map(|entries| entries.values | Type::NIL)
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
                Type { named: found.named.loosened(), ..found }
            }
        }
    }

    /// Field `name` of the Lua mixins of named type `id`, the last given first,
    /// as later ones overwrite earlier ones
    fn lua_mixin_field(&self, prior: &Analysis, id: NamedId, name: &str) -> Option<Type> {
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
    fn copied_into_field(&self, prior: &Analysis, mixin: TableId, name: &str) -> Option<Type> {
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
    fn own_field(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
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

    /// What a write changes, without noting it for the bodies being walked.
    /// What a table of a family is given, the family is too
    fn effect(&mut self, write: &Write) {
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
    fn stand_in_members(&self, at: TableId) -> Vec<TableId> {
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
    fn stood_for(&self, at: TableId) -> Vec<TableId> {
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
    fn stored(&mut self, place: Place, ty: Type, record: bool) -> Type {
        let (ty, added) = kept_at(self.stand_ins.get_mut(), place, ty);
        if let (true, Some((at, added))) = (record, added) {
            self.record_write(Write::StandIn(at, added));
        }
        ty
    }

    /// `before`, kept at `place`, joined with `value`. Once it's the place's
    /// stand-in, `value`'s tables go straight into what it stands for, rather
    /// than through a set of them
    fn joined_at(&mut self, place: Place, before: Type, value: Type, record: bool) -> Type {
        if let NamedRef::Table(at) = before.named {
            if is_stand_in(at) && matches!(value.named, NamedRef::Table(_) | NamedRef::Tables(_)) && at == stand_in(place) {
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
        }
        self.stored(place, before | value, record)
    }

    /// What's read through a stand-in, at `place`: if it's more tables than
    /// are kept apart, a stand-in for them, as for what's kept there, so a
    /// read doesn't make a set that size
    fn read_through(&self, place: Place, found: Type) -> Type {
        let (ty, _) = kept_at(&mut self.stand_ins.borrow_mut(), place, found);
        if let NamedRef::Table(at) = ty.named {
            if is_stand_in(at) {
                self.note(Read::StandIn(at));
            }
        }
        ty
    }

    /// The place a binding's value is kept
    fn binding_place(&self, id: BindingId) -> Place {
        let binding = &self.bindings[id];
        match binding.declaration {
            Some(range) => Place::Binding(Location { file: binding.file, offset: range.start(), slot: 0 }),
            None => Place::Global(name_key(&binding.name)),
        }
    }

    fn effect_here(&mut self, write: &Write) {
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
    fn record_write(&mut self, write: Write) {
        // Only the innermost: each body's writes join its parent's when it ends
        let Some(innermost) = self.frames.last_mut() else { return };
        if let Write::Binding(id, _) = write {
            if id >= innermost.bindings_start {
                return
            }
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
    fn begin_declaration(&mut self) {
        if let Some(body) = self.deps.get_mut().last_mut() {
            body.levels.push(DepLevel::default());
        }
    }

    /// Ends it: what it declared depends on what it read, and on `also`
    fn end_declaration(&mut self, also: Option<Read>) {
        if let Some(body) = self.deps.get_mut().last_mut() {
            if body.levels.len() > 1 {
                if let Some(mut level) = body.levels.pop() {
                    if let Some(also) = also {
                        let number = body.key(&also);
                        level.reads.push(number);
                    }
                    body.resolve(level);
                }
            }
        }
    }

    /// The statement being walked gives the body's signature something
    fn deps_to_signature(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.signature = true;
        }
    }

    /// The statement being walked requires a type of one of the body's parameters
    fn deps_to_requirements(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.requirement = true;
            level.signature = true;
        }
    }

    /// The statement being walked calls a function
    fn deps_to_calls(&self) {
        if let Some(level) = self.deps.borrow_mut().last_mut().and_then(|body| body.levels.last_mut()) {
            level.calls = true;
        }
    }

    /// Upvalues a body read as they are where it is: the parent's too, for
    /// those declared outside it
    fn upvalues_to_parent(&mut self, upvalues: &[(BindingId, Type)]) {
        if let Some(parent) = self.frames.last_mut() {
            parent.upvalues.extend(upvalues.iter().filter(|&&(id, _)| id < parent.bindings_start).copied());
        }
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
                    if let Some(at) = object.identity() {
                        self.apply(Write::Field(at, name.to_string(), value));
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
        let parent = self.function_ids.last().copied();
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
            own_writes: Vec::new(),
            upvalues: Vec::new(),
        });
        self.reads.borrow_mut().push(Default::default());
        let bindings_start = self.bindings.len();
        // The statement defining a method decides what its `self` is
        if self_type.is_some() {
            if let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
                level.self_types.push(id);
            }
        }
        self.deps.get_mut().push(BodyDeps { bindings_start, levels: vec![DepLevel::default()], ..BodyDeps::default() });
        self.walk_function_body(def, self_type, id);
        let mut body = self.deps.get_mut().pop().expect("pushed above");
        // Its top: what's done outside any statement, like setting up its
        // parameters and writing its signature at the end
        let mut top = Dep::default();
        while let Some(mut level) = body.levels.pop() {
            top.extend(&Dep::of(&level.reads, &level.locals));
            // Left to depend on all `top` does, below
            level.writes.clear();
            body.resolve(level);
        }
        let frame = self.frames.pop().expect("pushed above");
        // What the functions inside it read, and what it did
        let mut reads = self.reads.borrow_mut().pop().expect("pushed above");
        reads.extend(body.noted.iter().map(|number| body.keys[number as usize].clone()));
        let own_function = frame.function_count_start + 1;
        let own_writes: Arc<[Arc<Write>]> = frame.own_writes.into();
        let pending: Vec<(FunctionId, Arc<Dep>)> = body.pending_deps.drain(..)
            .filter_map(|(index, dep)| Some((self.pending_calls.get(index)?.callee, dep)))
            .collect();
        let influences = BodyInfluences {
            function: id,
            own_bindings: (frame.bindings_start..self.bindings.len()).filter(|&b| self.bindings[b].function == own_function).collect(),
            own_writes: own_writes.clone(),
            pending,
            top,
            body,
            found: std::sync::OnceLock::new(),
        };
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
            upvalues: frame.upvalues.into(),
            reads: Arc::new(reads),
            parent,
            own_writes,
            own_influences: Arc::new(influences),
        };
        self.mark_dirty(id, &record);
        self.pass_to_parent(&record.reads, &record.writes);
        self.upvalues_to_parent(&record.upvalues);
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
        let unchanged = || record.reads.iter().all(|read| !self.changed.contains(read) && !self.dirty.contains(read))
            && record.upvalues.iter().all(|&(binding, types)| self.current.get(binding) == Some(&types));
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
            upvalues: Arc::clone(&record.upvalues),
            parent: record.parent,
            own_writes: Arc::clone(&record.own_writes),
            own_influences: Arc::clone(&record.own_influences),
        };
        self.upvalues_to_parent(&record.upvalues);
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
        }
        fn shared(writes: &[Arc<Write>]) -> HashMap<Read, Vec<Shared<'_>>> {
            let mut shared: HashMap<Read, Vec<Shared>> = HashMap::default();
            for write in writes {
                match &**write {
                    Write::Function(id, info) => {
                        shared.entry(Read::Function(*id)).or_default().push(Shared::Signature(info.signature()));
                    }
                    Write::Parent(at, _) => shared.entry(Read::Parent(*at)).or_default().push(Shared::Write(write)),
                    Write::Triggered(at, event, _) => shared.entry(Read::Triggered(*at, *event)).or_default().push(Shared::Write(write)),
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
        // Defining a function doesn't run it: what's narrowed around it holds
        // on after it, and nothing narrowed there holds inside it
        let outer_fields = (std::mem::take(&mut self.narrowed_fields), self.field_epoch);
        self.returns.push(None);
        self.calls.push(Vec::new());
        self.function_ranges.push((def.syntax().text_range(), self.function));
        self.closure_loop_depths.push(self.loop_depth);
        self.function_ids.push(id);
        self.function_numbers.insert(self.function, id);
        self.fresh.push(None);
        self.kept_outside.push(HashSet::default());
        self.forwarded.push(Vec::new());
        self.field_writes.push(Vec::new());
        self.returned_fields.push(None);
        self.iterating.push(None);
        self.self_bindings.push(None);
        if let Some(self_type) = self_type {
            let depth = self.scopes.len() - 1;
            // Implicit, so it's declared by the method's name: `b` in `function a:b()`
            let declaration = def.identifier().and_then(|identifier| identifier.syntax().children_with_tokens()
                .filter_map(|t| t.into_token())
                .filter(|t| t.kind() == SyntaxKind::Name)
                .last())
                .map(|name| name.text_range());
            // As the statement defining the method decides
            let function = self.function_ids.last().copied();
            self.begin_declaration();
            let id = self.new_binding("self", BindingKind::Parameter, declaration, self_type, self.function, depth);
            self.end_declaration(function.map(Read::SelfType));
            self.bindings[id].assigned = true;
            self.bindings[id].visible = Some(def.syntax().text_range());
            self.scopes[depth].names.insert("self".to_string(), id);
            if let Some(binding) = self.self_bindings.last_mut() {
                *binding = Some(id);
            }
        }
        let params = def.params();
        let parameters = params.as_ref().map(|p| p.parameter_tokens()).unwrap_or_default();
        let parameter_bindings: Vec<BindingId> = parameters.iter().enumerate()
            .map(|(i, param)| {
                // What calls pass it, and nothing else read at the top
                self.begin_declaration();
                let types = self.prior_parameter_types(id, i);
                let binding = self.declare(param, BindingKind::Parameter, types);
                self.end_declaration(None);
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
        self.closure_loop_depths.pop();
        self.function_ids.pop();
        let fresh_returns = self.fresh.pop().flatten().unwrap_or_default();
        self.kept_outside.pop();
        let forwarded_calls = self.forwarded.pop().unwrap_or_default();
        self.self_bindings.pop();
        let returned_fields = self.returned_fields.pop().flatten().unwrap_or_default();
        // Only when every return gives a loop what to iterate, and it can't
        // reach `end` without returning
        let iterated = self.iterating.pop().flatten().flatten()
            .filter(|_| def.block().as_ref().is_some_and(always_returns));
        // A table it makes is given by the return that gives it new
        let mut field_writes: Vec<FieldWrite> = Vec::new();
        for (on, field, value) in self.field_writes.pop().unwrap_or_default() {
            let on = match on {
                Ok(on) => on,
                Err(made) => match fresh_returns.iter().position(|fresh| fresh.as_ref().is_some_and(|f| f.made == Some(made))) {
                    Some(slot) => On::Returned(slot),
                    None => continue,
                },
            };
            let write = FieldWrite { on, field, value };
            if !field_writes.contains(&write) {
                field_writes.push(write);
            }
        }
        let returns = match returned {
            Some(returns) if block.as_ref().is_some_and(always_returns) => returns,
            // Reaching `end` returns nothing
            Some(returns) => returns.union(&Values::none()),
            None => Values::none(),
        };
        let returns = self.with_returned_templates(def, returns);
        // Kept as its signature: more tables than are kept apart are a stand-in
        let returns = Values {
            values: returns.values.iter().enumerate().map(|(i, &value)| self.stored(Place::Return(id, i), value, true)).collect(),
            rest: self.stored(Place::Return(id, usize::MAX), returns.rest, true),
        };
        self.pop_scope();
        self.function = outer_function;
        self.function_scope = outer_function_scope;
        self.short_circuit_depth = outer_short_circuit;
        (self.narrowed_fields, self.field_epoch) = outer_fields;

        let parameters = parameters.iter().zip(&parameter_bindings).map(|(token, binding)| Parameter {
            name: token.text().to_string(),
            // Replaced by `settle_parameter_types` once all calls are seen
            types: self.current[*binding],
            mandated: self.mandates.get(binding).copied().flatten(),
        }).collect();
        let varargs = params.is_some_and(|p| p.ellipsis())
            .then(|| Parameter { name: "...".to_string(), types: Type::UNKNOWN, mandated: None });
        self.apply(Write::Function(id, FunctionInfo { parameters, method: self_type.is_some(), varargs, returns, fresh_returns, calls, forwarded_calls, field_writes, returned_fields, iterated, only: None }));
    }

    /// `returns`, with the templates the definitions say objects the function
    /// returns are made with (see `Definitions::returned_templates`): each of
    /// that kind is of the template's type, and may have members beyond it,
    /// as others may be made with more templates
    fn with_returned_templates(&mut self, def: &FunctionDefinition, returns: Values) -> Values {
        let Some(path) = function_path(def) else { return returns };
        let declared = self.definitions.returned_templates(&path).to_vec();
        if declared.is_empty() {
            return returns
        }
        let templates: Vec<(NamedId, NamedId)> = declared.iter()
            .filter_map(|(kind, template)| {
                let kind = self.definitions.named_type_id(kind)?;
                let (template, element) = self.definitions.template(template)?;
                (element == kind).then_some((kind, template))
            })
            .collect();
        let made: std::cell::RefCell<Vec<(TableId, NamedId)>> = Default::default();
        let with = |ty: Type| -> Type {
            let named = ty.named.map_objects(|object| {
                let Some((id, _)) = object.object_type() else { return object };
                let Some(&(_, template)) = templates.iter().find(|&&(kind, _)| kind == id) else { return object };
                match object {
                    NamedRef::Instance { at, .. } => {
                        made.borrow_mut().push((at, template));
                        NamedRef::Instance { named: template, at, extended: true }
                    }
                    _ => NamedRef::Extended(template),
                }
            });
            Type { named, ..ty }
        };
        let returns = Values { values: returns.values.iter().map(|&v| with(v)).collect(), rest: with(returns.rest) };
        // What it's made in has the template's mixins as it would made with it
        for (at, template) in made.into_inner() {
            let parent = self.known_parent(at).unwrap_or(Type::UNKNOWN);
            self.add_lua_mixins(at, template, parent);
        }
        returns
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
            // Only a requirement of its own parameter says what it takes
            self.deps_to_requirements();
            self.mandates.insert(id, merged);
        }
    }

    fn returns_of(&mut self, id: FunctionId) -> Values {
        let returns = self.known_returns(id);
        let Some(&caller) = self.function_ids.last() else { return returns };
        // What it calls is found from how the callee was reached, when a
        // call says so, else from all the statement read
        let found = self.callee_found.take();
        let added = match (found, self.deps.get_mut().last_mut()) {
            (Some((level, reads, locals)), Some(body)) => match body.levels.get(level) {
                Some(level) => {
                    for &number in &level.reads[reads] {
                        body.calls.reads.insert(number);
                    }
                    for &number in &level.locals[locals] {
                        body.calls.locals.insert(number);
                    }
                    true
                }
                None => false,
            },
            _ => false,
        };
        if !added {
            self.deps_to_calls();
        }
        if let Some(calls) = self.calls.last_mut().filter(|calls| !calls.contains(&id)) {
            calls.push(id);
        }
        if !self.recursive(caller, id) {
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

    /// Whether `caller` calling `callee` is recursive: the same function, or
    /// two in one cycle of calls, as the previous walk found the cycles
    fn recursive(&self, caller: FunctionId, callee: FunctionId) -> bool {
        if caller == callee {
            return true
        }
        let Some(prior) = self.prior else { return false };
        self.note(Read::CallCycle(caller));
        self.note(Read::CallCycle(callee));
        match (prior.call_cycles.get(&caller), prior.call_cycles.get(&callee)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
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
                        let types = self.narrowed_field(expression).unwrap_or(types);
                        self.field_reference(identifier.syntax(), &field, base, types, false);
                        self.check_field(base, &field, field.text());
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
                                        self.check_field(base, &token, &name);
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
                    let types = self.expression(&operand);
                    self.check_operand(u.kind(), types, u.syntax().text_range());
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
        let values = self.call(call);
        // The function may have changed any table's fields
        if !self.calls_library_global(call, "type") {
            self.forget_fields();
        }
        values
    }

    /// A call: what it passes depends on its callee and arguments, not on
    /// what the callee returns, which the rest of the statement uses
    fn call(&mut self, call: &FunctionCall) -> Values {
        let scope = self.deps.get_mut().last().and_then(|body| {
            let level = body.levels.last()?;
            Some(CallScope {
                level: body.levels.len() - 1,
                reads: level.reads.len(),
                locals: level.locals.len(),
                writes: level.writes.len(),
                pending: level.pending.len(),
                callee: None,
                ended: false,
            })
        });
        let tracked = scope.is_some();
        if let Some(scope) = scope {
            self.call_scopes.push(scope);
        }
        let values = self.call_walked(call);
        if tracked {
            self.end_call_scope(None);
            self.call_scopes.pop();
        }
        values
    }

    /// The call being walked has written what it passes, if it hadn't
    /// already: what it wrote depends on what it read. `callee`'s signature
    /// was read only for whether it's a method, so that's all what's passed
    /// depends on of it; the rest of the statement reads all of it
    fn end_call_scope(&mut self, callee: Option<FunctionId>) {
        let Some(scope) = self.call_scopes.last_mut() else { return };
        let Some(body) = self.deps.get_mut().last_mut() else { return };
        if scope.ended || scope.level + 1 != body.levels.len() {
            return
        }
        scope.ended = true;
        let level = body.levels.last_mut().expect("checked above");
        let writes = level.writes.split_off(scope.writes.min(level.writes.len()));
        let pending = level.pending.split_off(scope.pending.min(level.pending.len()));
        if writes.is_empty() && pending.is_empty() {
            return
        }
        let mut dep = Dep::of(&level.reads[scope.reads..], &level.locals[scope.locals..]);
        if let Some(callee) = callee {
            if let Some(&function) = body.key_numbers.get(&Read::Function(callee)) {
                if dep.reads.remove(function) {
                    let method = body.key(&Read::Method(callee));
                    dep.reads.insert(method);
                }
            }
        }
        let dep = Arc::new(dep);
        for write in writes {
            body.write_deps[write] = Some(dep.clone());
        }
        for pending in pending {
            body.pending_deps.push((pending, dep.clone()));
        }
    }

    /// What finding the callee of the call being walked read
    fn callee_found(&mut self) -> Option<(usize, std::ops::Range<usize>, std::ops::Range<usize>)> {
        let scope = self.call_scopes.last()?;
        let (reads, locals) = scope.callee?;
        Some((scope.level, scope.reads..reads, scope.locals..locals))
    }

    fn call_walked(&mut self, call: &FunctionCall) -> Values {
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
                    self.check_called_variable(types);
                }
                Expression::Identifier(i) if !is_method && field_access(i.syntax()).is_some() => {
                    self.last_field_is_callee();
                    callee_read = self.field_references.len().checked_sub(1).map(CalleeRead::Field);
                    if let Some((_, name)) = field_access(i.syntax()) {
                        let base = self.field_references.last().map_or(NamedRef::None, |field| field.base);
                        self.check_called_field(base, name.text(), name.text_range(), types);
                    }
                }
                // `t["f"]()`: the field reference `expression` just recorded
                // for the key, if it's one naming a field
                Expression::Identifier(i) if !is_method && index_access(i.syntax()).is_some() => {
                    let Some((_, Expression::Literal(key))) = index_access(i.syntax()) else { return types };
                    let name = key.get_string().as_deref().and_then(string_field_name).map(str::to_string);
                    let range = key.syntax().text_range();
                    let base = self.field_references.last()
                        .filter(|field| range.contains_range(field.range))
                        .map(|field| field.base);
                    if let (Some(name), Some(base)) = (name, base) {
                        self.check_called_field(base, &name, range, types);
                    }
                }
                _ => {}
            }
            types
        });
        let receiver = if is_method { callee } else { None };
        // `a:m()` calls field `m` of `a`
        // `p:m()` on a parameter, or on a table made here with parameters
        // mixed in: the parameters that may hold `m`, for a `ForwardedCall`
        let mut forward_on = None;
        let callee = match (callee, call.method_name()) {
            (Some(base), Some(method)) => {
                let types = self.field_read(base, method.text());
                forward_on = self.forwardable_receiver(call.callee().as_ref(), base, types);
                self.field_reference(call.syntax(), &method, base, types, false);
                self.last_field_is_callee();
                if forward_on.is_some() {
                    let function = self.function_ids.last().copied();
                    if let Some(field) = self.field_references.last_mut() {
                        field.forwarded = function;
                    }
                }
                callee_read = Some(CalleeRead::Field(self.field_references.len() - 1));
                self.check_called_field(base.named, method.text(), method.text_range(), types);
                self.check_method(base, &method);
                Some(types)
            }
            (callee, _) => callee,
        };
        // Which function it calls depends only on what was read so far
        if let (Some(scope), Some(body)) = (self.call_scopes.last_mut(), self.deps.get_mut().last()) {
            if let Some(level) = body.levels.get(scope.level).filter(|_| body.levels.len() == scope.level + 1) {
                scope.callee = Some((level.reads.len(), level.locals.len()));
            }
        }
        let arguments = call.arguments();
        let argument_types = self.expression_values(&arguments);
        if let (Some(on), Some(method)) = (forward_on, call.method_name()) {
            let forwarded = self.forwarded_arguments(&arguments, &argument_types);
            self.add_forwarded(ForwardedCall { on, method: method.text().to_string(), arguments: forwarded });
        }
        if let Some(callee) = callee {
            self.table_library_call(call, callee, arguments.len(), &argument_types);
        }
        // A method of one of several objects: which one it calls isn't known,
        // but each object's own method of that name gets what's passed
        if let (Some(base), Some(method), Some(FunctionRef::Unknown)) = (receiver, call.method_name(), callee.map(|c| c.function)) {
            self.pass_to_each_method(base, method.text(), &argument_types);
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
        // The definitions' functions don't list `self`, as `:` passes it, but
        // `s:format(x)` is `string.format(s, x)`: the string is its first
        // parameter, so the arguments come after it
        let string_receiver = is_method && !matches!(callee.function, FunctionRef::One(_))
            && receiver.is_some_and(|r| self.definitions.member_source(r) != r);
        let skipped = usize::from(string_receiver);
        let required: Vec<Option<Type>> = (0..arguments.len()).map(|i| function.mandated(i + skipped)).collect();
        // Where the call passes a template, if it takes one
        let template = function.parameters.iter()
            .position(|p| p.name.to_ascii_lowercase().contains("template"));
        let documented = function.returns.clone();
        let fresh_returns = function.fresh_returns.clone();
        let forwarded_calls = function.forwarded_calls.clone();
        let field_writes = function.field_writes.clone();
        let returned_fields = function.returned_fields.clone();
        // The callee's parameters as its body sees them, for its summaries
        let settled_parameters: Vec<Type> = match function.field_writes.is_empty() && function.forwarded_calls.is_empty() {
            true => Vec::new(),
            false => function.parameters.iter().map(|p| p.types).collect(),
        };
        let callee_method = function.method;
        // How far each argument is from the parameter it fills (see `pending_call`)
        let offset: isize = match (callee_method, is_method) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
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
                    Some(pending) => {
                        self.pending_calls.push(pending);
                        let index = self.pending_calls.len() - 1;
                        if let Some(level) = self.deps.get_mut().last_mut().and_then(|body| body.levels.last_mut()) {
                            level.pending.push(index);
                        }
                    }
                    None => self.apply(Write::CallArguments(id, parameter_types.clone())),
                }
                self.end_call_scope(Some(id));
                self.callee_found = self.callee_found();
                let mut returns = self.returns_of(id);
                for (slot, fresh) in fresh_returns.iter().enumerate() {
                    if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                        returns.values[slot] = self.fresh_object(call, slot, value, fresh, &parameter_types, &arguments, offset, None);
                    }
                }
                // A loop over this call gets what one over the function's
                // own return does
                if let Some(iterated) = self.known_function(FunctionRef::One(id)).and_then(|f| f.iterated.clone()) {
                    self.iterated.insert(self.location(call.syntax().text_range().start()), iterated);
                }
                returns
            }
            _ => self.select(callee, &arguments, &argument_types)
                .or_else(|| self.library_returns(callee, &arguments, &argument_types))
                .or_else(|| self.create_frame(call, callee, &arguments, &argument_types))
                .or_else(|| self.create_table(call, callee))
                .or_else(|| self.create_pool(call, callee, &arguments, &argument_types))
                .or_else(|| self.mix(call, callee, &arguments, &argument_types))
                .or_else(|| self.copy_table(call, callee, &arguments, &argument_types))
                .unwrap_or_else(|| {
                    // A definition's function returning a new table at each
                    // call, with arguments mixed in
                    let builtin = match callee.function { FunctionRef::Builtin(id) => Some(id), _ => None };
                    let mut returns = documented.clone();
                    for (slot, fresh) in fresh_returns.iter().enumerate() {
                        if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                            returns.values[slot] = self.fresh_object(call, slot, value, fresh, &argument_types, &arguments, 0, builtin);
                        }
                    }
                    returns
                }),
        };
        // What the function calls on what this call passes it. A definition's
        // function has its arguments by position
        let passed = if matches!(callee.function, FunctionRef::One(_)) { (&parameter_types, offset) } else { (&argument_types, 0) };
        // `self`, and the code giving it
        let self_value = match (is_method, callee_method) {
            (true, _) => receiver.map(|r| (r, call.callee())),
            (false, true) => Some((argument_types.get(0), arguments.first().and_then(copy_expression))),
            (false, false) => None,
        };
        if !field_writes.is_empty() || !forwarded_calls.is_empty() {
            let passing = self.passing(passed.0, &settled_parameters, self_value, &returns, &arguments, passed.1);
            let method = call.method_name().map(|name| name.text().to_string());
            self.apply_summaries(&field_writes, &forwarded_calls, method.as_deref().map(|name| (name, callee.function)), &passing, &mut vec![callee.function]);
        }
        let self_value = match (is_method, callee_method) {
            (true, _) => receiver,
            (false, true) => Some(argument_types.get(0)),
            (false, false) => None,
        };
        for (slot, field) in returned_fields.iter().enumerate() {
            let Some(field) = field else { continue };
            let on = match field.on {
                On::SelfValue => self_value,
                On::Parameter(p) => Some(passed.0.get(p)),
                On::Returned(_) => None,
            };
            let (Some(on), Some(&value)) = (on, returns.values.get(slot)) else { continue };
            // A table made in the files, or one the definitions give the field
            // (a global table like `NORMAL_FONT_COLOR`, with its `r`): not
            // what any table's field of that name may be
            let declared = self.definitions.field(self.definitions.member_source(on), &field.field).is_some();
            if on.named.identity().is_none() && !matches!(on.named, NamedRef::Several(_)) && !declared {
                continue
            }
            // What it returns isn't known yet: nothing it says narrows the field
            let held = self.field_read(on, &field.field);
            returns.values[slot] = if value.set == TypeSet::NEVER { held } else { narrowed_to(held, value) };
        }
        if let Some(receiver) = receiver.filter(|_| is_method) {
            self.callback_registry(receiver, callee, &arguments, &argument_types);
            if let Some(method) = call.method_name() {
                self.script_handler(receiver, callee, method.text(), &arguments, &argument_types);
            }
        }
        if let (Some(receiver), Some(method)) = (receiver, call.method_name()) {
            returns = self.object_method(call, receiver, method.text(), arguments.first(), template.and_then(|i| arguments.get(i)), &argument_types, returns);
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
    fn object_method(&mut self, call: &FunctionCall, receiver: Type, name: &str, kind: Option<&Expression>, template: Option<&Expression>, arguments: &Values, mut returns: Values) -> Values {
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
                // `CreateAnimation("Alpha")` makes the kind of animation it names
                let child = match name {
                    "CreateAnimation" => self.kind_named(kind, child),
                    _ => child,
                };
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
            let Some(named) = self.definitions.named_type(id) else { return };
            // A data table's members are all known too, and none are methods,
            // unless it's given ones that can't be named
            if !named.has_methods && !(self.definitions.has_fixed_fields(id) && !self.is_opened(prior, id)) {
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
    fn open(&mut self, ty: Type) {
        for object in ty.named.alternatives() {
            if let NamedRef::One(id) | NamedRef::Extended(id) = object {
                if self.definitions.has_fixed_fields(id) {
                    self.apply(Write::Opened(id));
                }
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
    fn check_called_field(&mut self, base: NamedRef, text: &str, range: TextRange, types: Type) {
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
    fn check_called_variable(&mut self, types: Type) {
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
    fn check_operand(&mut self, op: Operator, operand: Type, range: TextRange) {
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
    fn check_operands(&mut self, op: Operator, left: Type, right: Type, range: TextRange) {
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
    fn is_opened(&self, prior: &Analysis, id: NamedId) -> bool {
        self.note(Read::Opened(id));
        prior.opened_types.contains(&id) || self.layer.opened_types.contains(&id)
    }

    /// Reports reading field `name` (at `token`) of a data table whose fields
    /// the definitions give all of (`values`), when it has none of that name
    /// and no code in the files gives one of that name to a table like it.
    /// The addon may add its own fields, as the table is its to change
    fn check_field(&mut self, base: Type, token: &SyntaxToken, name: &str) {
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
        // A kind the function making it is passed: each call's own
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        if let Some((_, index)) = arguments.first().and_then(|kind| self.parameter_read(kind)).filter(|&(function, _)| Some(function) == caller) {
            self.kind_parameters.insert(at, index);
        }
        // The third argument, or nil without one
        self.apply(Write::Parent(at, argument_types.get(2)));
        self.add_lua_mixins(at, named, argument_types.get(2));
        let named = NamedRef::Instance { named, at, extended };
        Some(Values { values: vec![Type { named, ..Type::of(TypeSet::TABLE) }], rest: Type::NIL })
    }

    /// `table.create(n)`: a new empty table, as `{}` written at the call is
    fn create_table(&self, call: &FunctionCall, callee: Type) -> Option<Values> {
        if self.builtin("table.create") != Some(callee) {
            return None
        }
        let at = self.location(call.syntax().text_range().start());
        Some(Values { values: vec![Type::table(at)], rest: Type::NIL })
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
        Some(self.kind_named(argument, frame))
    }

    /// The kind of `base` a literal argument names (`"Button"`, `"Flipbook"`),
    /// ignoring case, else `base`
    fn kind_named(&self, argument: Option<&Expression>, base: NamedId) -> NamedId {
        match argument {
            Some(Expression::Literal(literal)) => literal.get_string()
                .and_then(|text| self.definitions.named_type_id_ignoring_case(text.trim().trim_matches(['"', '\''])))
                .filter(|&id| self.definitions.is_a(id, base))
                .unwrap_or(base),
            _ => base,
        }
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

    /// `registry:TriggerEvent("Name", ...)` and `registry:RegisterCallback("Name",
    /// callback, owner, ...)` on a table made in the files with Blizzard's
    /// `CallbackRegistryMixin`: each callback is called with its owner (a
    /// number made for it when there's none), what was registered after the
    /// owner, then what each `TriggerEvent` of that event passes
    fn callback_registry(&mut self, receiver: Type, callee: Type, arguments: &[Expression], argument_types: &Values) {
        let is = |w: &Walker, path: &str| callee.function != FunctionRef::None && w.builtin(path).is_some_and(|b| b.function == callee.function);
        let (trigger, register) = (is(self, "CallbackRegistryMixin.TriggerEvent"), is(self, "CallbackRegistryMixin.RegisterCallback"));
        if !trigger && !register {
            return
        }
        let Some(at) = receiver.named.identity() else { return };
        let Some(Expression::Literal(literal)) = arguments.first() else { return };
        let Some(event) = literal.get_string() else { return };
        let event = name_key(event.trim_matches(['"', '\'']));
        let after = |from: usize| Values { values: argument_types.values.iter().skip(from).copied().collect(), rest: argument_types.rest };
        if trigger {
            self.apply(Write::Triggered(at, event, after(1)));
            return
        }
        self.note(Read::Triggered(at, event));
        let prior = self.prior.and_then(|prior| prior.triggered.get(&(at, event)));
        let triggered = match (prior, self.layer.triggered.get(&(at, event))) {
            (Some(a), Some(b)) => a.union(b),
            (Some(found), None) | (None, Some(found)) => found.clone(),
            // Never triggered, so never called
            (None, None) => return,
        };
        let owner = match arguments.len() {
            0..=2 => Type::of(TypeSet::NUMBER),
            _ => {
                let owner = argument_types.get(2);
                match owner.set.contains(TypeSet::NIL) {
                    true => owner.without(TypeSet::NIL) | Type::of(TypeSet::NUMBER),
                    false => owner,
                }
            }
        };
        let mut values = vec![owner];
        let extras = after(3);
        // What was registered after the owner comes first, when how many
        // there are is known
        let rest = match arguments.len() > 3 && matches!(arguments.last(), Some(Expression::VarArgs(_) | Expression::FunctionCall(_))) {
            true => {
                values.extend(extras.values);
                Type::UNKNOWN
            }
            false => {
                values.extend(extras.values.iter().take(arguments.len().saturating_sub(3)).copied());
                values.extend(triggered.values);
                triggered.rest
            }
        };
        // A method's parameters don't count `self`, which the owner is
        let callback = argument_types.get(1);
        if self.known_function(callback.function).is_some_and(|f| f.method) {
            values.remove(0);
        }
        self.pass_to_callback(arguments.get(1), callback, Values { values, rest });
    }

    /// `frame:SetScript("OnEvent", handler)` or `HookScript`, with the game's
    /// method: the game calls the handler with the frame, then the arguments
    /// that kind of handler gets (see `xml::handler_parameters`)
    fn script_handler(&mut self, receiver: Type, callee: Type, method: &str, arguments: &[Expression], argument_types: &Values) {
        if !matches!(method, "SetScript" | "HookScript") || !matches!(callee.function, FunctionRef::Builtin(_)) {
            return
        }
        let Some(Expression::Literal(literal)) = arguments.first() else { return };
        let Some(script) = literal.get_string() else { return };
        let script = script.trim_matches(['"', '\'']);
        let mut values = vec![receiver];
        values.extend(crate::xml::handler_parameters(script).iter().map(|&(_, types)| Type::of(types)));
        // An event's payload, which isn't known
        let rest = if script == "OnEvent" { Type::UNKNOWN } else { Type::NIL };
        // A method's parameters don't count `self`, which the frame is
        let handler = argument_types.get(1);
        if self.known_function(handler.function).is_some_and(|f| f.method) {
            values.remove(0);
        }
        self.pass_to_callback(arguments.get(1), handler, Values { values, rest });
    }

    /// `a:m(...)` where `a` is one of several objects: what's passed goes to
    /// each object's own `m` that's known to be one function
    fn pass_to_each_method(&mut self, base: Type, method: &str, arguments: &Values) {
        if !matches!(base.named, NamedRef::Several(_) | NamedRef::Tables(_)) {
            return
        }
        for object in base.named.alternatives() {
            let receiver = Type { named: object, ..base };
            let FunctionRef::One(id) = self.field_read(receiver, method).function else { continue };
            let Some(takes_self) = self.known_function(FunctionRef::One(id)).map(|f| f.method) else { continue };
            let values = match takes_self {
                true => arguments.clone(),
                false => {
                    let mut values = vec![receiver];
                    values.extend(arguments.values.iter().copied());
                    Values { values, rest: arguments.rest }
                }
            };
            self.apply(Write::CallArguments(id, values));
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

    /// Whether a table or object was made by the function being walked: there,
    /// or new from a call there. Not a pool's object or a copy's table
    fn made_here(&self, at: TableId) -> bool {
        at.slot & (POOL_OBJECT | COPY) == 0 && at.file == self.file && self.function_ranges.last().is_some_and(|(range, _)| range.contains(at.offset))
    }

    /// The innermost function being walked that made a table or object (there,
    /// or new from a call there), or a pool whose object it is, as its number
    /// in the walk
    fn maker(&self, at: TableId) -> Option<usize> {
        if at.file != self.file || at.slot & COPY != 0 {
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
    /// What `expression`, in a function, is that a `FieldWrite` can be on: its
    /// `self`, a parameter, or a table it made (of type `ty`)
    fn write_target(&self, expression: &Expression, ty: Type) -> Option<Result<On, TableId>> {
        if self.function == MAIN_CHUNK {
            return None
        }
        let caller = self.function_ranges.last().map(|&(_, function)| function)?;
        if let Expression::Identifier(identifier) = expression {
            if let Some(name) = identifier.simple_name() {
                let self_binding = self.self_bindings.last().copied().flatten();
                if self_binding.is_some() && self.local_binding(name.text()) == self_binding {
                    return Some(Ok(On::SelfValue))
                }
            }
        }
        if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| function == caller) {
            return Some(Ok(On::Parameter(index)))
        }
        let at = ty.named.identity().filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at)))?;
        Some(Err(at))
    }

    /// What `value` is in terms of the function's parameters, if it's one,
    /// or `p or v` with `p` one and `v` a literal or one
    fn value_parts(&self, value: &Expression) -> Option<Vec<Forwarded>> {
        let caller = self.function_ranges.last().map(|&(_, function)| function)?;
        let parameter = |e: &Expression| self.parameter_read(e).filter(|&(function, _)| function == caller).map(|(_, index)| index);
        let literal = |e: &Expression| -> Option<Type> {
            let Expression::Literal(literal) = e else { return None };
            let set = if literal.get_number().is_some() { TypeSet::NUMBER }
                else if literal.get_string().is_some() { TypeSet::STRING }
                else { match literal.get_bool().as_deref() { Some("true") => TypeSet::TRUE, Some("false") => TypeSet::FALSE, _ => return None } };
            Some(Type::of(set))
        };
        if let Some(index) = parameter(value) {
            return Some(vec![Forwarded::Parameter(index)])
        }
        let Expression::BinaryExpression(binary) = value else { return None };
        let parts = binary.flatten();
        let [crate::ast::BinaryPart::Operand(left), crate::ast::BinaryPart::Operator(Operator::Or), crate::ast::BinaryPart::Operand(right)] = parts.as_slice() else { return None };
        let left = Forwarded::Truthy(parameter(left)?);
        let right = match (parameter(right), literal(right)) {
            (Some(index), _) => Forwarded::Parameter(index),
            (None, Some(ty)) => Forwarded::Value(ty),
            (None, None) => return None,
        };
        Some(vec![left, right])
    }

    /// Notes `base.field = value` in a function, as a `FieldWrite`, when it
    /// gives what it's passed to `self`, a parameter or a table it makes
    fn note_field_write(&mut self, base: &Expression, ty: Type, field: &str, value: Option<&Expression>) {
        let Some(value) = value.and_then(|value| self.value_parts(value)) else { return };
        let Some(on) = self.write_target(base, ty) else { return };
        self.add_field_write(on, field.to_string(), value);
    }

    fn add_field_write(&mut self, on: Result<On, TableId>, field: String, value: Vec<Forwarded>) {
        self.deps_to_signature();
        if let Some(writes) = self.field_writes.last_mut() {
            let write = (on, field, value);
            if !writes.contains(&write) {
                writes.push(write);
            }
        }
    }

    /// The field of `self` or a parameter `expression` reads, if it's one
    fn returned_field(&self, expression: &Expression) -> Option<ReturnedField> {
        let Expression::Identifier(identifier) = expression else { return None };
        let (base, field) = field_access(identifier.syntax())?;
        match self.write_target(&base, Type::default())? {
            Ok(on @ (On::SelfValue | On::Parameter(_))) => Some(ReturnedField { on, field: field.text().to_string() }),
            _ => None,
        }
    }

    /// The origins of a call's arguments and `self`, in the terms of the
    /// function making it (see `Passing`): from its argument `expressions`,
    /// each `offset` from the parameter it fills, to a callee with parameters
    /// of types `parameters`
    fn passing(&self, passed: &Values, parameters: &[Type], self_value: Option<(Type, Option<Expression>)>, returns: &Values, expressions: &[Expression], offset: isize) -> Passing {
        let named = parameters.len();
        let caller = self.function_ranges.last().map(|&(_, function)| function).filter(|_| self.function != MAIN_CHUNK);
        let expression = |position: usize| usize::try_from(position as isize - offset).ok().and_then(|i| expressions.get(i));
        let positions = passed.values.len().max(named);
        let origins = (0..positions)
            .map(|i| expression(i).and_then(|e| self.parameter_read(e)).filter(|&(function, _)| Some(function) == caller).map(|(_, index)| index))
            .collect();
        // Its `...` passed as the last argument, filling those past the named
        let rest_origin = caller.is_some() && usize::try_from(named as isize - offset).ok()
            .is_some_and(|i| i + 1 == expressions.len() && matches!(expressions.get(i), Some(Expression::VarArgs(_))));
        let self_origin = caller.and(self_value.as_ref())
            .and_then(|(ty, e)| e.as_ref().and_then(|e| self.write_target(e, *ty)));
        Passing { passed: passed.clone(), origins, rest_origin, named, parameters: parameters.to_vec(), self_value: self_value.map(|(ty, _)| ty), self_origin, returns: returns.clone() }
    }

    /// Applies what a callee's summaries say a call to it does, for the
    /// call `call` describes: the fields it gives (`field_writes`) and the
    /// methods it calls on what it's passed (`forwarded_calls`). `method` the
    /// method called, if it's
    /// a method call; `within` the functions whose summaries these come from,
    /// stopping a method calling itself
    fn apply_summaries(&mut self, field_writes: &[FieldWrite], forwarded_calls: &[ForwardedCall], method: Option<(&str, FunctionRef)>, call: &Passing, within: &mut Vec<FunctionRef>) {
        for write in field_writes {
            self.give_field(write, method, call);
        }
        for forwarded in forwarded_calls {
            self.forward_call(forwarded, call, within);
        }
    }

    /// What `on` is in the calling function's terms, for passing a summary on
    fn origin_of(&self, on: On, call: &Passing) -> Option<Result<On, TableId>> {
        let made = |ty: Type| ty.named.identity().filter(|&at| self.made_here(at) && !self.kept_outside.last().is_some_and(|kept| kept.contains(&at))).map(Err);
        match on {
            On::SelfValue => call.self_origin,
            On::Parameter(k) => call.origin(k).map(|j| Ok(On::Parameter(j))).or_else(|| made(call.passed.get(k))),
            On::Returned(slot) => made(call.returns.get(slot)),
        }
    }

    /// The field `write` stands for. Given to the table it's on (of several
    /// `self` may be, those whose method is `method`), and passed on as the
    /// calling function's own when that's its `self`, a parameter or a table
    /// it makes. Not a write to the calling function's own `self` by a method
    /// it calls on it, as that call is passed on itself (a `ForwardedCall` on
    /// `self`), to call whichever method `self` has at each call
    fn give_field(&mut self, write: &FieldWrite, method: Option<(&str, FunctionRef)>, call: &Passing) {
        let on = match write.on {
            On::SelfValue => match call.self_value {
                Some(ty) => ty,
                None => return,
            },
            On::Parameter(k) => call.passed.get(k),
            On::Returned(slot) => call.returns.get(slot),
        };
        let origin = self.origin_of(write.on, call);
        let passed_on_by_call = origin == Some(Ok(On::SelfValue)) && write.on == On::SelfValue;
        let target = origin.filter(|_| !passed_on_by_call);
        let argument = |i: usize| call.argument(i);
        // Given here, but for what the caller passes on of what it was passed
        // when the write is passed on too: its type here isn't what each call
        // to the caller passes, which the caller's own write gives, as a
        // parameter passed straight on is settled by its calls (see
        // `pending_call`)
        let passed_on = |part: Forwarded| match part {
            Forwarded::Parameter(i) | Forwarded::Truthy(i) => call.origin(i).is_some(),
            Forwarded::Rest => call.rest_origin,
            Forwarded::Value(_) => false,
        };
        let value = write.value.iter()
            .filter(|&&part| origin.is_none() || !passed_on(part))
            .map(|&part| match part {
                Forwarded::Parameter(i) => argument(i),
                Forwarded::Truthy(i) => argument(i).truthy(),
                Forwarded::Value(ty) => ty,
                Forwarded::Rest => call.passed.rest,
            })
            // Nothing known of it, which says nothing of what the table holds:
            // the function's own write gives it what its body knows
            .filter(|&ty| !says_nothing(ty))
            .reduce(Type::union);
        // Only a table that's known, not a field name of every table
        if let Some(value) = value.filter(|_| on.named.identity().is_some() || matches!(on.named, NamedRef::Several(_))) {
            let objects: Vec<TableId> = match (write.on, method, on.named) {
                (On::SelfValue, Some((name, function)), NamedRef::Several(_)) => on.named.alternatives().into_iter()
                    .filter(|&object| self.field_read(Type { named: object, ..on }, name).function == function)
                    .filter_map(|object| object.identity())
                    .collect(),
                _ => on.named.identity().into_iter().chain(self.several_objects(on)).collect(),
            };
            for at in objects {
                self.apply(Write::Field(at, write.field.clone(), value));
            }
        }
        let Some(target) = target else { return };
        let parts: Vec<Forwarded> = write.value.iter()
            .flat_map(|&part| call.in_caller_terms(part, &argument))
            .collect();
        self.add_field_write(target, write.field.clone(), parts);
    }

    /// For `receiver:m()` in a function, with `receiver` of type `base` and
    /// `m` of type `method`: what the method may be on, for a `ForwardedCall`,
    /// if it's the function's `self`, one of its parameters or a table it
    /// made with parameters mixed in. The method only one of the files' own,
    /// or one it can't tell, or one of the definitions' with summaries of its
    /// own: not a WoW object's method, which does nothing more on each call
    fn forwardable_receiver(&self, receiver: Option<&Expression>, base: Type, method: Type) -> Option<Vec<On>> {
        if self.function == MAIN_CHUNK {
            return None
        }
        let summarised = match method.function {
            FunctionRef::One(_) | FunctionRef::Unknown => true,
            FunctionRef::Builtin(_) => self.known_function(method.function).is_some_and(|info| !info.field_writes.is_empty() || !info.forwarded_calls.is_empty()),
            FunctionRef::None => false,
        };
        if !summarised {
            return None
        }
        match self.write_target(receiver?, base)? {
            Ok(on @ (On::SelfValue | On::Parameter(_))) => Some(vec![on]),
            Ok(On::Returned(_)) => None,
            Err(at) => {
                let mixed = self.parameter_mixins.get(&at).filter(|mixed| !mixed.is_empty())?;
                Some(mixed.iter().map(|&p| On::Parameter(p)).collect())
            }
        }
    }

    /// Notes that the function being walked makes `call` for each call to it,
    /// once: a recursive function passing its parameter on to itself makes
    /// the call it already makes, which would otherwise be added again each
    /// walk, never settling
    fn add_forwarded(&mut self, call: ForwardedCall) {
        self.deps_to_signature();
        if let Some(calls) = self.forwarded.last_mut() {
            if !calls.contains(&call) {
                calls.push(call);
            }
        }
    }

    /// The arguments of a call a `ForwardedCall` stands for: the function's
    /// parameters passed on, its `...`, and other values
    fn forwarded_arguments(&self, expressions: &[Expression], values: &Values) -> Vec<Forwarded> {
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        let mut forwarded = Vec::new();
        for (i, expression) in expressions.iter().enumerate() {
            if i + 1 == expressions.len() && matches!(expression, Expression::VarArgs(_)) {
                forwarded.push(Forwarded::Rest);
                break
            }
            match self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                Some((_, index)) => forwarded.push(Forwarded::Parameter(index)),
                None => forwarded.push(Forwarded::Value(values.get(i))),
            }
        }
        forwarded
    }

    /// The call `forwarded` stands for, made by the call `call` describes. It
    /// calls the method of the first of what it may be on (the last first)
    /// that has it, as known there, with its own summaries, and is passed on
    /// in turn when what it's on is the calling function's `self` or a
    /// parameter, which then makes the call for each of its own calls
    fn forward_call(&mut self, forwarded: &ForwardedCall, call: &Passing, within: &mut Vec<FunctionRef>) {
        // A method called on the calling function's own `self` that calls
        // this on its `self`: the caller's own summary has the call on `self`
        // (see `forwardable_receiver`), which each call to it resolves on
        // what `self` is there, this with it. Copying it up too would make
        // every method's summary every method it can reach
        if call.self_origin == Some(Ok(On::SelfValue)) && forwarded.on.contains(&On::SelfValue) {
            return
        }
        // Passed on by the function calling, when it's on its `self` or a
        // parameter: then each call to it makes the call, on what that call
        // passes, so there's nothing to do here
        if self.function != MAIN_CHUNK {
            let on: Vec<On> = forwarded.on.iter()
                .filter_map(|&on| match self.origin_of(on, call) {
                    Some(Ok(on @ (On::SelfValue | On::Parameter(_)))) => Some(on),
                    _ => None,
                })
                .collect();
            if !on.is_empty() {
                let arguments = forwarded.arguments.iter().flat_map(|&part| call.in_caller_terms(part, &|i| call.argument(i))).collect();
                self.add_forwarded(ForwardedCall { on, method: forwarded.method.clone(), arguments });
                return
            }
        }
        let Some((receiver, on, function)) = forwarded.on.iter().rev().find_map(|&on| {
            let receiver = match on {
                On::SelfValue => call.self_value?,
                On::Parameter(k) => call.passed.get(k),
                On::Returned(_) => return None,
            };
            match self.field_read(receiver, &forwarded.method).function {
                function @ (FunctionRef::One(_) | FunctionRef::Builtin(_)) => Some((receiver, on, function)),
                _ => None,
            }
        }) else { return };
        // What it passes the method
        let mut values = Vec::new();
        let mut rest = Type::NIL;
        for &part in &forwarded.arguments {
            match part {
                Forwarded::Parameter(i) => values.push(call.argument(i)),
                Forwarded::Truthy(i) => values.push(call.argument(i).truthy()),
                Forwarded::Value(ty) => values.push(ty),
                Forwarded::Rest => {
                    values.extend(call.passed.values.iter().skip(call.named).copied());
                    rest = call.passed.rest;
                }
            }
        }
        let Some(info) = self.known_function(function) else { return };
        let (method_takes_self, named) = (info.method, info.parameters.len());
        let (field_writes, forwarded_calls) = (info.field_writes.clone(), info.forwarded_calls.clone());
        let parameters: Vec<Type> = info.parameters.iter().map(|p| p.types).collect();
        if let FunctionRef::One(id) = function {
            // Only what's known: this adds to what the body's own call passes
            // (or calls the method's file sees), and what says nothing would
            // swamp it
            let mut passed: Vec<Type> = values.iter().map(|&ty| if says_nothing(ty) { Type::default() } else { ty }).collect();
            // Called with `:`, so `self` is separate unless the function takes it
            if !method_takes_self {
                passed.insert(0, receiver);
            }
            let rest = if says_nothing(rest) { Type::default() } else { rest };
            self.apply(Write::CallArguments(id, Values { values: passed, rest }));
        }
        // What the method does, in turn: deep enough for real chains of
        // methods, and never one already being applied
        if within.len() >= 6 || within.contains(&function) || (field_writes.is_empty() && forwarded_calls.is_empty()) {
            return
        }
        let mut origins: Vec<Option<usize>> = forwarded.arguments.iter()
            .take_while(|part| !matches!(part, Forwarded::Rest))
            .map(|&part| match part {
                Forwarded::Parameter(i) => call.origin(i),
                _ => None,
            })
            .collect();
        let rest_origin = forwarded.arguments.last() == Some(&Forwarded::Rest) && call.rest_origin;
        origins.resize(origins.len().max(values.len()), None);
        let self_origin = match on {
            On::SelfValue => call.self_origin,
            on => self.origin_of(on, call),
        };
        let inner = Passing {
            passed: Values { values, rest },
            origins,
            rest_origin: rest_origin && forwarded.arguments.len() - 1 == named,
            named,
            parameters,
            self_value: Some(receiver),
            self_origin,
            returns: Values::none(),
        };
        within.push(function);
        self.apply_summaries(&field_writes, &forwarded_calls, Some((&forwarded.method, function)), &inner, within);
        within.pop();
    }

    /// `expressions` are the call's arguments, each `offset` from the
    /// parameter it fills (see `pending_call`)
    /// `function` is the definitions' function called, if it's one, whose
    /// tables are of a family (see `family`)
    fn fresh_object(&mut self, call: &FunctionCall, slot: usize, value: Type, fresh: &FreshObject, arguments: &Values, expressions: &[Expression], offset: isize, function: Option<BuiltinId>) -> Type {
        let here = self.location(call.syntax().text_range().start());
        let at = function.and_then(|function| family_member(here, function, slot))
            .unwrap_or(Location { slot: slot as u32 + 1, ..here });
        // Passing on a parameter of the function calling, so each call to that
        // mixes in what it passes, as for `CreateFromMixins(parameter)`
        let caller = self.function_ranges.last().map(|&(_, function)| function);
        for &position in &fresh.mixed_parameters {
            let Some(expression) = usize::try_from(position as isize - offset).ok().and_then(|i| expressions.get(i)) else { continue };
            if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                self.apply(Write::ParameterMixin(at, index));
            }
        }
        // A frame of the kind the call passes, if the function makes one of
        // the kind it's passed: a literal kind names it, and a parameter of
        // the function calling is passed on, for each call to that
        let kind_expression = fresh.kind_parameter
            .and_then(|position| usize::try_from(position as isize - offset).ok())
            .and_then(|i| expressions.get(i));
        let passed_kind = kind_expression.and_then(|expression| {
            if let Some((_, index)) = self.parameter_read(expression).filter(|&(function, _)| Some(function) == caller) {
                self.kind_parameters.insert(at, index);
            }
            let frame = self.definitions.named_type_id("Frame")?;
            Some(self.kind_named(Some(expression), frame)).filter(|&kind| kind != frame)
        });
        let named = match value.named {
            NamedRef::Table(_) => NamedRef::Table(at),
            NamedRef::Instance { named, extended, .. } => NamedRef::Instance { named: passed_kind.unwrap_or(named), at, extended },
            // A definition's `table`
            NamedRef::None | NamedRef::Unknown if value.set.contains(TypeSet::TABLE) && fresh.made.is_none() => NamedRef::Table(at),
            // A definition's table of the type its mixins make (see
            // `Definitions::add_mixin_types`): one of that type
            NamedRef::One(named) if fresh.made.is_none() => {
                let extended = !self.definitions.named_type(named).is_some_and(|t| t.has_methods);
                NamedRef::Instance { named, at, extended }
            }
            other => return Type { named: other, ..value },
        };
        // What the function made, for the fields it set there
        let mut mixins: Vec<Type> = value.named.identity().map(|_| value).into_iter().collect();
        // Unless the type it is has them (see `Definitions::add_mixin_types`)
        if !matches!(named, NamedRef::Instance { .. }) {
            mixins.extend(fresh.mixin_globals.iter().filter_map(|name| self.definitions.global(name)));
        }
        mixins.extend(fresh.mixed_parameters.iter().map(|&i| arguments.get(i)));
        for mixin in &mixins {
            self.record_mixed_into(*mixin, named);
        }
        self.apply(Write::MixinsReplaced(at, mixins));
        for (name, ty) in &fresh.fields {
            self.apply(Write::Field(at, name.clone(), *ty));
        }
        if let Some(parent) = fresh.made.and_then(|made| self.known_parent(made)) {
            self.apply(Write::Parent(at, parent));
        }
        // A pool: its objects are new for the call too, with the call's
        // arguments for the parameters its init or reset function mixed in
        if let (Some(mixed), Some(inner)) = (&fresh.pool_mixed_parameters, fresh.made.and_then(|made| self.pool_object(made))) {
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
        if !mixins.is_empty() {
            self.open(object);
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

    /// `CopyTable(t)` (and `CopyTableSafe`): a new table, made at the call,
    /// with what `t` (or any of the tables it may be) holds: its fields, contents and mixins, and a copy of each
    /// table among them, as the copy is deep. `CopyTable(t, true)` copies
    /// only `t`, sharing the tables it holds
    fn copy_table(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let safe = self.definitions.global("CopyTableSafe") == Some(callee);
        if !safe && self.definitions.global("CopyTable") != Some(callee) {
            return None
        }
        let source = arguments.get(0);
        // Any of several tables: a copy of whichever it is, so what any holds
        let sources = match source.named {
            NamedRef::Table(_) | NamedRef::Tables(_) => source.named.identities(),
            _ => return None,
        };
        let shallow = matches!(expressions.get(1), Some(Expression::Literal(literal)) if literal.get_bool().as_deref() == Some("true"));
        let at = self.location(call.syntax().text_range().start());
        for from in sources {
            self.copy_into(from, at, shallow, &mut vec![from]);
        }
        let mut copy = Type::table(at);
        // `CopyTableSafe(nil)` gives nil
        if safe && source.set.intersection(TypeSet::NIL | TypeSet::FALSE) != TypeSet::NEVER {
            copy |= Type::NIL;
        }
        Some(Values { values: vec![copy], rest: Type::NIL })
    }

    /// Gives table `to` what table `from` holds, as the previous walk (or a
    /// dependency) knows it, each table in it copied too unless `shallow`.
    /// `within` are the tables being copied, which a table holding one of
    /// them shares rather than copying forever
    fn copy_into(&mut self, from: TableId, to: TableId, shallow: bool, within: &mut Vec<TableId>) {
        self.apply(Write::CopiedFrom(to, from));
        let Some(prior) = self.prior else { return };
        self.note(Read::Fields(from));
        let mut fields: Vec<(String, Type)> = Vec::new();
        for (name, &ty) in [prior.fields.get(&from), self.layer.fields.get(&from)].into_iter().flatten().flatten() {
            match fields.iter_mut().find(|(n, _)| n == name) {
                Some((_, found)) => *found |= ty,
                None => fields.push((name.clone(), ty)),
            }
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        // A copy into a saved table leaves out values of unknown shape, as a
        // write does (see `stored_in`). A profile copied into itself would
        // otherwise keep one forever once it got in
        for (name, ty) in fields {
            let ty = self.copied(ty, to, &name, shallow, within);
            if let Some(ty) = self.stored_in(to, ty) {
                self.apply(Write::Field(to, name, ty));
            }
        }
        self.note(Read::Entries(from));
        if let Some(entries) = prior.entries.get(&from).or_else(|| self.layer.entries.get(&from)).cloned() {
            let values = self.copied(entries.values, to, "[]", shallow, within);
            if let Some(values) = self.stored_in(to, values) {
                self.apply(Write::Entry(to, entries.keys, values));
            }
        }
        // Mixins' fields were copied into it, so they're the copy's too
        self.note(Read::Mixins(from));
        let mixins = prior.mixins.get(&from).or_else(|| self.layer.mixins.get(&from)).cloned().unwrap_or_default();
        for mixin in mixins {
            self.apply(Write::MixinAdded(to, mixin));
        }
    }

    /// What a copy into table `into` holds under `key` for a value of type
    /// `ty`: a copy of the table it is, made in `into`, unless `shallow`
    fn copied(&mut self, ty: Type, into: TableId, key: &str, shallow: bool, within: &mut Vec<TableId>) -> Type {
        let NamedRef::Table(nested) = ty.named else { return ty };
        // Deep enough for any real table, and a table holding itself is shared
        if shallow || nested.slot & PSEUDO != 0 || within.contains(&nested) || within.len() > 8 {
            return ty
        }
        let place = name_key(&format!("{}/{key}", into.slot)) as u32 & (COPY - 1);
        let copy = Location { slot: COPY | place, ..into };
        within.push(nested);
        self.copy_into(nested, copy, false, within);
        within.pop();
        Type { named: NamedRef::Table(copy), ..ty }
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
    fn stored_in(&self, at: TableId, value: Type) -> Option<Type> {
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
    fn index_read(&self, base: Type, key: Type) -> Type {
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
    fn entries_of_any(&self, prior: &Analysis, tables: &[TableId]) -> Option<Entries> {
        tables.iter().filter_map(|&at| self.entries_of(prior, at))
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })
    }

    /// Every key and value any of `tables` holds (see `contents_of`)
    fn contents_of_any(&self, prior: &Analysis, tables: &[TableId]) -> Option<Entries> {
        tables.iter().filter_map(|&at| self.contents_of(prior, at))
            .reduce(|a, b| Entries { keys: a.keys | b.keys, values: a.values | b.values })
    }

    /// What a table holds beyond its named fields: here, and from a dependency
    fn entries_of(&self, prior: &Analysis, at: TableId) -> Option<Entries> {
        self.entries_within(prior, at, 0)
    }

    fn entries_within(&self, prior: &Analysis, at: TableId, depth: usize) -> Option<Entries> {
        // A stand-in: what any table it stands for holds, and what's put
        // through it
        if is_stand_in(at) && depth <= 16 {
            self.note(Read::StandIn(at));
            let all: Vec<Entries> = self.stand_in_members(at).into_iter()
                .filter_map(|member| self.entries_within(prior, member, depth + 1))
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
        // given is only some of what the table may hold
        if at.slot & PSEUDO != 0 {
            let anything = Type::UNKNOWN.without(TypeSet::NIL);
            return Some(Entries { keys: anything, values: anything })
        }
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
        // On the first walk, nothing yet: what the loop gives grows from
        // there, as a field read's does
        let Some(prior) = self.prior else {
            let nothing = Values { values: vec![Type::default(), Type::default()], rest: Type::NIL };
            self.iterated.insert(self.location(call.syntax().text_range().start()), nothing);
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
            Operation::Binary(op, left_operation, right_operation) => {
                let left = self.operation(left_operation);
                let right = self.operation(right_operation);
                let range = left_operation.range().cover(right_operation.range());
                self.check_operands(*op, left, right, range);
                combine(*op, left, right)
            }
        }
    }

    /// Runs `f` with locals narrowed to the types given. Afterwards each is
    /// what it was before, with anything `f` assigned it, as if never narrowed
    fn with_narrowing<R>(&mut self, narrowed: &[(Target, Type)], f: impl FnOnce(&mut Self) -> R) -> R {
        let saved: Vec<(BindingId, Type)> = narrowed.iter()
            .filter_map(|(target, _)| match target { Target::Local(id) => Some((*id, self.current[*id])), Target::Field(..) => None })
            .collect();
        let (saved_fields, epoch) = (self.narrowed_fields.clone(), self.field_epoch);
        for (target, ty) in narrowed {
            self.narrow(target, *ty);
        }
        let result = f(self);
        for (id, before) in saved {
            self.set_current(id, self.current[id] | before);
        }
        // As before, unless `f` may have changed fields
        if self.field_epoch == epoch {
            self.narrowed_fields = saved_fields;
        } else {
            self.narrowed_fields.clear();
        }
        result
    }

    /// Narrows the locals among `narrowed` from here. Inside an `if`, which
    /// joins what its branches leave, so they aren't put back after
    fn narrow_locals(&mut self, narrowed: &[(Target, Type)]) {
        for (target, ty) in narrowed {
            if let Target::Local(_) = target {
                self.narrow(target, *ty);
            }
        }
    }

    /// The condition of a branch of the `if` being walked has been: what it
    /// did happens whichever branch is taken after it
    fn conditions_walked(&mut self) {
        let Some(join) = self.joins.last() else { return };
        let changed: Vec<BindingId> = join.before.keys().copied().collect();
        let held: Vec<(BindingId, Type)> = changed.into_iter().map(|id| (id, self.current[id])).collect();
        if let Some(join) = self.joins.last_mut() {
            join.after_conditions.extend(held);
        }
    }

    /// A branch of an `if`
    fn branch_block(&mut self, block: Option<Block>) {
        if let Some(block) = block {
            self.push_scope(true, block.syntax().text_range().end());
            if let Some(scope) = self.scopes.last_mut() {
                scope.joined = true;
            }
            self.statements(&block);
            self.pop_scope();
        }
    }

    /// The end of a branch of the `if` being walked: what it leaves, if it
    /// goes on past the `if` (`goes_on`), joins `ends`, and the next branch
    /// starts from what the `if` started with
    fn end_branch(&mut self, ends: &mut HashMap<BindingId, Type>, goes_on: bool) {
        let Some(join) = self.joins.last() else { return };
        let before: Vec<(BindingId, Type)> = join.before.iter()
            .map(|(&id, &(ty, _))| (id, join.after_conditions.get(&id).copied().unwrap_or(ty)))
            .collect();
        if goes_on {
            for &(id, _) in &before {
                *ends.entry(id).or_default() |= self.current[id];
            }
            if let Some(join) = self.joins.last_mut() {
                join.fell_through += 1;
            }
        }
        for (id, ty) in before {
            self.set_current(id, ty);
        }
    }

    /// Takes `target` to be of type `ty` from here
    fn narrow(&mut self, target: &Target, ty: Type) {
        match target {
            Target::Local(id) => self.set_current(*id, ty),
            Target::Field(root, path) => {
                match self.narrowed_fields.iter_mut().find(|(r, p, _)| r == root && p == path) {
                    Some((_, _, existing)) => *existing = ty,
                    None => self.narrowed_fields.push((*root, path.clone(), ty)),
                }
            }
        }
    }

    /// Something may have changed any table's fields: none stay narrowed
    fn forget_fields(&mut self) {
        self.narrowed_fields.clear();
        self.field_epoch += 1;
    }

    /// `e` as a local of the current function and the field names from it,
    /// for `x` (no names) or `x.a.b`
    fn field_path(&self, e: &Expression) -> Option<(BindingId, Vec<String>)> {
        let Expression::Identifier(identifier) = e else { return None };
        if let Some(name) = identifier.simple_name() {
            // In the main chunk, a global is followed as it's assigned, as
            // `if G == nil then G = {} end` needs, so it narrows like a local
            let id = match self.local_binding(name.text()) {
                Some(id) => id,
                None if self.function == MAIN_CHUNK => *self.globals.get(name.text())?,
                None => return None,
            };
            return (self.bindings[id].function == self.function).then(|| (id, Vec::new()))
        }
        let (base, field) = field_access(identifier.syntax())?;
        let (id, mut path) = self.field_path(&base)?;
        path.push(field.text().to_string());
        Some((id, path))
    }

    /// The narrowed type of the field `e` reads, if it's narrowed here
    fn narrowed_field(&self, e: &Expression) -> Option<Type> {
        if self.narrowed_fields.is_empty() {
            return None
        }
        let (root, path) = self.field_path(e)?;
        self.narrowed_fields.iter().find(|(r, p, _)| *r == root && *p == path).map(|(_, _, ty)| *ty)
    }

    /// A local of the current function named by `expression`, and its type
    /// here. Upvalues aren't narrowed, as a closure can run at any time
    /// Or a field reached from one by names, while nothing changes it (see
    /// `narrowed_fields`)
    fn narrowable(&self, expression: &Expression) -> Option<(Target, Type)> {
        let (root, path) = self.field_path(expression)?;
        // A global the files don't assign is its preset, as a read finds
        let held = self.builtin_global(root).unwrap_or(self.current[root]);
        if path.is_empty() {
            return Some((Target::Local(root), held))
        }
        // Its type here: as narrowed, or read from what it's on
        let mut ty = held;
        for (i, name) in path.iter().enumerate() {
            ty = match self.narrowed_fields.iter().find(|(r, p, _)| *r == root && p[..] == path[..=i]) {
                Some((_, _, narrowed)) => *narrowed,
                None => self.field_read(ty, name),
            };
        }
        Some((Target::Field(root, path), ty))
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
                Some((target, ty)) => Narrowing { when_true: vec![(target.clone(), ty.truthy())], when_false: vec![(target, ty.falsy())] },
                None => Narrowing::default(),
            },
        }
    }

    fn operation_narrowing(&self, operation: &Operation) -> Narrowing {
        match operation {
            Operation::Operand(e) => self.narrowing(e),
            // Both true; false says nothing about which wasn't
            // What the left proves of fields doesn't hold past a call on the right
            Operation::Binary(Operator::And, left, right) => Narrowing {
                when_true: both(self.left_narrowing(left, right).when_true, self.operation_narrowing(right).when_true),
                when_false: Vec::new(),
            },
            Operation::Binary(Operator::Or, left, right) => Narrowing {
                when_true: Vec::new(),
                when_false: both(self.left_narrowing(left, right).when_false, self.operation_narrowing(right).when_false),
            },
            Operation::Binary(op @ (Operator::Equals | Operator::NotEquals), left, right) => {
                let (Operation::Operand(left), Operation::Operand(right)) = (&**left, &**right) else { return Narrowing::default() };
                let narrowing = self.comparison(left, right).or_else(|| self.comparison(right, left)).unwrap_or_default();
                if *op == Operator::NotEquals { narrowing.negated() } else { narrowing }
            }
            _ => Narrowing::default(),
        }
    }

    /// What the left of an `and`/`or` proves once its right has run: nothing of
    /// fields when the right may call a function, which could change them
    fn left_narrowing(&self, left: &Operation, right: &Operation) -> Narrowing {
        let narrowing = self.operation_narrowing(left);
        match right.syntax_nodes().iter().any(|node| may_call(self, node)) {
            true => narrowing.locals_only(),
            false => narrowing,
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
            let (target, ty) = self.narrowable(tested)?;
            return Some(Narrowing { when_true: vec![(target.clone(), Type::NIL)], when_false: vec![(target, ty.without(TypeSet::NIL))] })
        }
        // `type(x)`, while `type` is the library's
        let Expression::FunctionCall(call) = tested else { return None };
        if !self.calls_library_global(call, "type") {
            return None
        }
        let [argument] = call.arguments().try_into().ok()?;
        let (target, ty) = self.narrowable(&argument)?;
        let set = type_name_set(string_field_name(&literal.get_string()?)?)?;
        Some(Narrowing { when_true: vec![(target.clone(), intersect(ty, set))], when_false: vec![(target, ty.without(set))] })
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
/// An `if` statement being walked: what each older variable it changed held
/// before, and how many of its branches had gone on past it by then
#[derive(Default)]
struct Join {
    /// The bindings made before it, the only ones that matter after it
    bindings: usize,
    before: HashMap<BindingId, (Type, usize)>,
    /// What each holds after the conditions walked so far, which run
    /// whichever branch is taken, so each later branch starts from it
    after_conditions: HashMap<BindingId, Type>,
    /// Branches walked so far that go on past it
    fell_through: usize,
}

/// The fields among narrowed targets
fn fields_only(narrowed: &[(Target, Type)]) -> Vec<(Target, Type)> {
    narrowed.iter().filter(|(target, _)| matches!(target, Target::Field(..))).cloned().collect()
}

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

/// How the definitions name a function defined with a name, like
/// `Mixin.Method` for `function Mixin:Method()`, or `f` for `local function f()`
fn function_path(def: &FunctionDefinition) -> Option<String> {
    if let Some(name) = def.local_name() {
        return Some(name.text().to_string())
    }
    let identifier = def.identifier()?;
    let path: String = identifier.syntax().text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
    Some(path.replace(':', "."))
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
        // The right side only runs if the left can be falsy (`or`) or truthy
        // (`and`). A left side not known yet may be, so `x or default` is the
        // default until it is: `t.list = t.list or {}` gives a table, where
        // nothing would go round to the read and stay nothing for good
        Operator::Or if left.set.falsy() == TypeSet::NEVER && left.set != TypeSet::NEVER => left,
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

    /// With records kept: the analysis, the table each of `tables` names
    /// holds, and the function each of `functions` holds
    fn analyse_keeping(source: &str) -> Analysis {
        keeping_records(|| analyse(source))
    }

    fn table_of(analysis: &Analysis, name: &str) -> TableId {
        analysis.bindings.iter().find(|b| &*b.name == name).and_then(|b| b.types.named.identity()).expect("a table")
    }

    fn function_of(analysis: &Analysis, name: &str) -> FunctionId {
        analysis.bindings.iter().find(|b| &*b.name == name)
            .and_then(|b| match b.types.function { FunctionRef::One(id) => Some(id), _ => None })
            .expect("a function")
    }

    /// What body `function`'s record says `key` depends on
    fn depends(analysis: &Analysis, function: FunctionId, key: &Read) -> Vec<Read> {
        let record = analysis.records.get(&function).expect("a record");
        record.own_influences.get().iter()
            .filter(|(k, _)| k == key)
            .flat_map(|(_, deps)| deps.iter().cloned())
            .collect()
    }

    #[test]
    fn writes_depend_on_what_their_statement_and_its_locals_read() {
        let analysis = analyse_keeping("local t, u = { x = 1 }, {}
            local function f()
                local x = t.x
                u.y = x
                u.z = 1
            end");
        let (t, u, f) = (table_of(&analysis, "t"), table_of(&analysis, "u"), function_of(&analysis, "f"));
        let read_x = Read::Field(t, name_key("x"));
        assert!(depends(&analysis, f, &Read::Field(u, name_key("y"))).contains(&read_x));
        assert!(!depends(&analysis, f, &Read::Field(u, name_key("z"))).contains(&read_x));
    }

    #[test]
    fn a_local_assigned_later_round_a_loop_counts() {
        let analysis = analyse_keeping("local t, u = { x = 1 }, {}
            local function f()
                local v
                for i = 1, 2 do
                    u.a = v
                    v = t.x
                end
            end");
        let (t, u, f) = (table_of(&analysis, "t"), table_of(&analysis, "u"), function_of(&analysis, "f"));
        assert!(depends(&analysis, f, &Read::Field(u, name_key("a"))).contains(&Read::Field(t, name_key("x"))));
    }

    #[test]
    fn what_a_call_passes_depends_on_its_arguments_not_its_returns() {
        let analysis = analyse_keeping("local t = { arg = 1, r = 2 }
            local function g(p) return t.r end
            local function f() local y = g(t.arg) return y end");
        let (t, g, f) = (table_of(&analysis, "t"), function_of(&analysis, "g"), function_of(&analysis, "f"));
        let passed = depends(&analysis, f, &Read::ParameterTypes(g));
        assert!(passed.contains(&Read::Field(t, name_key("arg"))), "{passed:?}");
        assert!(passed.contains(&Read::Method(g)), "{passed:?}");
        assert!(!passed.contains(&Read::Function(g)), "{passed:?}");
        // What it returns does depend on `g`'s
        assert!(depends(&analysis, f, &Read::Function(f)).contains(&Read::Function(g)));
    }

    #[test]
    fn a_signature_depends_only_on_what_gives_it_something() {
        let analysis = analyse_keeping("local t, u = { w = 1 }, {}
            local function f(a) u.q = t.w return a end");
        let (t, f) = (table_of(&analysis, "t"), function_of(&analysis, "f"));
        let signature = depends(&analysis, f, &Read::Function(f));
        assert!(!signature.contains(&Read::Field(t, name_key("w"))), "{signature:?}");
        assert!(signature.contains(&Read::ParameterTypes(f)), "{signature:?}");
        // Its parameters' types: what it requires of them, and nothing here
        assert!(depends(&analysis, f, &Read::ParameterTypes(f)).is_empty());
    }

    #[test]
    fn cycles_of_calls_are_found_once_a_walk_is_done() {
        let analysis = analyse("local a, b, c
            function a() return b() end
            function b() return a() end
            function c() return a() end");
        let cycle = |name: &str| analysis.bindings.iter().find(|b| &*b.name == name)
            .and_then(|b| match b.types.function { FunctionRef::One(id) => analysis.call_cycles.get(&id).copied(), _ => None });
        assert!(cycle("a").is_some());
        assert_eq!(cycle("a"), cycle("b"));
        // Calls into the cycle, but isn't called back
        assert_eq!(cycle("c"), None);
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
    fn operators_on_values_they_cant_take() {
        let warnings = |source: &str| -> Vec<String> { analyse(source).diagnostics.iter().map(|d| d.message.clone()).collect() };
        assert_eq!(warnings("local b = true local n = b + 1"), ["Can't do arithmetic on a boolean"]);
        assert_eq!(warnings("local f = function() end local n = 2 * f"), ["Can't do arithmetic on a function"]);
        assert_eq!(warnings("local b = 1 > 2 local n = -b"), ["Can't do arithmetic on a boolean"]);
        assert_eq!(warnings("local b = false local s = 'x' .. b"), ["Can't concatenate a boolean"]);
        assert_eq!(warnings("local n = 5 local l = #n"), ["Can't get the length of a number"]);
        assert_eq!(warnings("local a, b = 1, 'x' local c = a < b"), ["Can't compare a number with a string"]);
        assert_eq!(warnings("local t = {} local c = t >= 1"), ["Can't compare a table with a number"]);
        assert_eq!(warnings("local c = true < false"), ["Can't compare a boolean with a boolean"]);
        // One report for an expression, though both sides are wrong
        assert_eq!(warnings("local b = true local n = b + b"), ["Can't do arithmetic on a boolean"]);
        // Perhaps nil as well: nil raises an error too
        assert_eq!(warnings("local b = x and true or nil local n = b + 1"), ["Can't do arithmetic on a boolean"]);
        // Numbers, strings that may be numbers, and tables with metamethods
        assert!(warnings("local n = 1 + '2' .. 3 local m = -'4'").is_empty());
        assert!(warnings("local t = setmetatable({}, {}) local n = t + 1 local s = t .. 'x' local u = -t").is_empty());
        assert!(warnings("local s = 'ab' local t = {} local n = #s + #t").is_empty());
        assert!(warnings("local c = 1 < 2 or 'a' <= 'b' local t = {} local d = t < t").is_empty());
        // Not when it may be something that works, or isn't known
        assert!(warnings("local b = x and true or 1 local n = b + 1 local c = b < 2").is_empty());
        assert!(warnings("local n = x + 1 local c = y < 'a' local l = #z").is_empty());
        // Nor nil alone, nor a check that rules out what fails
        assert!(warnings("local v local n = v + 1 local s = v .. 'x'").is_empty());
        assert!(warnings("local v = x and 1 or 'a' if type(v) == 'number' then local c = v < 2 end").is_empty());
        // `==` never raises an error
        assert!(warnings("local c = 1 == 'a' local d = true ~= {}").is_empty());
    }

    #[test]
    fn narrowed_fields() {
        let warnings = |source: &str| -> Vec<String> { analyse_with(source, &library()).diagnostics.iter().map(|d| d.message.clone()).collect() };
        let types = |source: &str| -> Vec<String> {
            let file = analyse_with(source, &library());
            file.field_references.iter().filter(|f| !f.write && f.name() == "m").map(|f| file.describe(f.types)).collect()
        };
        // A number or function, as `ScrollUtil.lua`'s `areaMargin`
        let margin = "local T = {} function T:Set(m) self.m = m end T:Set(5) T:Set(function() end)\n";
        assert_eq!(types(&format!("{margin}function T:Get() if type(self.m) == 'number' then return self.m end return self.m end")), ["number|function()", "number", "function()"]);
        // Only a number reaches it: past the check, nothing does
        let number = "local T = {} function T:Set(m) self.m = m end function T:Reset() end T:Set(5)\n";
        assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'number' then return self.m end return self.m(x) end")).is_empty());
        assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) ~= 'function' then return 0 end return self.m(x) end")).is_empty());
        assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then return self.m(x) end end")).is_empty());
        assert!(warnings(&format!("{number}function T:Get(x) return type(self.m) == 'function' and self.m(x) end")).is_empty());
        assert!(warnings(&format!("{number}function T:Get(x) assert(type(self.m) == 'function') return self.m(x) end")).is_empty());
        // Nested fields, and through a local that isn't `self`
        assert!(warnings("local t = {a = {m = 1}} if type(t.a.m) == 'function' then t.a.m() end").is_empty());
        // Unnarrowed, it's reported
        assert_eq!(warnings(&format!("{number}function T:Get(x) return self.m(x) end")), ["`m` is number, not a function"]);
        // A call or a field write may change it, so it's not narrowed past one
        let changed = |between: &str| warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then {between} return self.m(x) end end"));
        assert_eq!(changed("self:Reset()"), ["`m` is number, not a function"]);
        assert_eq!(changed("self.other = 1"), ["`m` is number, not a function"]);
        assert_eq!(changed("local o = self o.k = 1"), ["`m` is number, not a function"]);
        assert_eq!(changed("self = T"), ["`m` is number, not a function"]);
        assert!(changed("local y = x + 1").is_empty());
        assert_eq!(warnings(&format!("{number}function T:Get(x) return type(self.m) == 'function' and self:Reset() and self.m(x) end")), ["`m` is number, not a function"]);
        assert_eq!(warnings(&format!("{number}function T:Get(x) if type(self.m) ~= 'function' then return elseif self:Reset() then return end return self.m(x) end")), ["`m` is number, not a function"]);
        // Nor into a loop, whose earlier passes may change it, nor a function
        // (and after them, past the loop and the call)
        assert_eq!(changed("for i = 1, 2 do self.m(i) end"), ["`m` is number, not a function"; 2]);
        assert_eq!(changed("local function f() self.m(x) end f()"), ["`m` is number, not a function"; 2]);
        // Defining a function doesn't run it, so the narrowing holds past one
        assert!(changed("local function f() end").is_empty());
        // Nor past the branch that narrowed it
        assert_eq!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then end return self.m(x) end")), ["`m` is number, not a function"]);
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
        // Every branch assigning it replaces what it held
        assert_eq!(types_of("local x if c then x = 1 else x = 's' end local y = x", "y"), ["number|string"]);
        assert_eq!(types_of("local x if c then x = 1 elseif d then x = 's' end local y = x", "y"), ["number|string|nil"]);
        // Within a branch, as anywhere, an assignment replaces it
        assert_eq!(types_of("local x = 1 if c then x = 's' local y = x end", "y"), ["string"]);
        // A branch that returns doesn't go on past the `if`
        assert_eq!(types_of("local x = 1 if c then x = 's' return end local y = x", "y"), ["number"]);
        // What a condition does happens whichever way it goes
        let source = "local x = 1 local function g() x = 's' return true end";
        assert_eq!(types_of(&format!("{source} if g() then end local y = x"), "y"), ["number|string"]);
        assert_eq!(types_of(&format!("{source} if c then elseif g() then x = true end local y = x"), "y"), ["number|string|true"]);
        // Nor what a loop's pass leaves, as it may run again
        assert_eq!(types_of("local x = 1 while c do if d then x = 's' end end local y = x", "y"), ["number|string"]);
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
            "string": {"functions": {"format": {"parameters": ["format: string", "..."], "returns": ["string"]},
                "rep": {"parameters": ["s: string", "n: number"], "returns": ["string"]}}},
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
        // Narrowed through, in the main chunk as in a function
        assert_eq!(builtin_types_of("local x = string.format and string.format('%d', 1)", "x"), ["string"]);
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
        // Until the files assign it, it's what the preset says
        assert_eq!(builtin_types_of("local x = tostring() tostring = nil", "x"), ["string"]);
        assert_eq!(builtin_types_of("tostring = nil local x = tostring", "x"), ["nil"]);
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
        // A string's method takes the string first, so `x` isn't the format
        assert_eq!(builtin_types_of("local function f(x) return ('%d'):format(x) end f(1)", "x"), ["number", "number"]);
        assert_eq!(builtin_types_of("local function f(x) return ('a'):rep(x) end", "x"), ["number", "number"]);
        assert_eq!(builtin_types_of("local function f(s) return string.format(s) end", "s"), ["string", "string"]);
        // Varargs can require a type too
        assert_eq!(builtin_types_of("local function f(a, b) return max(a, b) end", "b"), ["number", "number"]);
        // Globals and methods can be called from anywhere, but still have to obey
        assert_eq!(builtin_types_of("function T:m(x) return strlen(x) end", "x"), ["string", "string"]);
        assert_eq!(builtin_types_of("function F(x) local y = floor(x) end", "x"), ["number", "number"]);
        // What the body requires beats what callers pass
        assert_eq!(builtin_types_of("local function f(x) return floor(x) end f('1')", "x"), ["number", "number"]);
        // Any of several tables meets a requirement of a table, so they're kept
        let source = "local a = { x = 1 } local b = { y = 2 } local u = c and a or b local function g(t) local k = next(t) local v = t.x end g(u)";
        assert_eq!(library_types_of(source, "v")[0], "number");
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
        // And a field not named could be any entry under a string key
        assert_eq!(types_of("local k = 'x' .. 1 local t = {} t[k] = 'v' local a = t.n", "a"), ["string?"]);
        assert_eq!(types_of("local t = {} for k in pairs({ a = 1 }) do t[k] = 'v' end local a = t.a", "a"), ["string?"]);
        // Number keys only: no field of a name is among them
        assert_eq!(types_of("local t = {} t[1] = 'a' local a = t.n", "a"), ["unknown"]);
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
    fn tables_a_field_is_reset_to() {
        // A field given a new table again (emptied) is any of them: what's
        // put in it is in whichever it is, so taking it back out gives that
        let source = "local P = {} function P:Init() self.list = {} end
            function P:Add() table.insert(self.list, true) self.list.count = 1 end
            function P:Reset() self.list = {} end
            function P:Take() local x, c = table.remove(self.list), self.list.count
                for _, v in ipairs(self.list) do local each = v end end";
        assert_eq!(library_types_of(source, "x")[0], "true?");
        assert_eq!(library_types_of(source, "each")[0], "true");
        assert_eq!(library_types_of(source, "c")[0], "number");
        // One without the field doesn't make it unknown: Auctionator's
        // `Constants = {}`, then `Auctionator.Constants = { IsForever = ... }`
        let source = "local A = { C = {} } A.C = { F = true } local x = A.C.F";
        assert_eq!(types_of(source, "x"), ["true"]);
        assert_eq!(types_of("local A = { C = { G = 1 } } A.C = { F = true } local x, y = A.C.F, A.C.G", "y"), ["number"]);
    }

    #[test]
    fn many_records_of_one_constructor() {
        // More records than are kept apart: any of the constructor's
        // elements, with each one's fields
        let records: Vec<String> = (0..12).map(|i| format!("K{i} = {{ key = 'k{i}'{} }}", if i == 3 { ", refresh = 1" } else { "" })).collect();
        let source = format!("local s = {{ {} }} for _, d in pairs(s) do local x, r = d.key, d.refresh d.seen = true end local z = s.K2.seen", records.join(", "));
        assert_eq!(library_types_of(&source, "x")[0], "string");
        assert_eq!(library_types_of(&source, "r")[0], "number");
        // What's written through any of them, each may have
        assert_eq!(library_types_of(&source, "z")[0], "true");
        // Array items too, joined with others put in after
        let items: Vec<&str> = (0..12).map(|_| "{ text = 's' }").collect();
        let source = format!("local lines = {{ {} }} table.insert(lines, {{ text = 1 }}) for _, l in ipairs(lines) do local t = l.text end", items.join(", "));
        assert_eq!(library_types_of(&source, "t")[0], "number|string");
    }

    #[test]
    fn loops_over_what_functions_return() {
        // `return ipairs(self.list)`: a loop over a call gets what the list holds
        let source = "local P = {} function P:Init() self.list = {} end
            function P:Add() table.insert(self.list, 'a') end
            function P:Enumerate() return ipairs(self.list) end
            function P:Use() for i, v in self:Enumerate() do local index, each = i, v end end";
        assert_eq!(library_types_of(source, "each")[0], "string");
        assert_eq!(library_types_of(source, "index")[0], "number");
    }

    #[test]
    fn table_create_makes_a_table() {
        assert_eq!(library_types_of("local t = table.create(2) t[1] = 's' local x = t[1]", "x")[0], "string?");
    }

    #[test]
    fn values_passed_round_a_loop_settle_from_what_they_are_given() {
        // A pool's free frames: released into its list and acquired from it
        // again, so what's acquired is only ever what was made
        let pool = "local P = {} function P:Init() self.free = {} end
            function P:Create() table.insert(self.free, CreateFrame('Button')) end
            function P:Acquire() return table.remove(self.free) end
            function P:Release(f) table.insert(self.free, f) end";
        let source = format!("{pool} local a = P:Acquire() P:Release(a)");
        assert_eq!(library_types_of(&source, "a")[0], "Button?");
        // Through a map of what's in use, found through a parameter, whose
        // type isn't known until a later walk
        let source = format!("{pool} local G = {{}} function G:Pool() return self.pool end
            local function Refresh(group)
                local old = group.used group.used = {{}}
                for _, f in pairs(old) do group:Pool():Release(f) end
                local got = group:Pool():Acquire() group.used[1] = got
            end
            local g = {{ pool = P, used = {{}} }} setmetatable(g, {{ __index = G }}) Refresh(g)");
        assert_eq!(library_types_of(&source, "got")[0], "Button?");
        // Released where it isn't seen: anything may be put back
        let source = format!("{pool} Pool = P");
        assert_eq!(library_types_of(&source, "f")[0], "unknown");
    }

    #[test]
    fn methods_of_tables_a_mixin_is_copied_into() {
        // `self` in `S`'s methods is a frame that got `S` through `P`, so it
        // has `P`'s methods too
        let source = "S = {} function S:A() local b = self:B() return b end
            P = CreateFromMixins(S) function P:B() return 1 end
            local f = CreateFrame('Frame') Mixin(f, P) local x = f:A()";
        assert_eq!(library_types_of(source, "b")[0], "number");
        assert_eq!(library_types_of(source, "x")[0], "number");
    }

    #[test]
    fn frames_of_the_kind_a_function_is_passed() {
        let source = "local Make function Make(kind, parent) return CreateFrame(kind, nil, parent) end
            local function Outer(kind) return Make(kind) end
            local a, b, c, d = Make('Button'), Make('StatusBar'), Outer('Slider'), Make(GetKind())";
        assert_eq!(library_types_of(source, "a")[0], "Button");
        assert_eq!(library_types_of(source, "b")[0], "StatusBar");
        assert_eq!(library_types_of(source, "c")[0], "Slider");
        // Not a literal: any frame
        assert_eq!(library_types_of(source, "d")[0], "Frame");
    }

    #[test]
    fn returned_templates() {
        let definitions = Arc::new(Definitions::from_json(r#"{
            "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}, "Button": {"base": "table", "inherits": ["Frame"]}},
            "method_tables": {"FrameAPI": {"functions": {"Show": {}}}},
            "mixins": {"SlotMixin": {"functions": {"Glow": {"returns": ["number"]}}}},
            "elements": {"Button": "Button", "Frame": "Frame"},
            "templates": {"SlotT": {"type": "Button", "mixins": ["SlotMixin"]}},
            "functions": {"CreateFrame": {"parameters": ["frameType: string"], "returns": ["Frame"]}},
            "returned_templates": {"Provider.Acquire": {"Button": "SlotT"}, "Other.Acquire": {"Button": "NoSuchT"}}
        }"#).unwrap());
        let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
        // Each object of that kind it returns is taken to be made with the template
        let source = "Provider = {} function Provider:Acquire() return CreateFrame('Button') end
            local f = Provider:Acquire() local g = f:Glow()";
        assert_eq!(types_of(source, "g")[0], "number");
        // Another kind it may return stays as it is, so may not have them
        let source = "Provider = {} function Provider:Acquire(plain) if plain then return CreateFrame('Frame') end return CreateFrame('Button') end
            local f = Provider:Acquire() local g = f:Glow()";
        assert_eq!(types_of(source, "g")[0], "unknown");
        // Not for another function, nor a template that isn't known
        let source = "Other = {} function Other:Acquire() return CreateFrame('Button') end
            local f = Other:Acquire() local g = f:Glow()";
        assert_eq!(types_of(source, "g")[0], "unknown");
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
        // Only the `else` goes on, where it's set
        assert_eq!(library_types_of(&format!("{maybe} if not v then return else end local x = v"), "x")[0], "number");
        // Without an `else`, going on with every condition false narrows too
        assert_eq!(library_types_of(&format!("{maybe} if v == nil then v = 's' end local x = v"), "x")[0], "number|string");
        // Assigned in a branch: that's kept as well, and otherwise it was false
        assert_eq!(library_types_of(&format!("{maybe} if v then v = 's' end local x = v"), "x")[0], "string?");
        // `and` and `or`
        assert_eq!(library_types_of(&format!("{maybe} local x = v and v + 1"), "x")[0], "number?");
        assert_eq!(library_types_of(&format!("{maybe} if v and v > 0 then local x = v end"), "x")[0], "number");
        assert_eq!(library_types_of(&format!("{maybe} if not v or v > 0 then else local x = v end"), "x")[0], "number");
        let t = "local t if math.random() > 0.5 then t = { n = 1 } end ";
        assert_eq!(library_types_of(&format!("{t} local x = t and t.n"), "x")[0], "number?");
        // `while`
        assert_eq!(library_types_of(&format!("{maybe} while v do local x = v v = nil end"), "x")[0], "number");
        // An upvalue is narrowed as it is where the closure is made when
        // nothing can assign it after
        assert_eq!(library_types_of(&format!("{maybe} if v then local f = function() local x = v end end"), "x")[0], "number");
        // Not when something may: the closure can run at any time
        assert_eq!(library_types_of(&format!("{maybe} if v then local f = function() local x = v end end v = nil"), "x")[0], "number?");
        assert_eq!(library_types_of(&format!("{maybe} local function g() v = nil end if v then local f = function() local x = v end end"), "x")[0], "number?");
        // Nor in a loop the variable is outside of, which runs its writes again
        // while an earlier closure lives on
        assert_eq!(library_types_of(&format!("{maybe} while true do v = math.random() > 0.5 and 1 or nil if v then local f = function() local x = v end end end"), "x")[0], "number?");
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
            SourceFile { root: SyntaxNode::new_root(Generator::new(&text).process_all()), addon: in_addon.then_some(0), handler: None }
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
            let replaying = Analysis::analyse_replaying(files, &library(), Arc::default(), &[], true);
            let walking = Analysis::analyse_replaying(files, &library(), Arc::default(), &[], false);
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
        // Defaulted from itself: the default, rather than nothing for good
        assert_eq!(types_of("local t = {} t.list = t.list or {} local a = t.list", "a"), ["table"]);
        assert_eq!(types_of("local t = {} t.n = (t.n or 0) + 1 local a = t.n", "a"), ["number"]);
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
        // What it's seen given under other keys is only some of what it holds
        let source = "local s = f() s[5] = true local a = s[3] for _, v in pairs(s) do local b = v end local c = unpack(s)";
        assert_eq!(types_of(source, "a"), ["unknown"]);
        assert_eq!(types_of(source, "b"), ["unknown (not nil)"]);
        assert_eq!(types_of(source, "c"), ["unknown"]);
        let source = "local function P(s) if s[5] == nil then s[5] = s[1] == 1 end return s[5] end local function I(s, l) local W = s[3] return l / W end";
        assert!(analyse(source).diagnostics.is_empty());
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
        // Reading from either of two tables gets what the one with it holds
        assert_eq!(types_of("local t = {} t.x = 1 local a = (c and {} or t).x", "a"), ["number"]);
        // Reading from something unknown stays unknown
        assert_eq!(types_of("local t = {} t.x = 1 local a = (c and f() or t).x", "a"), ["unknown"]);
    }

    /// Files analysed together, in order, as one addon (`in_addon`) or just
    /// sharing globals
    fn analyse_files(sources: &[&str], in_addon: bool) -> Analysis {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: in_addon.then_some(0), handler: None })
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
        assert_eq!(types_in(&analysis, 0, "x"), ["nil"]);
        // So making it if it isn't there makes it
        assert_eq!(types_of("T = T or {} local a = T", "a"), ["table"]);
        assert_eq!(types_of("if G == nil then G = 's' end local a = G", "a"), ["string"]);
        // One the files never set is whatever else set it
        assert_eq!(types_of("local a = Unset", "a"), ["unknown"]);
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
                .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
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

    #[test]
    fn script_handlers_set_in_lua() {
        let source = "local f = CreateFrame('Button')
            f:SetScript('OnEvent', function(self, eventName, a) local s, e, x = self, eventName, a end)
            f:SetScript('OnClick', function(self, button, down) local b, d = button, down end)
            local M = {} function M:OnUpdate(elapsed) local el = elapsed end
            f:HookScript('OnUpdate', M.OnUpdate)
            f:SetScript('OnUpdate', nil)";
        // The frame, then what that kind of handler is passed
        assert_eq!(widget_types_of(source, "s")[0], "Button");
        assert_eq!(widget_types_of(source, "e")[0], "string");
        // An event's payload isn't known
        assert_eq!(widget_types_of(source, "x")[0], "unknown");
        assert_eq!(widget_types_of(source, "b")[0], "string");
        assert_eq!(widget_types_of(source, "d")[0], "boolean");
        // A method's `self` is the frame, so its parameters start after
        assert_eq!(widget_types_of(source, "el")[0], "number");
    }

    #[test]
    fn functions_only_some_clients_have() {
        let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
            "Engrave": {"parameters": ["slot: number"], "returns": ["boolean"], "only": ["Classic"]},
            "Both": {"parameters": [], "returns": [], "only": ["Classic", "Forever"]}
        }}"#).unwrap());
        let analysis = analyse_with("local a, b = Engrave, Both local c = Engrave(1)", &definitions);
        assert_eq!(describe_uses(&analysis, "a")[0], "function(slot: number) -> boolean (Classic only)");
        assert_eq!(describe_uses(&analysis, "b")[0], "function() (Classic and Forever only)");
        assert_eq!(describe_uses(&analysis, "c")[0], "boolean");
    }

    #[test]
    fn callback_registry_callbacks() {
        let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
            "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]}
        }, "tables": {"CallbackRegistryMixin": {"functions": {
            "RegisterCallback": {"parameters": ["event", "func", "owner", "..."], "returns": ["unknown"]},
            "TriggerEvent": {"parameters": ["event", "..."]}
        }}}}"#).unwrap());
        let source = "local R = CreateFromMixins(CallbackRegistryMixin)
            R:RegisterCallback('X', function(o, v) local a, own = v, o end)
            local M = {} function M:OnX(v) local b = v end
            R:RegisterCallback('X', M.OnX, M)
            R:RegisterCallback('X', function(o, extra, v) local c, e = v, extra end, M, 's')
            R:RegisterCallback('Never', function(o, v) local d = v end)
            R:TriggerEvent('X', 5)";
        let analysis = analyse_with(source, &definitions);
        // Each gets its owner (a number made for it without one), what was
        // registered after the owner, then what the event passes
        assert_eq!(describe_uses(&analysis, "a")[0], "number");
        assert_eq!(describe_uses(&analysis, "own")[0], "number");
        assert_eq!(describe_uses(&analysis, "b")[0], "number");
        assert_eq!(describe_uses(&analysis, "c")[0], "number");
        assert_eq!(describe_uses(&analysis, "e")[0], "string");
        // Never triggered: nothing known
        assert_eq!(describe_uses(&analysis, "d")[0], "unknown");
    }

    fn mixin_definitions() -> Arc<Definitions> {
        Arc::new(Definitions::from_json(r#"{"functions": {
            "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
            "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
            "CreateAndInitFromMixin": {"parameters": ["mixin", "..."], "returns": ["table"], "fresh_returns": [{"parameters": [0]}], "method_calls": [{"on": [0], "method": "Init", "arguments": ["..."]}]},
            "CreateFooter": {"parameters": ["parent"], "returns": ["table"], "fresh_returns": [{"mixins": ["FooterMixin"]}]},
            "CreateColumn": {"returns": ["table"], "fresh_returns": [{"mixins": ["FooterMixin"], "values": {"width": "number", "OnResize": "function"}}]},
            "MakePaint": {"parameters": ["r", "g", "a"], "returns": ["table"], "fresh_returns": [{"mixins": ["PaintMixin"]}],
                "field_writes": [{"on": {"returned": 0}, "field": "r", "value": [0]}, {"on": {"returned": 0}, "field": "a", "value": [{"truthy": 2}, "number"]}]}
        }, "tables": {
            "FooterMixin": {"functions": {"Finalize": {"returns": ["number"]}}},
            "PaintMixin": {"functions": {
                "GetR": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "r"}]},
                "SetR": {"parameters": ["r"], "field_writes": [{"on": "self", "field": "r", "value": [0]}]}
            }, "values": {"r": "unknown", "a": "unknown"}},
            "SOME_COLOR": {"functions": {
                "GetR": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "r"}]},
                "GetLabel": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "label"}]}
            }, "values": {"r": "number"}}
        }}"#).unwrap())
    }

    #[test]
    fn fields_given_after_tables_are_made() {
        // As Blizzard's code gives a column a width where it loops over them:
        // each call's has it, besides its mixins' methods
        let analysis = analyse_mixins(&["local c = CreateColumn() local w, done = c.width, c:Finalize() c:OnResize()"]);
        assert_eq!(types_in(&analysis, 0, "w"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
        // A function it's given is a method it has
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
        assert!(!analyse_mixins(&["CreateColumn():Nope()"]).diagnostics.is_empty());
    }

    #[test]
    fn global_tables_return_their_fields() {
        // A global table the definitions give: the field they give it, as
        // for `NORMAL_FONT_COLOR:GetRGB()`, but not what any table's field
        // of that name may be
        let analysis = analyse_mixins(&["local t = {} t.label = 's' local r, label = SOME_COLOR:GetR(), SOME_COLOR:GetLabel()"]);
        assert_eq!(types_in(&analysis, 0, "r"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "label"), ["unknown"]);
    }

    fn analyse_mixins(sources: &[&str]) -> Analysis {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
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
    fn methods_called_on_what_a_function_is_passed() {
        // A definition's: `CreateAndInitFromMixin` calls `Init` on its new table
        let analysis = analyse_mixins(&["local M = {} function M:Init(x, y) self.x = x end
            local o = CreateAndInitFromMixin(M, 1, 's') local v = o.x"]);
        assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "v"), ["number"]);
        // The files' own, on a parameter or a table made with one mixed in,
        // through a function passing its parameters on, each call with its
        // own mixin: neither `Init` gets the other's arguments
        let analysis = analyse_mixins(&["local A = {} function A:Init(a) end local B = {} function B:Init(b) end
            local function Make(mixin, ...) local o = CreateFromMixins(mixin) o:Init(...) return o end
            local function Start(mixin, value) mixin:Init(value) end
            local function MakeAgain(mixin, ...) return Make(mixin, ...) end
            Make(A, 1) MakeAgain(B, 's') Start(A, 2)"]);
        assert_eq!(types_in(&analysis, 0, "a"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "b"), ["string"]);
        // Called in a function whose calls aren't all seen, as a callback
        // handed to something else is: that may call it with anything
        let analysis = analyse_mixins(&["local M = {} function M:Set(v) end M:Set(1)
            register(function(button, value) button:Set(value) end)"]);
        assert_eq!(types_in(&analysis, 0, "v"), ["unknown"]);
        // Passing it on to itself, as Blizzard's `VariadicInsert` does, settles
        let analysis = analyse_mixins(&["local A = {} function A:Insert(a) end
            local function Insert(on, ...) local first = ... if first == nil then return end on:Insert(first) Insert(on, select(2, ...)) end
            Insert(A, 1, 2)"]);
        assert!(analysis.walks < 10);
    }

    #[test]
    fn tables_of_a_family() {
        // Any number of the tables a definition's function makes, together,
        // are of its family: its mixins' type, with what any was given
        let footers: Vec<String> = (0..12).map(|_| "CreateFooter()".to_string()).collect();
        let analysis = analyse_mixins(&[&format!("local all = {{ {} }} all[2].label = 's'
            local any = all[1] local done, label = any:Finalize(), any.label
            local one = CreateFooter() one.own = 1 local own, other = one.own, CreateFooter().own", footers.join(", "))]);
        assert_eq!(types_in(&analysis, 0, "any")[0], "FooterMixin?");
        assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "label"), ["string"]);
        // Each still has its own fields
        assert_eq!(types_in(&analysis, 0, "own"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "other"), ["unknown"]);
        // Mixing in what it's passed: each call's own, any of them all of them
        let analysis = analyse_mixins(&["local A = {} function A:Name() return 's' end local B = {} function B:Size() return 1 end
            local all = { CreateAndInitFromMixin(A), CreateAndInitFromMixin(B) } local any = all[1]
            local name, size, a = any:Name(), any:Size(), CreateAndInitFromMixin(A).Size"]);
        assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "size"), ["number"]);
    }

    #[test]
    fn fields_given_what_functions_are_passed() {
        // As `CreateColor` and `ColorMixin` are: each call's table has what
        // that call passed, its methods give it back, and any of many has
        // what each has
        let paints: Vec<String> = (0..9).map(|_| "MakePaint(1)".to_string()).collect();
        let analysis = analyse_mixins(&[&format!("local p = MakePaint(1) local s = MakePaint('s')
            local r, a, sr, got = p.r, p.a, s.r, p:GetR()
            local t = MakePaint('t') t:SetR(true) local set = t.r
            local all = {{ {} }} local any = all[1] local anyr, anyget = any.r, any:GetR()", paints.join(", "))]);
        assert_eq!(types_in(&analysis, 0, "r"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "a"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "sr"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "got"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "set"), ["string|true"]);
        // Any of them is of the function's family: what any of its tables
        // in the file was given, the others' too
        assert_eq!(types_in(&analysis, 0, "anyr"), ["number|string|true"]);
        assert_eq!(types_in(&analysis, 0, "anyget"), ["number|string|true"]);
        // The files' own, through the methods a function calls on its new
        // table, each call's its own
        let analysis = analyse_mixins(&["local M = {}
            function M:SetRA(r, a) self.r = r self.a = a or 1 end
            function M:OnLoad(r, a) self:SetRA(r, a) end
            function M:GetR() return self.r end
            local function Make(r, a) local c = CreateFromMixins(M) c:OnLoad(r, a) return c end
            local c, s = Make(1), Make('s', 'x')
            local cr, sr, ca, sa, got = c.r, s.r, c.a, s.a, s:GetR()"]);
        assert_eq!(types_in(&analysis, 0, "cr"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "sr"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "ca"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "sa"), ["number|string"]);
        assert_eq!(types_in(&analysis, 0, "got"), ["string"]);
    }

    #[test]
    fn cached_tables_are_not_new_each_call() {
        // Kept in an upvalue or a global, so later calls return the same one
        let analysis = analyse_mixins(&["local cached local function Get() if not cached then cached = {} end return cached end
            local function GetMade() if not Cached then Cached = CreateAndInitFromMixin({}) end return Cached end
            Get().x = 1 GetMade().y = 1 local x, y = Get().x, GetMade().y"]);
        assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
    }

    #[test]
    fn definitions_making_tables_with_mixins() {
        // As generated from Blizzard's `CreateAndInitFromMixin`: each call a
        // new table, with what it's passed mixed in
        let analysis = analyse_mixins(&["local A = { x = 1 } function A:Name() return 's' end local B = { x = 's' }
            local a, b = CreateAndInitFromMixin(A, 1), CreateAndInitFromMixin(B)
            local ax, name, bx = a.x, a:Name(), b.x"]);
        assert_eq!(types_in(&analysis, 0, "ax"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "bx"), ["string"]);
        // As `GamepadSharedUtility.CreatePromptedBindingFooter` is: with a
        // global of the definitions mixed in, and fields of its own
        let analysis = analyse_mixins(&["local f = CreateFooter() f.x = 1 local done, x, other = f:Finalize(), f.x, CreateFooter().x"]);
        assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
        // Another call's table doesn't have them
        assert_eq!(types_in(&analysis, 0, "other"), ["unknown"]);
        // Functions of the files returning what such a function does: new at
        // each call too, with what they pass on mixed in
        let analysis = analyse_mixins(&["local A = {} function A:Name() return 's' end local B = {} function B:Size() return 1 end
            local function Make(mixin) return CreateAndInitFromMixin(mixin) end
            local function MakeA() return CreateAndInitFromMixin(A) end
            local function MakeAgain(mixin) return Make(mixin) end
            local a, b, c = MakeA():Name(), Make(B):Size(), MakeAgain(A):Name()"]);
        assert_eq!(types_in(&analysis, 0, "a"), ["string"]);
        assert_eq!(types_in(&analysis, 0, "b"), ["number"]);
        assert_eq!(types_in(&analysis, 0, "c"), ["string"]);
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
            .collect();
        Analysis::analyse(&files, &definitions)
    }

    #[test]
    fn functions_mixing_in_parameters() {
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
                "AnimationGroup": {"base": "table", "method_tables": ["AnimationGroupAPI"]},
                "Animation": {"base": "table", "method_tables": ["AnimationAPI"]},
                "Alpha": {"base": "table", "inherits": ["Animation"], "method_tables": ["AlphaAPI"]},
                "ItemKey": {"base": "table"}
            },
            "method_tables": {
                "RegionAPI": {"functions": {"SetPoint": {}, "Show": {}, "GetParent": {"returns": ["Region"]}, "SetParent": {"parameters": ["parent: Frame|nil"]}}},
                "FrameAPI": {"functions": {
                    "SetScript": {"parameters": ["scriptTypeName: string", "script: function|nil"]},
                    "HookScript": {"parameters": ["scriptTypeName: string", "script: function"]},
                    "CreateTexture": {"returns": ["Texture"]},
                    "CreateFontString": {"parameters": ["name: string|nil", "templateName: string|nil"], "returns": ["Texture"]},
                    "GetChild": {"returns": ["Frame|nil"]},
                    "CreateAnimationGroup": {"returns": ["AnimationGroup"]}
                }},
                "AnimationGroupAPI": {"functions": {"CreateAnimation": {"parameters": ["animationType: string|nil", "name: string|nil", "templateName: string|nil"], "returns": ["Animation"]}}},
                "AnimationAPI": {"functions": {"Play": {}}},
                "AlphaAPI": {"functions": {"SetFromAlpha": {}}},
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
                "CopyTable": {"parameters": ["settings", "shallow"], "returns": ["table"]},
                "CopyTableSafe": {"parameters": ["settings", "shallow"], "returns": ["table|nil"]},
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
    fn missing_data_fields() {
        let definitions = Arc::new(Definitions::from_json(r#"{
            "types": {"Info": {"base": "table", "values": {"count": "number", "link": "string|nil"}}, "Loose": {"base": "table"}},
            "functions": {"GetInfo": {"returns": ["Info|nil"]}, "GetLoose": {"returns": ["Loose"]},
                "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
                "rawset": {"parameters": ["t: table", "key", "value"], "returns": ["table"]},
                "setmetatable": {"parameters": ["t: table", "mt"], "returns": ["table"]},
                "pairs": {"parameters": ["t"], "returns": ["unknown"]}}
        }"#).unwrap());
        let warnings = |source: &str| -> Vec<String> {
            analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect()
        };
        assert!(warnings("local i = GetInfo() print(i.count, i.link, i['count'])").is_empty());
        assert_eq!(warnings("local i = GetInfo() print(i.cuont)"), ["Info has no field `cuont`"]);
        assert_eq!(warnings("local i = GetInfo() print(i['cuont'])"), ["Info has no field `cuont`"]);
        // The addon may give it fields of its own, even after reading them
        assert!(warnings("local i = GetInfo() print(i.extra) i.extra = 1").is_empty());
        assert!(warnings("local function tag(t) t.extra = true end local i = GetInfo() tag(i) print(i.extra)").is_empty());
        // A type without `values` may have any field
        assert!(warnings("local l = GetLoose() print(l.anything)").is_empty());
        // Or fields that can't be named: under a computed key, by `rawset`, a
        // mixin or a metatable, wherever that is
        assert!(warnings("local i = GetInfo() local k = 'extra' i[k] = 1 print(i.extra)").is_empty());
        assert!(warnings("local i = GetInfo() print(i.extra) local function f() for k, v in pairs({}) do i[k] = v end end f()").is_empty());
        assert!(warnings("local i = GetInfo() rawset(i, 'extra', 1) print(i.extra)").is_empty());
        assert!(warnings("local i = GetInfo() Mixin(i, {extra = 1}) print(i.extra)").is_empty());
        assert!(warnings("local i = GetInfo() setmetatable(i, {__index = {extra = 1}}) print(i.extra)").is_empty());
        // Numbered entries don't give it names
        assert_eq!(warnings("local i = GetInfo() i[1] = true print(i.extra)"), ["Info has no field `extra`"]);
        // Nor methods, unless the addon gives it some
        assert_eq!(warnings("local i = GetInfo() i:Describe()"), ["Info has no method `Describe`"]);
        assert!(warnings("local i = GetInfo() i:Describe() function i:Describe() end").is_empty());
        assert!(warnings("local i = GetInfo() i:Describe() Mixin(i, {Describe = function() end})").is_empty());
        assert!(warnings("local l = GetLoose() l:Anything()").is_empty());
        // Calling a field that isn't a function
        assert_eq!(warnings("local i = GetInfo() i.count()"), ["`count` is number, not a function"]);
        assert_eq!(warnings("local i = GetInfo() i:link()"), ["`link` is string|nil, not a function"]);
        assert_eq!(warnings("local t = {n = 1} t.n()"), ["`n` is number, not a function"]);
        assert_eq!(warnings("local t = {} t.flag = true t:flag()"), ["`flag` is true, not a function"]);
        // Not when it may be one, or what it is is a guess
        assert!(warnings("local t = {n = 1} t.n = function() end t.n()").is_empty());
        assert!(warnings("local t = {c = setmetatable({}, {__call = function() end})} t.c()").is_empty());
        assert!(warnings("local function f(x) x.n() end local a = {} a.n = 1 f(a) local b = {} b.n = print f(b)").is_empty());
        assert_eq!(warnings("local function f(x) x.n() end local a = {} a.n = 1 f(a)"), ["`n` is number, not a function"]);
        assert!(warnings("local t = {} t.n()").is_empty());
        // Through a string key
        assert_eq!(warnings("local i = GetInfo() i['count']()"), ["`count` is number, not a function"]);
        assert_eq!(warnings("local t = {n = 's'} t['n']()"), ["`n` is string, not a function"]);
        assert!(warnings("local t = {n = print} t['n']() local k = 'n' t[k]()").is_empty());
        // A local variable
        assert_eq!(warnings("local n = 1 n()"), ["`n` is number, not a function"]);
        assert_eq!(warnings("local f = x and 'a' or 2 f()"), ["`f` is number|string, not a function"]);
        assert_eq!(warnings("local n = 1 local function g() n() end g()"), ["`n` is number, not a function"]);
        assert!(warnings("local n = 1 n = print n()").is_empty());
        assert!(warnings("local f = x and 1 or print f()").is_empty());
        assert!(warnings("local f if x then f = print end f()").is_empty());
        assert!(warnings("local f = 1 local function g() f() end f = print g()").is_empty());
        assert!(warnings("local t = setmetatable({}, {__call = print}) t()").is_empty());
        assert!(warnings("local f f()").is_empty());
        // A parameter, when every call is seen passing one
        assert_eq!(warnings("local function g(cb) cb() end g(1) g('a')"), ["`cb` is number|string, not a function"]);
        assert!(warnings("local function g(cb) cb() end g(1) g(print)").is_empty());
        // Not once the function escapes, so other code may call it
        assert!(warnings("local function g(cb) cb() end g(1) Register(g)").is_empty());
        assert!(warnings("T = {} function T.g(cb) cb() end T.g(1) local h = T.g").is_empty());
        // A global, by every value the files give it
        assert_eq!(warnings("G = 1 G()"), ["`G` is number, not a function"]);
        assert_eq!(warnings("G = 1 _G.G()"), ["`G` is number, not a function"]);
        assert!(warnings("G = 1 local function f() G() end G = print f()").is_empty());
        assert!(warnings("Unset()").is_empty());
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
    fn animations_of_the_kind_named() {
        let group = "local g = CreateFrame('Frame'):CreateAnimationGroup() ";
        assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation('alpha')"), "a"), ["Alpha"]);
        assert!(warnings(&format!("{group} local a = g:CreateAnimation('Alpha') a:SetFromAlpha() a:Play()")).is_empty());
        // Not a kind of animation, not a literal, or none: an Animation
        assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation('Button')"), "a"), ["Animation"]);
        assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation(kind)"), "a"), ["Animation"]);
        assert_eq!(warnings(&format!("{group} g:CreateAnimation():SetFromAlpha()")), ["Animation has no method `SetFromAlpha`"]);
    }

    #[test]
    fn copied_tables() {
        let made = "local t = { n = 1, sub = { s = 'x' }, list = { 1, 2 } } local M = {} function M:Go() return true end Mixin(t, M)
            local c = CopyTable(t) ";
        // What it holds, however deep, and its mixins'
        assert_eq!(widget_types_of(&format!("{made} local n, s, l, g = c.n, c.sub.s, c.list, c:Go()"), "n"), ["number"]);
        assert_eq!(widget_types_of(&format!("{made} local n, s = c.n, c.sub.s"), "s"), ["string"]);
        assert_eq!(widget_types_of(&format!("{made} local l = c.list"), "l"), ["number[]"]);
        assert_eq!(widget_types_of(&format!("{made} local g = c:Go()"), "g"), ["true"]);
        // A table of its own, its tables copied too: writes to it don't reach the original
        assert_eq!(widget_types_of(&format!("{made} c.added = 1 c.sub.extra = 1 local a, e = t.added, t.sub.extra"), "a"), ["unknown"]);
        assert_eq!(widget_types_of(&format!("{made} c.sub.extra = 1 local e = t.sub.extra"), "e"), ["unknown"]);
        assert_eq!(widget_types_of(&format!("{made} c.sub.extra = 1 local e = c.sub.extra"), "e"), ["number"]);
        // A shallow copy shares the tables it holds
        assert_eq!(widget_types_of(&format!("{made} local h = CopyTable(t, true) h.sub.extra = 1 local e = t.sub.extra"), "e"), ["number"]);
        // A table holding itself is shared rather than copied forever
        assert_eq!(widget_types_of("local t = { n = 1 } t.me = t local c = CopyTable(t) local n = c.me.n", "n"), ["number"]);
        // `CopyTableSafe` gives nil for nil
        assert_eq!(widget_types_of("local t = c and { n = 1 } or nil local s = CopyTableSafe(t)", "s"), ["table?"]);
        // Not a table made in the files: as the definitions say
        assert_eq!(widget_types_of("local c = CopyTable(unknown)", "c"), ["table"]);
        // Any of several tables: what any of them holds
        assert_eq!(widget_types_of("local t = x and { n = 1 } or { s = 'a' } local c = CopyTable(t) local n, s = c.n, c.s", "n"), ["number"]);
        assert_eq!(widget_types_of("local t = x and { n = 1 } or { s = 'a' } local c = CopyTable(t) local s = c.s", "s"), ["string"]);
    }

    /// Types at each use of `name` in `source`, with `saved` the globals the
    /// game saves between sessions
    fn saved_types_of(source: &str, saved: &[&str], name: &str) -> Vec<String> {
        let root = SyntaxNode::new_root(Generator::new(source).process_all());
        let saved: Vec<String> = saved.iter().map(|s| s.to_string()).collect();
        let files = [SourceFile { root, addon: None, handler: None }];
        describe_uses(&Analysis::analyse_over(&files, &library(), Arc::default(), &saved), name)
    }

    #[test]
    fn saved_variables_hold_what_the_addon_writes() {
        // A profile filled with defaults, which an imported table of any shape
        // may replace
        let source = "local Defaults = { a = 1, b = 'x' }
            function Init()
              if SV == nil then SV = { Profiles = { DEFAULT = {} } } end
              local profile = SV.Profiles[Current] or SV.Profiles.DEFAULT
              for k, v in pairs(Defaults) do
                if profile[k] == nil then profile[k] = v end
              end
              Profile = profile
            end
            function Import(data, name) if type(data) == 'table' then SV.Profiles[name] = data end end
            function Get(name) return Profile[name] end
            local got = Get('a')";
        assert_eq!(saved_types_of(source, &["SV"], "got"), ["number|string|nil"]);
        // Not saved: the import may hold anything, so what's read from it may be
        assert_eq!(saved_types_of(source, &[], "got"), ["unknown"]);
        // Of a value that may also be such a table, the rest is kept
        let source = "SV = { list = {} }
            function Add(t) if type(t) == 'table' then SV.list[#SV.list + 1] = x and t or 1 end end
            local got = SV.list[1]";
        assert_eq!(saved_types_of(source, &["SV"], "got"), ["number?"]);
        assert_eq!(saved_types_of(source, &[], "got"), ["number|table|nil"]);
        // A copy of a saved table into itself doesn't keep what was left out
        let source = "SV = { p = { a = 1 } }
            function Copy() SV.p = CopyTable(SV.p) end
            function Import(data, k) if type(data) == 'table' then SV.p[k] = data end end
            local got = SV.p['a']";
        assert_eq!(saved_types_of(source, &["SV"], "got"), ["number"]);
    }

    #[test]
    fn mixins_the_definitions_give() {
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // Hover shows the native type, not the mixins it's made with
        assert_eq!(hover("Acquire"), "field pool:Acquire: function() -> Button, boolean");
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
        // Hover shows the native type, not the template
        assert_eq!(widget_types_of(made, "f"), ["Button"]);
        let either = "local f = c and CreateFrame('Button', nil, nil, 'ButtonT') or CreateFrame('Button') ";
        assert_eq!(widget_types_of(either, "f"), ["Button"]);
        // What the objects are made with
        let _made_with = MadeWith::shown();
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
        Arc::new(definitions.with_xml(&objects, crate::xml::sites(&objects, "/addon/Frames.xml", xml)))
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
        // What the objects are made with, which hover doesn't show
        let _made_with = MadeWith::shown();
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
    fn definitions_in_addon_xml() {
        let definitions = with_xml(ADDON_XML);
        let line_of = |text: &str| ADDON_XML[..ADDON_XML.find(text).unwrap()].matches('\n').count() as u32 + 1;
        let source = "AddonMixin = {}\nfunction AddonMixin:Extra() end\nlocal f = CreateFrame('Button', nil, nil, 'AddonT')\nprint(f.Glow, AddonFrame.Close)\nf:Extra()";
        let analysis = analyse_with(source, &definitions);
        let at = |text: &str| TextSize::from((source.find(text).unwrap() + 1) as u32);
        // A template's child, and a named frame's, in the XML
        assert_eq!(analysis.external_definition(FileId(0), at("Glow")), Some(("/addon/Frames.xml".into(), line_of("<Texture parentKey=\"Glow\""))));
        assert_eq!(analysis.external_definition(FileId(0), at("Close")), Some(("/addon/Frames.xml".into(), line_of("<Button name=\"$parentClose\""))));
        assert_eq!(analysis.external_definition(FileId(0), at("AddonFrame")), Some(("/addon/Frames.xml".into(), line_of("<Frame name=\"AddonFrame\""))));
        // A method from the template's Lua mixin, in the files
        let extra = source.rfind("Extra").unwrap() + 1;
        let sites = analysis.definition(FileId(0), TextSize::from(extra as u32));
        assert_eq!(sites.iter().map(|s| &source[usize::from(s.range.start())..][..5]).collect::<Vec<_>>(), ["Extra"]);
        assert!(usize::from(sites[0].range.start()) < source.find("local f").unwrap());
        // Defined in the files: not outside them
        assert_eq!(analysis.external_definition(FileId(0), TextSize::from(extra as u32)), None);
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
        // A field that isn't a function is another problem, reported as that
        assert_eq!(warnings("local t = { Tree = 1 } t:Tree()"), ["`Tree` is number, not a function"]);
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
        definitions_with(sources, &library())
    }

    fn definitions_with(sources: &[&str], definitions: &Arc<Definitions>) -> Vec<(u32, String)> {
        let (file, offset) = sources.iter().enumerate()
            .find_map(|(i, s)| s.find('@').map(|at| (i, at)))
            .expect("a cursor");
        let sources: Vec<String> = sources.iter().map(|s| s.replace('@', "")).collect();
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
            .collect();
        let analysis = Analysis::analyse(&files, definitions);
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
    fn definitions_of_copied_fields() {
        // Where the original's field is set, however deep
        let source = "local t = { n = 1, sub = { s = 'x' } }\nlocal c = CopyTable(t)\nprint(c.@n)";
        assert_eq!(definitions_with(&[source], &widgets()), [at(0, "n = 1, sub = { s = 'x' } }")]);
        let source = "local t = { n = 1, sub = { s = 'x' } }\nlocal c = CopyTable(t)\nprint(c.sub.@s)";
        assert_eq!(definitions_with(&[source], &widgets()), [at(0, "s = 'x' } }")]);
        // Set on the copy itself: there
        let source = "local t = { n = 1 }\nlocal c = CopyTable(t)\nc.n = 2\nprint(c.@n)";
        assert_eq!(definitions_with(&[source], &widgets()), [at(0, "n = 2")]);
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
