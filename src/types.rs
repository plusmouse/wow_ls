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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum NamedRef {
    /// Can't be a table or userdata
    #[default]
    None,
    One(NamedId),
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
            _ => NamedRef::Unknown,
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
    pub parameters: Vec<Parameter>,
    /// The type of each extra argument, if the function takes `...`
    pub varargs: Option<Parameter>,
    pub returns: Values,
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
    function: usize,
    scope_depth: usize,
    assigned: bool,
    /// Written after its declaration (so far, on this walk)
    reassigned: bool,
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
}

impl FieldReference {
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
        let named = match ty.named {
            NamedRef::One(id) => self.definitions.named_type(id).filter(|named| !named.namespace),
            _ => None,
        };
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
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] => named.name.clone(),
                (_, _, "number") if numeric.is_some() => numeric.map_or_else(String::new, |n| n.name.clone()),
                _ => name.to_string(),
            })
            .collect();
        let described = names.join("|");
        if optional { described + "?" } else { described }
    }

    fn signature(&self, function: &FunctionInfo) -> String {
        let parameter = |p: &Parameter| match p.types {
            Type::UNKNOWN => p.name.clone(),
            types => format!("{}: {}", p.name, self.describe_shallow(types)),
        };
        let mut parameters: Vec<String> = function.parameters.iter().map(parameter).collect();
        if let Some(varargs) = &function.varargs {
            parameters.push(parameter(varargs));
        }
        let mut signature = format!("function({})", parameters.join(", "));
        let returns = &function.returns;
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
            return Some((format!("{} {}: {}", binding.kind, binding.name, self.describe(r.types)), r.range))
        }
        let field = self.field_reference_at(file, offset)?;
        Some((format!("field {}: {}", field.path(), self.describe(field.types)), field.range))
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

    pub fn binding(&self, reference: &Reference) -> &Binding {
        &self.bindings[reference.binding]
    }
}

struct Scope {
    names: HashMap<String, BindingId>,
    /// Code in this scope might not run (or might run many times)
    conditional: bool,
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
}

/// How much had been recorded before a loop pass, to discard that pass
struct Snapshot {
    bindings: usize,
    references: usize,
    field_references: usize,
    deferred: usize,
    globals_created: usize,
}

/// Every file's top level, which run one after another
const MAIN_CHUNK: usize = 0;

