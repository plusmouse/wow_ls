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
//! parameter types settle (call-site types can chain through several functions),
//! or after [`MAX_WALKS`].
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
//! Known gaps: indexing with brackets (`t[k]`) isn't tracked, and globals the
//! file doesn't assign (like saved variables) have no table identity. Recursive
//! calls return unknown.

use std::collections::HashMap;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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
    /// A plain table, or one of several named types
    Unknown,
}

/// Where a table was made (its `{`), or where the variable given a pseudo
/// identity was declared. The same across walks
pub type TableId = Location;

impl NamedRef {
    fn union(self, other: NamedRef) -> NamedRef {
        match (self, other) {
            (NamedRef::None, x) | (x, NamedRef::None) => x,
            (a, b) if a == b => a,
            (NamedRef::Instance { named: a, at: x, extended: e }, NamedRef::Instance { named: b, at: y, extended: f }) if a == b && x == y => {
                NamedRef::Instance { named: a, at: x, extended: e || f }
            }
            // Different objects of one type are still that type
            (a, b) => match (a.object_type(), b.object_type()) {
                (Some((a, e)), Some((b, f))) if a == b => if e || f { NamedRef::Extended(a) } else { NamedRef::One(a) },
                _ => NamedRef::Unknown,
            },
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
            numeric: self.numeric.union(other.numeric),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshObject {
    pub made: TableId,
    pub mixed_parameters: Vec<usize>,
    /// When it's a pool, the parameters mixed into its objects (by its init
    /// or reset function)
    pub pool_mixed_parameters: Option<Vec<usize>>,
}

impl FunctionInfo {
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

#[derive(Debug)]
pub struct Binding {
    pub name: String,
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

#[derive(Debug)]
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
#[derive(Debug)]
pub struct FieldReference {
    pub file: FileId,
    /// The field name
    pub range: TextRange,
    /// The code reaching the field, for `path`. Kept rather than its text,
    /// which is only needed for hover
    access: SyntaxNode,
    /// The field's type, or for a write the value assigned
    pub types: Type,
    pub write: bool,
    /// A read that's directly called: `f` in `t.f(x)` or `t:f(x)`
    pub callee: bool,
    /// For a callee, what this call returns when that's more than the function
    /// says (a pool's `Acquire` giving the pool's object)
    pub call_returns: Option<Values>,
}

impl FieldReference {
    /// The field name
    fn name(&self) -> Option<String> {
        self.access.covering_element(self.range).into_token().map(|t| t.text().to_string())
    }

    /// How the field was reached, without whitespace or comments, like
    /// `addonTable.hiddenTable`
    pub fn path(&self) -> String {
        let end = self.range.end();
        self.access.descendants_with_tokens()
            .filter_map(|t| t.into_token())
            .filter(|t| t.text_range().end() <= end)
            .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment))
            .map(|t| t.text().to_string())
            .collect()
    }
}

/// A problem found while analysing, like a call of a method a WoW type
/// doesn't have
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: FileId,
    pub range: TextRange,
    pub message: String,
}

/// A file to analyse
pub struct SourceFile {
    pub root: SyntaxNode,
    /// Whether the file is part of an addon, so `...` at its top is the addon's
    /// name and namespace
    pub in_addon: bool,
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
    /// Every field name any table in the files was given a function under
    pub function_fields: std::collections::HashSet<String>,
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
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// One file on its own
    pub fn analyse_file(root: &SyntaxNode, definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse(&[SourceFile { root: root.clone(), in_addon: false }], definitions)
    }

    /// Files in the order they load, as one program. The `FileId`s in the
    /// result are positions in `files`
    pub fn analyse(files: &[SourceFile], definitions: &Arc<Definitions>) -> Analysis {
        let mut result = Walker::new(None, definitions).walk(files);
        for _ in 1..MAX_WALKS {
            let next = Walker::new(Some(&result), definitions).walk(files);
            let settled = next.fields == result.fields
                && next.parents == result.parents
                && next.mixins == result.mixins
                && next.mixed_into == result.mixed_into
                && next.pool_objects == result.pool_objects
                && next.any_table_fields == result.any_table_fields
                && next.functions.iter().all(|(id, f)| {
                    result.functions.get(id).is_some_and(|before| before.parameters == f.parameters)
                });
            result = next;
            if settled {
                break
            }
        }
        result
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
        let names = ty.set.names();
        let signature = match ty.function {
            FunctionRef::One(id) if signatures => self.functions.get(&id).map(|f| self.signature(f)),
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
        // A single type that can also be nil is written `T?`
        let optional = names.len() == 2 && names[1] == "nil" && !names[0].starts_with("unknown");
        let wrap = names.len() > 1;
        let names: Vec<String> = names.iter()
            .filter(|&&name| !(optional && name == "nil"))
            .map(|&name| match (&signature, named, name) {
                (Some(s), _, "function") if wrap && s.contains(" -> ") => format!("({s})"),
                (Some(s), _, "function") => s.clone(),
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] && signatures
                    && ty.named.identity().is_some_and(|at| self.pool_objects.contains_key(&at)) => {
                    let object = ty.named.identity().and_then(|at| self.pool_objects.get(&at)).copied().unwrap_or(Type::UNKNOWN);
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
        match self.table_names.get(&at) {
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
        for mixin in self.mixins.get(&at).into_iter().flatten() {
            let Some(mixin) = mixin.named.identity() else { continue };
            match self.table_names.get(&mixin) {
                Some(name) => if !names.contains(name) { names.push(name.clone()) },
                None => self.collect_mixins(mixin, names, depth + 1),
            }
        }
    }

    fn signature(&self, function: &FunctionInfo) -> String {
        self.signature_returning(function, &function.returns)
    }

    /// A callee's type, with what the call returns when that's known better
    /// than the function's own returns
    pub fn describe_callee(&self, ty: Type, call_returns: Option<&Values>) -> String {
        let function = match ty.function {
            FunctionRef::One(id) => self.functions.get(&id),
            FunctionRef::Builtin(id) => self.definitions.function(id),
            _ => None,
        };
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
        Some((format!("field {}: {}", field.path(), self.describe_callee(field.types, field.call_returns.as_ref())), field.range))
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

    /// The function a value is, if it's exactly one known function
    pub fn function_info(&self, function: FunctionRef) -> Option<&FunctionInfo> {
        match function {
            FunctionRef::One(id) => self.functions.get(&id),
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
    /// The file being walked
    file: FileId,
    /// Whether that file is part of an addon
    in_addon: bool,
    definitions: Arc<Definitions>,
    bindings: Vec<Binding>,
    /// Type of each binding at the current point of the walk
    current: Vec<Type>,
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
    /// Index in `scopes` of the current function's body
    function_scope: usize,
    /// Inside the right of an `and`/`or`, which might not run
    short_circuit_depth: usize,
    /// Union of the arguments at every direct call of each function
    call_arguments: HashMap<FunctionId, Values>,
    /// The single type each parameter has been required to have so far. None
    /// when two requirements conflicted
    mandates: HashMap<BindingId, Option<Type>>,
    /// The local each function literal was directly assigned to
    assigned_to: HashMap<FunctionId, BindingId>,
    /// Field writes seen on this walk. Reads use the previous walk's, as a
    /// field can be written after (or in another function from) a read
    fields: HashMap<TableId, HashMap<String, Type>>,
    any_table_fields: HashMap<String, Type>,
    /// Parents seen on this walk. Reads use the previous walk's, like fields
    parents: HashMap<TableId, Type>,
    /// The field names each function was stored under, in any table
    stored_in_fields: HashMap<FunctionId, std::collections::HashSet<String>>,
    /// Mixins seen on this walk. Reads use the previous walk's, like fields
    mixins: HashMap<TableId, Vec<Type>>,
    /// What each table made in the files was mixed into on this walk
    mixed_into: HashMap<TableId, Vec<NamedRef>>,
    /// Pools made on this walk, and what they hold
    pool_objects: HashMap<TableId, Type>,
    /// Functions passed straight to a pool, which calls them only as it
    /// documents (with its objects)
    pool_callbacks: std::collections::HashSet<FunctionId>,
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

/// How much had been recorded before a loop pass, to discard that pass
struct Snapshot {
    bindings: usize,
    references: usize,
    field_references: usize,
    diagnostics: usize,
    deferred: usize,
    globals_created: usize,
}

/// Every file's top level, which run one after another
const MAIN_CHUNK: usize = 0;

/// The table every file of an addon gets as the second value of `...`. Not a
/// real file position, so it can't clash with a table made in a file
const ADDON_NAMESPACE: TableId = Location { file: FileId(u32::MAX), offset: TextSize::new(0), slot: 0 };

/// Each walk past the second lets call-site parameter types pass one more step
/// down a chain of calls (`a(1)` calling `b(x)` calling `c(y)`). The last walk is
/// sound even if parameters hadn't settled, just less precise
pub const MAX_WALKS: usize = 4;

/// Passes after which a loop that still hasn't settled is widened: any variable
/// whose type is still changing becomes unknown. Without this, a loop already
/// terminates (each pass adds at least one bit to some variable's TypeSet), but
/// nested loops multiply their passes. Realistic code settles in 2-3 passes
const LOOP_PASSES_BEFORE_WIDENING: usize = 8;

impl<'a> Walker<'a> {
    fn new(prior: Option<&'a Analysis>, definitions: &Arc<Definitions>) -> Walker<'a> {
        Walker { prior, definitions: definitions.clone(), ..Walker::default() }
    }

    /// Each file's top level runs in turn, so globals continue from one file to
    /// the next. Locals end with their file
    fn walk(mut self, files: &[SourceFile]) -> Analysis {
        for (i, file) in files.iter().enumerate() {
            self.file = FileId(i as u32);
            self.in_addon = file.in_addon;
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

    /// `...` at the top of an addon's file: its name and namespace table
    fn addon_arguments(&self) -> Option<Values> {
        (self.in_addon && self.function == MAIN_CHUNK).then(|| Values {
            values: vec![Type::of(TypeSet::STRING), Type::table(ADDON_NAMESPACE)],
            rest: Type::NIL,
        })
    }

    fn finish(mut self) -> Analysis {
        self.settle_parameter_types();
        for binding in self.bindings.iter_mut() {
            if binding.kind == BindingKind::Global && !binding.assigned {
                binding.types = self.definitions.global(&binding.name).unwrap_or(Type::UNKNOWN);
            }
        }
        for &index in &self.deferred {
            let binding = self.references[index].binding;
            self.references[index].types = self.bindings[binding].types;
        }
        self.references.sort_by_key(|r| (r.file, r.range.start()));
        self.field_references.sort_by_key(|r| (r.file, r.range.start()));
        self.diagnostics.sort_by_key(|d| (d.file, d.range.start()));
        let function_fields = self.fields.values().flat_map(|fields| fields.iter())
            .chain(self.any_table_fields.iter())
            .filter(|(_, ty)| ty.set.contains(TypeSet::FUNCTION))
            .map(|(name, _)| name.clone())
            .collect();
        let mixed_into = self.mixed_into.keys()
            .filter_map(|&mixin| match self.mixin_target(mixin, 0) {
                MixinTarget::Object(named, extended) => Some((mixin, (named, extended))),
                _ => None,
            })
            .collect();
        // References are sorted, so the first write of each table wins
        let mut table_names: HashMap<TableId, String> = HashMap::new();
        // Not parameters, whose types come from what calls pass them
        let variables = self.references.iter()
            .filter(|r| r.write && self.bindings[r.binding].kind != BindingKind::Parameter)
            .map(|r| (r.file, r.range.start(), r.types, self.bindings[r.binding].name.clone()));
        let fields = self.field_references.iter().filter(|f| f.write)
            .map(|f| (f.file, f.range.start(), f.types, f.path()));
        let mut named: Vec<_> = variables.chain(fields).collect();
        named.sort_by_key(|&(file, start, ..)| (file, start));
        for (_, _, types, name) in named {
            if let NamedRef::Table(at) = types.named {
                table_names.entry(at).or_insert(name);
            }
        }
        Analysis {
            function_fields,
            mixed_into,
            table_names,
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
        let mut escaped = std::collections::HashSet::new();
        let mut unresolved = std::collections::HashSet::new();
        for field in self.field_references.iter().filter(|f| !f.write) {
            match field.types.function {
                FunctionRef::One(function) if !field.callee => { escaped.insert(function); }
                FunctionRef::Unknown => { unresolved.extend(field.name()); }
                _ => {}
            }
        }
        for (id, function) in self.functions.iter_mut() {
            // Every call to it is one we saw: it was assigned straight to a
            // variable (local or global) only ever called, which never holds
            // another function, and any field it was stored in is only ever
            // called, with no reads of a field by that name that could be it
            let in_variable = self.assigned_to.get(id).map(|&b| {
                let binding = &self.bindings[b];
                binding.types.function == FunctionRef::One(*id) && only_called[b]
            });
            let in_fields = self.stored_in_fields.get(id).map(|names| {
                !escaped.contains(id) && names.iter().all(|name| !unresolved.contains(name))
            });
            let all_calls_seen = match (in_variable, in_fields) {
                (None, None) => self.pool_callbacks.contains(id),
                (variable, fields) => variable.unwrap_or(true) && fields.unwrap_or(true),
            };
            let calls = self.call_arguments.get(id);
            for (i, parameter) in function.parameters.iter_mut().enumerate() {
                parameter.types = match (parameter.mandated, calls) {
                    (Some(mandated), _) => mandated,
                    (None, Some(calls)) if all_calls_seen => calls.get(i),
                    _ => Type::UNKNOWN,
                };
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
            let entry = self.current.clone();
            body(self);
            let exit = &self.current[..snapshot.bindings];
            if exit == entry.as_slice() {
                return
            }
            let mut carried = exit.to_vec();
            if pass >= LOOP_PASSES_BEFORE_WIDENING {
                // Each further pass makes at least one more variable unknown
                for (types, before) in carried.iter_mut().zip(&entry) {
                    if types != before {
                        *types = Type::UNKNOWN;
                    }
                }
            }
            self.restore(&snapshot);
            self.current.copy_from_slice(&carried);
        }
    }

    fn push_scope(&mut self, conditional: bool, end: TextSize) {
        self.scopes.push(Scope { names: HashMap::new(), conditional, end });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn new_binding(&mut self, name: &str, kind: BindingKind, declaration: Option<TextRange>, types: Type, function: usize, scope_depth: usize) -> BindingId {
        let id = self.bindings.len();
        self.bindings.push(Binding {
            name: name.to_string(),
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
            types.named = NamedRef::Table(self.location(token.text_range().start()));
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
        if let Some(&id) = self.globals.get(name) {
            return id;
        }
        let id = self.new_binding(name, BindingKind::Global, None, Type::default(), MAIN_CHUNK, 0);
        self.current[id] = Type::UNKNOWN;
        self.globals.insert(name.to_string(), id);
        self.globals_created.push(name.to_string());
        id
    }

    fn is_global(&self, name: &str) -> bool {
        !self.scopes.iter().any(|s| s.names.contains_key(name))
    }

    fn read(&mut self, token: &SyntaxToken) -> Type {
        let id = self.resolve(token.text());
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
    /// created in the same order on every walk, so the ids line up
    fn prior_binding_types(&self, id: BindingId) -> Type {
        self.prior
            .and_then(|prior| prior.bindings.get(id))
            .filter(|prior| prior.name == self.bindings[id].name)
            .map_or(Type::UNKNOWN, |prior| prior.types)
    }

    /// True for a global the file never assigns (as far as this walk has seen
    /// on the first walk, anywhere on the second), so presets can apply
    fn is_unassigned_global(&self, id: BindingId) -> bool {
        let binding = &self.bindings[id];
        let prior = self.prior.and_then(|p| p.bindings.get(id)).filter(|p| p.name == binding.name);
        binding.kind == BindingKind::Global && !binding.assigned && prior.is_none_or(|p| !p.assigned)
    }

    fn builtin_global(&self, id: BindingId) -> Option<Type> {
        if !self.is_unassigned_global(id) {
            return None
        }
        self.definitions.global(&self.bindings[id].name)
    }

    fn write(&mut self, token: &SyntaxToken, types: Type) {
        let id = self.resolve(token.text());
        let binding = &mut self.bindings[id];
        binding.types |= types;
        binding.assigned = true;
        binding.reassigned = true;
        let innermost_conditional = self.scopes.iter().rposition(|s| s.conditional);
        let replaces = binding.function == self.function
            && innermost_conditional.is_none_or(|depth| binding.scope_depth >= depth);
        if replaces {
            self.current[id] = types;
        } else {
            self.current[id] |= types;
        }
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: true, callee: false, call_returns: None });
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
                let after = s.syntax().descendants_with_tokens()
                    .filter_map(|e| e.into_token())
                    .filter(|t| !matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment))
                    .last()
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
                        self.assigned_to.insert(self.function_id(def), binding);
                    }
                }
            }
            Statement::Assign(s) => {
                let targets = s.variable_list().map(|v| v.identifiers()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, targets.len());
                for (i, (target, types)) in targets.iter().zip(types).enumerate() {
                    match target.simple_name() {
                        Some(name) => {
                            self.write(&name, types);
                            if let Some(Expression::Function(def)) = expressions.get(i) {
                                let binding = self.resolve(name.text());
                                self.assigned_to.insert(self.function_id(def), binding);
                            }
                        }
                        None => match field_access(target.syntax()) {
                            Some((base, field)) => {
                                let base = self.expression(&base);
                                self.field_write(base, field.text(), types);
                                self.field_reference(target.syntax(), &field, types, true);
                            }
                            // `t[k] = v` isn't tracked, but still reads `t` and `k`
                            None => self.identifier_parts(target.syntax()),
                        },
                    }
                }
            }
            Statement::FunctionCall(call) => {
                self.function_call(call);
            }
            Statement::Do(s) => self.block(s.block(), false),
            Statement::While(s) => self.loop_body(|w| {
                if let Some(condition) = s.condition() {
                    w.expression(&condition);
                }
                w.block(s.block(), true);
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
                for (i, branch) in s.if_branches().iter().enumerate() {
                    if let Some(condition) = branch.expression() {
                        // An `elseif` condition only runs if the ones before failed
                        self.short_circuit_depth += usize::from(i > 0);
                        self.expression(&condition);
                        self.short_circuit_depth -= usize::from(i > 0);
                    }
                    self.block(branch.block(), true);
                }
                if let Some(branch) = s.else_branch() {
                    self.block(branch.block(), true);
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
                for e in &expressions {
                    self.expression(e);
                }
                // `for object in pool:EnumerateActive()`
                let enumerated = match expressions.as_slice() {
                    [Expression::FunctionCall(call)] => self.enumerated.get(&self.location(call.syntax().text_range().start())).copied(),
                    _ => None,
                };
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                self.loop_body(|w| {
                    w.push_scope(true, s.syntax().text_range().end());
                    for (i, name) in names.iter().enumerate() {
                        if let (0, Some(object)) = (i, enumerated) {
                            w.declare(name, BindingKind::Local, object);
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
                        self.assigned_to.insert(self.function_id(def), binding);
                    }
                    self.function_body(def, None);
                } else if let Some(identifier) = def.identifier() {
                    match identifier.simple_name() {
                        Some(name) => {
                            self.write(&name, Type::function(self.function_id(def)));
                            let binding = self.resolve(name.text());
                            self.assigned_to.insert(self.function_id(def), binding);
                            self.function_body(def, None);
                        }
                        // `function a.b:c()` assigns field `c` of `a.b`
                        None => {
                            let owner = self.function_name_owner(&identifier);
                            if let Some((owner, field)) = &owner {
                                let function = Type::function(self.function_id(def));
                                self.field_write(*owner, field.text(), function);
                                self.field_reference(identifier.syntax(), field, function, true);
                            }
                            // `self` is the table the method is defined on
                            let self_type = identifier.is_call_to_self().then(|| {
                                let owner = owner.map_or(Type::UNKNOWN, |(owner, _)| owner);
                                match owner.named {
                                    // Unknown, so assume a table, with fields of its own
                                    NamedRef::None | NamedRef::Unknown => Type::table(self.function_id(def)),
                                    // A mixin: `self` is the WoW object it's mixed into, with
                                    // the mixin's fields too
                                    NamedRef::Table(at) => match self.prior.and_then(|prior| prior.mixed_into.get(&at)) {
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
                            let pool_mixed_parameters = self.pool_objects.get(&made).and_then(|object| object.named.identity())
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
            owner = self.field_read(owner, name.text());
            self.field_reference(identifier.syntax(), name, owner, false);
        }
        Some((owner, last.clone()))
    }

    /// `access` is the code reaching the field (`a.b` for `b`), or for a table
    /// constructor's key, the field entry
    fn field_reference(&mut self, access: &SyntaxNode, name: &SyntaxToken, types: Type, write: bool) {
        self.field_references.push(FieldReference { file: self.file, range: name.text_range(), access: access.clone(), types, write, callee: false, call_returns: None });
    }

    /// The field read just recorded is the function being called
    fn last_field_is_callee(&mut self) {
        if let Some(field) = self.field_references.last_mut() {
            field.callee = true;
        }
    }

    /// The type of field `name` of a value of type `base`
    fn field_read(&self, base: Type, name: &str) -> Type {
        if let Some(declared) = self.definitions.field(base, name) {
            return declared
        }
        // Writes can come later in the file, so only a previous walk knows them all
        let Some(prior) = self.prior else { return Type::UNKNOWN };
        let own = match base.named {
            NamedRef::Table(id) | NamedRef::Instance { at: id, .. } => self.own_field(prior, id, name, 0),
            NamedRef::One(_) | NamedRef::Extended(_) => None,
            // Not a table we know, so nothing to go on
            NamedRef::None | NamedRef::Unknown => return Type::UNKNOWN,
        };
        // What the table's own code (or a mixin's) sets wins. Writes to tables of
        // unknown identity are only a fallback: one field name is reused by many
        // unrelated tables (`entry.texts.cooldown` and `pools.cooldown`), so
        // merging them in would lose what's known about this one
        match own {
            Some(own) => own,
            None => prior.any_table_fields.get(name).copied().unwrap_or(Type::UNKNOWN),
        }
    }

    /// A field of a table made in the files: its own, else one it got from its
    /// mixins (the last mixin first, as later ones overwrite earlier ones)
    fn own_field(&self, prior: &Analysis, table: TableId, name: &str, depth: usize) -> Option<Type> {
        if let Some(own) = prior.fields.get(&table).and_then(|fields| fields.get(name)) {
            return Some(*own)
        }
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return None
        }
        prior.mixins.get(&table)?.iter().rev().find_map(|mixin| {
            let mixin = mixin.named.identity()?;
            self.own_field(prior, mixin, name, depth + 1)
        })
    }

    fn field_write(&mut self, base: Type, name: &str, value: Type) {
        if let FunctionRef::One(function) = value.function {
            self.stored_in_fields.entry(function).or_default().insert(name.to_string());
        }
        let field = match base.named {
            NamedRef::Table(id) | NamedRef::Instance { at: id, .. } => self.fields.entry(id).or_default().entry(name.to_string()).or_default(),
            // Could be any table, or shared by every object of a WoW type
            NamedRef::One(_) | NamedRef::Extended(_) | NamedRef::Unknown => self.any_table_fields.entry(name.to_string()).or_default(),
            NamedRef::None => return,
        };
        *field |= value;
    }

    fn function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>) -> FunctionId {
        let id = self.function_id(def);
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true, def.syntax().text_range().end());
        let outer_function_scope = std::mem::replace(&mut self.function_scope, self.scopes.len() - 1);
        let outer_short_circuit = std::mem::take(&mut self.short_circuit_depth);
        self.returns.push(None);
        self.function_ranges.push((def.syntax().text_range(), self.function));
        self.fresh.push(None);
        if let Some(self_type) = self_type {
            let depth = self.scopes.len() - 1;
            // Implicit, so there's no token to declare it at
            let id = self.new_binding("self", BindingKind::Parameter, None, self_type, self.function, depth);
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
                binding
            })
            .collect();
        let block = def.block();
        if let Some(block) = &block {
            self.statements(block);
        }
        let returned = self.returns.pop().flatten();
        self.function_ranges.pop();
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
        self.functions.insert(id, FunctionInfo { parameters, method: self_type.is_some(), varargs, returns, fresh_returns });
        id
    }

    fn prior_parameter_types(&self, id: FunctionId, index: usize) -> Type {
        self.prior
            .and_then(|prior| prior.functions.get(&id))
            .and_then(|function| function.parameters.get(index))
            .map_or(Type::UNKNOWN, |parameter| parameter.types)
    }

    /// The function being called, as far as this walk knows it
    fn known_function(&self, function: FunctionRef) -> Option<&FunctionInfo> {
        match function {
            FunctionRef::One(id) => self.functions.get(&id).or_else(|| self.prior?.functions.get(&id)),
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

    fn returns_of(&self, id: FunctionId) -> Values {
        // Not walked yet on this walk: defined later in the file, or a recursive call
        let function = self.functions.get(&id).or_else(|| self.prior?.functions.get(&id));
        function.map_or_else(Values::unknown, |f| f.returns.clone())
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
                Expression::VarArgs(_) if last => match self.addon_arguments() {
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
            Expression::Identifier(identifier) => match identifier.simple_name() {
                Some(name) => self.read(&name),
                None => match field_access(identifier.syntax()) {
                    // `a.b`: field `b` of whatever `a` is
                    Some((base, field)) => {
                        let base = self.expression(&base);
                        let types = self.field_read(base, field.text());
                        self.field_reference(identifier.syntax(), &field, types, false);
                        types
                    }
                    None => {
                        self.identifier_parts(identifier.syntax());
                        Type::UNKNOWN
                    }
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
            Expression::VarArgs(_) => self.addon_arguments().map_or(Type::UNKNOWN, |arguments| arguments.first()),
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
                Expression::Identifier(i) if !is_method && i.simple_name().is_some() => {
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
                self.field_reference(call.syntax(), &method, types, false);
                self.last_field_is_callee();
                callee_read = Some(CalleeRead::Field(self.field_references.len() - 1));
                self.check_method(base, &method);
                Some(types)
            }
            (callee, _) => callee,
        };
        let arguments = call.arguments();
        let argument_types = self.expression_values(&arguments);
        let Some(callee) = callee.filter(|c| c.set.contains(TypeSet::FUNCTION)) else {
            return Values::unknown()
        };
        let Some(function) = self.known_function(callee.function) else { return Values::unknown() };
        let required: Vec<Option<Type>> = (0..arguments.len()).map(|i| function.mandated(i)).collect();
        // Whether the call passes a template, which can add members
        let template = function.parameters.iter()
            .position(|p| p.name.to_ascii_lowercase().contains("template"))
            .and_then(|i| arguments.get(i))
            .is_some_and(|a| !matches!(a, Expression::Literal(l) if l.is_nil()));
        let documented = function.returns.clone();
        let fresh_returns = function.fresh_returns.clone();
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
                self.call_arguments.entry(id)
                    .and_modify(|all| *all = all.union(&parameter_types))
                    .or_insert(parameter_types.clone());
                let mut returns = self.returns_of(id);
                for (slot, fresh) in fresh_returns.iter().enumerate() {
                    if let (Some(fresh), Some(&value)) = (fresh, returns.values.get(slot)) {
                        returns.values[slot] = self.fresh_object(call, slot, value, fresh, &parameter_types);
                    }
                }
                returns
            }
            _ => match self.select(callee, &arguments, &argument_types) {
                Some(selected) => selected,
                None => match self.create_frame(call, callee, &arguments, &argument_types) {
                    Some(frame) => frame,
                    None => match self.create_pool(call, callee, &arguments, &argument_types) {
                        Some(pool) => pool,
                        None => self.mix(call, callee, &arguments, &argument_types).unwrap_or_else(|| documented.clone()),
                    },
                },
            },
        };
        if let (Some(receiver), Some(method)) = (receiver, call.method_name()) {
            returns = self.object_method(call, receiver, method.text(), template, &argument_types, returns);
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
    fn object_method(&mut self, call: &FunctionCall, receiver: Type, name: &str, template: bool, arguments: &Values, mut returns: Values) -> Values {
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
                    *self.parents.entry(at).or_default() |= arguments.first();
                }
            }
            "GetParent" => {
                // What it was made with or given, if it was made in the files
                if let Some(parent) = parent.and_then(|at| self.known_parent(at)) {
                    returns = Values { values: vec![parent], rest: Type::NIL };
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
                first.named = NamedRef::Instance { named: child, at, extended: template };
                *self.parents.entry(at).or_default() |= receiver;
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
        let id = match receiver.named {
            NamedRef::One(id) | NamedRef::Instance { named: id, extended: false, .. } => id,
            _ => return,
        };
        if receiver.set.without(OBJECTS | TypeSet::NIL) != TypeSet::NEVER {
            return
        }
        let Some(named) = self.definitions.named_type(id).filter(|named| named.has_methods) else { return };
        let name = method.text();
        if self.definitions.field(receiver, name).is_some() || prior.function_fields.contains(name) {
            return
        }
        let message = format!("{} has no method `{}`", named.name, name);
        self.diagnostics.push(Diagnostic { file: self.file, range: method.text_range(), message });
    }

    /// `CreateFrame("Button", ...)` gives a `Button`: the frame type named by a
    /// literal first argument, if it's a kind of `Frame`. A template (the fourth
    /// argument) can add Lua methods, so its frames aren't checked for missing ones
    fn create_frame(&mut self, call: &FunctionCall, callee: Type, arguments: &[Expression], argument_types: &Values) -> Option<Values> {
        if self.definitions.global("CreateFrame") != Some(callee) {
            return None
        }
        let named = self.frame_type(arguments.first())?;
        let template = arguments.get(3).is_some_and(|a| !matches!(a, Expression::Literal(l) if l.is_nil()));
        let at = self.location(call.syntax().text_range().start());
        // The third argument, or nil without one
        *self.parents.entry(at).or_default() |= argument_types.get(2);
        let named = NamedRef::Instance { named, at, extended: template };
        Some(Values { values: vec![Type { named, ..Type::of(TypeSet::TABLE) }], rest: Type::NIL })
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
        self.pool_objects.get(&pool).or_else(|| self.prior?.pool_objects.get(&pool)).copied()
    }

    /// `CreateFramePool`, `CreateTexturePool`, `CreateFontStringPool` and
    /// `CreateObjectPool` make a pool, whose objects are all one object here:
    /// of the type the pool makes (or its create function returns), made with
    /// the pool's parent. Its init and reset functions are called with it, so
    /// the fields and mixins they give it are what `Acquire` returns
    fn create_pool(&mut self, call: &FunctionCall, callee: Type, expressions: &[Expression], arguments: &Values) -> Option<Values> {
        let is = |name: &str| self.definitions.global(name) == Some(callee);
        let at = self.location(call.syntax().text_range().start());
        let object_at = Location { slot: POOL_OBJECT, ..at };
        let has = |i: usize| expressions.get(i).is_some_and(|a| !matches!(a, Expression::Literal(l) if l.is_nil()));
        // A template can add members, as with `CreateFrame`
        let region = |named: Option<NamedId>, template: bool| -> Option<Type> {
            let named = NamedRef::Instance { named: named?, at: object_at, extended: template };
            Some(Type { named, ..Type::of(TypeSet::TABLE) })
        };
        // The object, its parent, then each callback's argument position and
        // whether it's given the pool before the object
        let (object, parent, callbacks): (Type, Option<Type>, &[(usize, bool)]) = if is("CreateFramePool") {
            (region(self.frame_type(expressions.first()), has(2))?, Some(arguments.get(1)), &[(3, true), (5, false)])
        } else if is("CreateTexturePool") {
            (region(self.definitions.named_type_id("Texture"), has(3))?, Some(arguments.get(0)), &[(4, true)])
        } else if is("CreateFontStringPool") {
            (region(self.definitions.named_type_id("FontString"), has(3))?, Some(arguments.get(0)), &[(4, true)])
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
            *self.parents.entry(object_at).or_default() |= parent;
        }
        let pool = self.pool_type(at);
        self.pool_objects.insert(at, object);
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
        self.call_arguments.entry(id)
            .and_modify(|all| *all = all.union(&arguments))
            .or_insert(arguments);
        let Some(Expression::Identifier(identifier)) = expression else {
            // A function written right there, which nothing else can call
            if matches!(expression, Some(Expression::Function(_))) {
                self.pool_callbacks.insert(id);
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
        // From the previous walk, which saw every `SetParent`, and this one so
        // far. Parents only gain types, so the two together are never wrong
        let prior = self.prior.and_then(|prior| prior.parents.get(&at)).copied();
        let current = self.parents.get(&at).copied();
        match (prior, current) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default() | b.unwrap_or_default()),
        }
    }

    /// Whether a table or object was made by the function being walked
    fn made_here(&self, at: TableId) -> bool {
        at.slot == 0 && at.file == self.file && self.function_ranges.last().is_some_and(|(range, _)| range.contains(at.offset))
    }

    /// The innermost function being walked that made a table or object (or a
    /// pool whose object it is), as its number in the walk
    fn maker(&self, at: TableId) -> Option<usize> {
        if at.file != self.file || at.slot & !POOL_OBJECT != 0 {
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
        self.mixins.insert(at, mixins);
        if let Some(parent) = self.known_parent(fresh.made) {
            *self.parents.entry(at).or_default() |= parent;
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
                self.mixins.insert(object_at, mixins);
                if let Some(parent) = inner.named.identity().and_then(|inner| self.known_parent(inner)) {
                    *self.parents.entry(object_at).or_default() |= parent;
                }
            }
            self.pool_objects.insert(at, object);
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
            let recorded = self.parameter_mixins.entry(at).or_default();
            for parameter in parameters {
                if !recorded.contains(&parameter) {
                    recorded.push(parameter);
                }
            }
        }
        for mixin in mixins {
            self.record_mixed_into(*mixin, object.named);
        }
        // Only a table or object made in the files can keep track of its mixins
        if let Some(at) = object.named.identity() {
            let recorded = self.mixins.entry(at).or_default();
            for mixin in mixins {
                if !recorded.contains(mixin) {
                    recorded.push(*mixin);
                }
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
        let recorded = self.mixed_into.entry(mixin).or_default();
        if !recorded.contains(&object) {
            recorded.push(object);
        }
    }

    /// What `mixin` was mixed into on this walk, following tables made from it
    /// to what they were mixed into
    fn mixin_target(&self, mixin: TableId, depth: usize) -> MixinTarget {
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return MixinTarget::Nothing
        }
        let Some(objects) = self.mixed_into.get(&mixin) else { return MixinTarget::Nothing };
        objects.iter().fold(MixinTarget::Nothing, |target, &object| {
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
        for field in node.children().filter(|n| n.kind() == SyntaxKind::Field) {
            let mut in_brackets = false;
            let mut after_assign = false;
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
                    NodeOrToken::Node(n) if n.kind() == SyntaxKind::Identifier && !in_brackets && !after_assign => {
                        key = Identifier::cast(n).and_then(|i| i.simple_name());
                    }
                    NodeOrToken::Node(n) => {
                        if let Some(e) = Expression::cast(n) {
                            let value = self.expression(&e);
                            if let (Some(key), true) = (&key, after_assign) {
                                self.field_write(table, key.text(), value);
                                self.field_reference(&key.parent().unwrap_or_else(|| field.clone()), key, value, true);
                            }
                        }
                    }
                }
            }
        }
        table
    }

    fn binary_expression(&mut self, b: &BinaryExpression) -> Type {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        // Anything after an `and`/`or` might not run
        let mut short_circuited = false;
        for part in b.flatten() {
            match part {
                BinaryPart::Operand(e) => {
                    self.short_circuit_depth += usize::from(short_circuited);
                    operands.push(self.expression(&e));
                    self.short_circuit_depth -= usize::from(short_circuited);
                }
                BinaryPart::Operator(op) => {
                    short_circuited |= matches!(op, Operator::And | Operator::Or);
                    operators.push(op);
                }
            }
        }
        if operands.len() != operators.len() + 1 {
            // Syntax error somewhere inside
            return Type::UNKNOWN;
        }
        let mut position = 0;
        climb(&operands, &operators, &mut position, 0)
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

/// Precedence climbing over operand types. Associativity doesn't change the
/// result type for any operator, so everything is treated as left associative.
fn climb(operands: &[Type], operators: &[Operator], position: &mut usize, min_precedence: u8) -> Type {
    let mut left = operands[*position];
    while let Some(&op) = operators.get(*position) {
        let p = precedence(op);
        if p <= min_precedence {
            break;
        }
        *position += 1;
        let right = climb(operands, operators, position, p);
        left = combine(op, left, right);
    }
    left
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
            .filter(|r| file.binding(r).name == name)
            .map(|r| file.describe(r.types))
            .collect()
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
        let y = file.references.iter().find(|r| file.binding(r).name == "y").unwrap();
        assert!(y.types.set.contains(TypeSet::NUMBER | TypeSet::STRING), "{}", y.types.set);
        assert_eq!(y.types.set, TypeSet::UNKNOWN);
        assert_eq!(file.bindings.iter().filter(|b| b.name == "y").count(), 1);
    }

    #[test]
    fn loop_passes_leave_one_set_of_records() {
        let file = analyse("local x = 1 while c do local y = x x = 's' print(Global) end");
        assert_eq!(file.bindings.iter().filter(|b| b.name == "y").count(), 1);
        assert_eq!(file.bindings.iter().filter(|b| b.name == "Global").count(), 1);
        assert_eq!(file.references.iter().filter(|r| file.binding(r).name == "x").count(), 3);
        assert_eq!(file.references.iter().filter(|r| file.binding(r).name == "print").count(), 1);
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
        let qs: Vec<_> = file.references.iter().filter(|r| file.binding(r).name == "q").collect();
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
        assert_eq!(types_of("local function f(...) return ... end local a = f()", "a"), ["unknown"]);
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
        assert_eq!(builtin_types_of("local x = string['format']()", "x"), ["unknown"]);
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
        // Different named types, or a named type and a plain table, are just a table
        assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or GetTexture()", "f"), ["table"]);
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
    fn constructor_fields() {
        assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.count", "a"), ["number"]);
        assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.name", "a"), ["string"]);
        assert_eq!(types_of("local t = { f = function() return true end } local a = t.f()", "a"), ["true"]);
        // Positional and bracketed entries aren't fields by name
        assert_eq!(types_of("local t = { 1, ['k'] = 2 } local a = t.k", "a"), ["unknown"]);
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), in_addon })
            .collect();
        Analysis::analyse(&files, &Arc::new(Definitions::default()))
    }

    /// Type at each use of `name` in file `file`
    fn types_in(analysis: &Analysis, file: u32, name: &str) -> Vec<String> {
        analysis.references.iter()
            .filter(|r| r.file == FileId(file) && analysis.binding(r).name == name)
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
                .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), in_addon: true })
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), in_addon: true })
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), in_addon: true })
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
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), in_addon: true })
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
        assert_eq!(analysis.binding(first).name, "a");
        assert_eq!(analysis.binding(second).name, "b");
        assert!(analysis.reference_at(FileId(2), TextSize::from(6)).is_none());
    }

    /// Hover text for each use of the field reached by `path`, in source order
    fn field_hovers(analysis: &Analysis, path: &str) -> Vec<String> {
        analysis.field_references.iter()
            .filter(|f| f.path() == path)
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
            "values": {"UIParent": "Frame"},
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
        let source = "local b = CreateFrame('Button', nil, UIParent) b:SetParent(CreateFrame('Button')) local p = b:GetParent()";
        assert_eq!(widget_types_of(source, "p"), ["table"]);
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
    fn missing_methods_only_when_sure() {
        // Made from a template, which can add methods
        assert!(warnings("local f = CreateFrame('Frame', nil, nil, 'BackdropTemplate') f:SetBackdrop()").is_empty());
        // Not one known WoW type
        assert!(warnings("local f = c and CreateFrame('Frame') or 's' f:Nope()").is_empty());
        assert!(warnings("local f = c and CreateFrame('Frame') or {} f:Nope()").is_empty());
        assert!(warnings("local t = {} t:Nope()").is_empty());
        assert!(warnings("unknown:Nope()").is_empty());
        // A type without known methods, like a data structure
        assert!(warnings("local k = GetItemKey() k:Nope()").is_empty());
        // Recorded once, even in loops
        assert_eq!(warnings("local f = CreateFrame('Frame') while c do f:Nope() end").len(), 1);
    }

    #[test]
    fn reference_lookup() {
        let source = "local x = 5\nlocal y = x";
        let file = analyse(source);
        let r = file.reference_at(FileId(0), TextSize::from(22)).expect("reference at x");
        assert_eq!(file.binding(r).name, "x");
        assert!(!r.write);
        assert!(file.reference_at(FileId(0), TextSize::from(3)).is_none());
    }
}