/// The table every file of an addon gets as the second value of `...`. Not a
/// real file position, so it can't clash with a table made in a file
const ADDON_NAMESPACE: TableId = Location { file: FileId(u32::MAX), offset: TextSize::new(0) };

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
                self.push_scope(false);
                self.statements(&block);
                self.pop_scope();
            }
        }
        self.finish()
    }

    fn location(&self, offset: TextSize) -> Location {
        Location { file: self.file, offset }
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
        Analysis {
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
        for (id, function) in self.functions.iter_mut() {
            // Directly assigned to a local that's only ever called, and never
            // given another function, so every call to it is one we saw
            let all_calls_seen = self.assigned_to.get(id).is_some_and(|&b| {
                let binding = &self.bindings[b];
                binding.kind == BindingKind::Local && binding.types.function == FunctionRef::One(*id) && only_called[b]
            });
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

    fn push_scope(&mut self, conditional: bool) {
        self.scopes.push(Scope { names: HashMap::new(), conditional });
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
        self.scopes[depth].names.insert(token.text().to_string(), id);
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: true, callee: false });
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
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: false, callee: false });
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
        self.references.push(Reference { file: self.file, range: token.text_range(), binding: id, types, write: true, callee: false });
    }

    fn block(&mut self, block: Option<Block>, conditional: bool) {
        if let Some(block) = block {
            self.push_scope(conditional);
            self.statements(&block);
            self.pop_scope();
        }
    }

    fn statements(&mut self, block: &Block) {
        for statement in block.statements() {
            self.statement(&statement);
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::LocalAssign(s) => {
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, names.len());
                for (i, (name, types)) in names.iter().zip(types).enumerate() {
                    let binding = self.declare(name, BindingKind::Local, types);
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
                w.push_scope(true);
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
                    w.push_scope(true);
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
                let names = s.name_list().map(|n| n.name_tokens()).unwrap_or_default();
                self.loop_body(|w| {
                    w.push_scope(true);
                    for (i, name) in names.iter().enumerate() {
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
        self.field_references.push(FieldReference { file: self.file, range: name.text_range(), access: access.clone(), types, write });
    }

    /// The type of field `name` of a value of type `base`
    fn field_read(&self, base: Type, name: &str) -> Type {
        if let Some(declared) = self.definitions.field(base, name) {
            return declared
        }
        // Writes can come later in the file, so only a previous walk knows them all
        let Some(prior) = self.prior else { return Type::UNKNOWN };
        let own = match base.named {
            NamedRef::Table(id) => prior.fields.get(&id).and_then(|fields| fields.get(name)).copied(),
            NamedRef::One(_) => None,
            // Not a table we know, so nothing to go on
            NamedRef::None | NamedRef::Unknown => return Type::UNKNOWN,
        };
        let any = prior.any_table_fields.get(name).copied();
        match (own, any) {
            (None, None) => Type::UNKNOWN,
            (own, any) => own.unwrap_or_default() | any.unwrap_or_default(),
        }
    }

    fn field_write(&mut self, base: Type, name: &str, value: Type) {
        let field = match base.named {
            NamedRef::Table(id) => self.fields.entry(id).or_default().entry(name.to_string()).or_default(),
            // Could be any table, or shared by every object of a WoW type
            NamedRef::One(_) | NamedRef::Unknown => self.any_table_fields.entry(name.to_string()).or_default(),
            NamedRef::None => return,
        };
        *field |= value;
    }

    fn function_body(&mut self, def: &FunctionDefinition, self_type: Option<Type>) -> FunctionId {
        let id = self.function_id(def);
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true);
        let outer_function_scope = std::mem::replace(&mut self.function_scope, self.scopes.len() - 1);
        let outer_short_circuit = std::mem::take(&mut self.short_circuit_depth);
        self.returns.push(None);
        if let Some(self_type) = self_type {
            let depth = self.scopes.len() - 1;
            // Implicit, so there's no token to declare it at
            let id = self.new_binding("self", BindingKind::Parameter, None, self_type, self.function, depth);
            self.bindings[id].assigned = true;
            self.scopes[depth].names.insert("self".to_string(), id);
        }
        let params = def.params();
        let parameters = params.as_ref().map(|p| p.parameter_tokens()).unwrap_or_default();
        let parameter_bindings: Vec<BindingId> = parameters.iter().enumerate()
            .map(|(i, param)| {
                let types = self.prior_parameter_types(id, i);
                self.declare(param, BindingKind::Parameter, types)
            })
            .collect();
        let block = def.block();
        if let Some(block) = &block {
            self.statements(block);
        }
        let returned = self.returns.pop().flatten();
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
        self.functions.insert(id, FunctionInfo { parameters, varargs, returns });
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
            let mandate = self.mandates.entry(id).or_insert(Some(*required));
            if *mandate != Some(*required) {
                *mandate = None;
            }
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
        let callee = call.callee().map(|callee| {
            let types = self.expression(&callee);
            if !is_method && matches!(&callee, Expression::Identifier(i) if i.simple_name().is_some()) {
                // The read `expression` just recorded
                if let Some(reference) = self.references.last_mut() {
                    reference.callee = true;
                }
            }
            types
        });
        // `a:m()` calls field `m` of `a`
        let callee = match (callee, call.method_name()) {
            (Some(base), Some(method)) => {
                let types = self.field_read(base, method.text());
                self.field_reference(call.syntax(), &method, types, false);
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
        let returns = match callee.function {
            FunctionRef::One(id) => {
                self.call_arguments.entry(id)
                    .and_modify(|all| *all = all.union(&argument_types))
                    .or_insert(argument_types);
                self.returns_of(id)
            }
            _ => match self.select(callee, &arguments, &argument_types) {
                Some(selected) => selected,
                None => function.returns.clone(),
            },
        };
        self.require_arguments(&arguments, &required);
        returns
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
        assert_eq!(types_of("function F(x) end F(1)", "x"), ["unknown"]);
        assert_eq!(types_of("local t = {} function t.f(x) end t.f(1)", "x"), ["unknown"]);
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
