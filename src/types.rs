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

//! Single-file type tracking for un-annotated Lua.
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
//! The file is walked twice. On the first walk, reading an upvalue or calling a
//! function defined further down gives unknown. The second walk uses the first
//! walk's final types for those, which is sound because the first walk can only
//! have over-approximated.
//!
//! Known gaps: table fields aren't tracked, so method calls (`a:m()`) and
//! calls through fields (`t.f()`) return unknown. Recursive calls return unknown.

use std::collections::HashMap;
use std::fmt;

use rowan::{NodeOrToken, TextRange, TextSize};

use crate::ast::*;
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
pub type FunctionId = TextSize;

/// Which function a value is, when it can be one
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FunctionRef {
    /// Can't be a function
    #[default]
    None,
    /// Exactly this function
    One(FunctionId),
    /// A function we can't pin down: a parameter, an unknown call result, or one
    /// of several functions
    Unknown,
}

impl FunctionRef {
    fn union(self, other: FunctionRef) -> FunctionRef {
        match (self, other) {
            (FunctionRef::None, x) | (x, FunctionRef::None) => x,
            (FunctionRef::One(a), FunctionRef::One(b)) if a == b => self,
            _ => FunctionRef::Unknown,
        }
    }
}

/// A value's possible types, and which function it is if it can be one.
/// `function` is None exactly when `set` doesn't contain FUNCTION
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Type {
    pub set: TypeSet,
    pub function: FunctionRef,
}

impl Type {
    pub const NIL: Type = Type { set: TypeSet::NIL, function: FunctionRef::None };
    pub const UNKNOWN: Type = Type { set: TypeSet::UNKNOWN, function: FunctionRef::Unknown };

    /// A type with no known function identity
    pub fn of(set: TypeSet) -> Type {
        let function = if set.contains(TypeSet::FUNCTION) { FunctionRef::Unknown } else { FunctionRef::None };
        Type { set, function }
    }
    pub fn function(id: FunctionId) -> Type {
        Type { set: TypeSet::FUNCTION, function: FunctionRef::One(id) }
    }
    pub fn union(self, other: Type) -> Type {
        Type { set: self.set | other.set, function: self.function.union(other.function) }
    }
    /// Functions are always truthy, so they're kept
    pub fn truthy(self) -> Type {
        Type { set: self.set.truthy(), function: self.function }
    }
    pub fn falsy(self) -> Type {
        Type::of(self.set.falsy())
    }
    pub fn without(self, other: TypeSet) -> Type {
        let set = self.set.without(other);
        let function = if set.contains(TypeSet::FUNCTION) { self.function } else { FunctionRef::None };
        Type { set, function }
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

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub parameters: Vec<String>,
    pub varargs: bool,
    pub returns: Values,
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
}

#[derive(Debug)]
pub struct Reference {
    pub range: TextRange,
    pub binding: BindingId,
    /// The type at this point: the value read, or the value assigned for a write
    pub types: Type,
    pub write: bool,
}

#[derive(Debug, Default)]
pub struct FileTypes {
    pub bindings: Vec<Binding>,
    /// Sorted by position
    pub references: Vec<Reference>,
    pub functions: HashMap<FunctionId, FunctionInfo>,
}

impl FileTypes {
    pub fn analyse(root: &SyntaxNode) -> FileTypes {
        let first = Walker::new(None).walk(root);
        Walker::new(Some(&first)).walk(root)
    }

    /// e.g. `number|nil` or `function(a, b) -> string`
    pub fn describe(&self, ty: Type) -> String {
        let names = ty.set.names();
        let signature = match ty.function {
            FunctionRef::One(id) => self.functions.get(&id).map(|f| self.signature(f)),
            _ => None,
        };
        let wrap = names.len() > 1;
        names.iter().map(|&name| match (&signature, name) {
            (Some(s), "function") if wrap && s.contains(" -> ") => format!("({s})"),
            (Some(s), "function") => s.clone(),
            _ => name.to_string(),
        }).collect::<Vec<_>>().join("|")
    }

    fn signature(&self, function: &FunctionInfo) -> String {
        let mut parameters = function.parameters.clone();
        if function.varargs {
            parameters.push("...".to_string());
        }
        let mut signature = format!("function({})", parameters.join(", "));
        let returns = &function.returns;
        // Not `describe`, which could recurse forever on `function f() return f end`
        let mut parts: Vec<String> = returns.values.iter().map(|t| t.set.to_string()).collect();
        if returns.rest != Type::NIL {
            parts.push("...".to_string());
        }
        if !parts.is_empty() {
            signature += " -> ";
            signature += &parts.join(", ");
        }
        signature
    }

    pub fn reference_at(&self, offset: TextSize) -> Option<&Reference> {
        let after = self.references.partition_point(|r| r.range.start() <= offset);
        let candidate = self.references.get(after.checked_sub(1)?)?;
        candidate.range.contains_inclusive(offset).then_some(candidate)
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
    prior: Option<&'a FileTypes>,
    bindings: Vec<Binding>,
    /// Type of each binding at the current point of the walk
    current: Vec<Type>,
    references: Vec<Reference>,
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
}

/// How much had been recorded before a loop pass, to discard that pass
struct Snapshot {
    bindings: usize,
    references: usize,
    deferred: usize,
    globals_created: usize,
}

const MAIN_CHUNK: usize = 0;

/// Passes after which a loop that still hasn't settled is widened: any variable
/// whose type is still changing becomes unknown. Without this, a loop already
/// terminates (each pass adds at least one bit to some variable's TypeSet), but
/// nested loops multiply their passes. Realistic code settles in 2-3 passes
const LOOP_PASSES_BEFORE_WIDENING: usize = 8;

impl<'a> Walker<'a> {
    fn new(prior: Option<&'a FileTypes>) -> Walker<'a> {
        Walker { prior, ..Walker::default() }
    }

    fn walk(mut self, root: &SyntaxNode) -> FileTypes {
        if let Some(block) = Block::cast(root.clone()) {
            self.push_scope(false);
            self.statements(&block);
            self.pop_scope();
        }
        self.finish()
    }

    fn finish(mut self) -> FileTypes {
        for binding in self.bindings.iter_mut() {
            if binding.kind == BindingKind::Global && !binding.assigned {
                binding.types = Type::UNKNOWN;
            }
        }
        for &index in &self.deferred {
            let binding = self.references[index].binding;
            self.references[index].types = self.bindings[binding].types;
        }
        self.references.sort_by_key(|r| r.range.start());
        FileTypes { bindings: self.bindings, references: self.references, functions: self.functions }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            bindings: self.bindings.len(),
            references: self.references.len(),
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
        });
        self.current.push(types);
        id
    }

    fn declare(&mut self, token: &SyntaxToken, kind: BindingKind, types: Type) -> BindingId {
        let depth = self.scopes.len() - 1;
        let id = self.new_binding(token.text(), kind, Some(token.text_range()), types, self.function, depth);
        self.bindings[id].assigned = true;
        self.scopes[depth].names.insert(token.text().to_string(), id);
        self.references.push(Reference { range: token.text_range(), binding: id, types, write: true });
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
        self.references.push(Reference { range: token.text_range(), binding: id, types, write: false });
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

    fn write(&mut self, token: &SyntaxToken, types: Type) {
        let id = self.resolve(token.text());
        let binding = &mut self.bindings[id];
        binding.types |= types;
        binding.assigned = true;
        let innermost_conditional = self.scopes.iter().rposition(|s| s.conditional);
        let replaces = binding.function == self.function
            && innermost_conditional.is_none_or(|depth| binding.scope_depth >= depth);
        if replaces {
            self.current[id] = types;
        } else {
            self.current[id] |= types;
        }
        self.references.push(Reference { range: token.text_range(), binding: id, types, write: true });
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
                for (name, types) in names.iter().zip(types) {
                    self.declare(name, BindingKind::Local, types);
                }
            }
            Statement::Assign(s) => {
                let targets = s.variable_list().map(|v| v.identifiers()).unwrap_or_default();
                let expressions = s.expression_list().map(|e| e.expressions()).unwrap_or_default();
                let types = self.expression_list(&expressions, targets.len());
                for (target, types) in targets.iter().zip(types) {
                    match target.simple_name() {
                        Some(name) => self.write(&name, types),
                        // `t.x = 1` reads `t`. Field types aren't tracked yet
                        None => self.identifier_parts(target.syntax()),
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
                for branch in s.if_branches() {
                    if let Some(condition) = branch.expression() {
                        self.expression(&condition);
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
                        self.declare(&name, BindingKind::Local, Type::function(function_id(def)));
                    }
                    self.function_body(def, false);
                } else if let Some(identifier) = def.identifier() {
                    match identifier.simple_name() {
                        Some(name) => {
                            self.write(&name, Type::function(function_id(def)));
                            self.function_body(def, false);
                        }
                        None => {
                            self.identifier_parts(identifier.syntax());
                            self.function_body(def, identifier.is_call_to_self());
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

    fn function_body(&mut self, def: &FunctionDefinition, has_self: bool) -> FunctionId {
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true);
        self.returns.push(None);
        if has_self {
            let depth = self.scopes.len() - 1;
            // Implicit, so there's no token to declare it at
            let id = self.new_binding("self", BindingKind::Parameter, None, Type::of(TypeSet::TABLE), self.function, depth);
            self.bindings[id].assigned = true;
            self.scopes[depth].names.insert("self".to_string(), id);
        }
        let params = def.params();
        let parameters = params.as_ref().map(|p| p.parameter_tokens()).unwrap_or_default();
        for param in &parameters {
            self.declare(param, BindingKind::Parameter, Type::UNKNOWN);
        }
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

        let id = function_id(def);
        self.functions.insert(id, FunctionInfo {
            parameters: parameters.iter().map(|p| p.text().to_string()).collect(),
            varargs: params.is_some_and(|p| p.ellipsis()),
            returns,
        });
        id
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
                Expression::VarArgs(_) if last => values.rest = Type::UNKNOWN,
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
            Expression::Function(def) => Type::function(self.function_body(def, false)),
            Expression::TableConstructor(t) => {
                self.table_constructor(t.syntax());
                Type::of(TypeSet::TABLE)
            }
            // `f()` in the middle of an expression only gives its first value
            Expression::FunctionCall(call) => self.function_call(call).first(),
            Expression::Identifier(identifier) => match identifier.simple_name() {
                Some(name) => self.read(&name),
                None => {
                    self.identifier_parts(identifier.syntax());
                    Type::UNKNOWN
                }
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
            Expression::VarArgs(_) => Type::UNKNOWN,
        }
    }

    fn function_call(&mut self, call: &FunctionCall) -> Values {
        let callee = call.callee().map(|callee| self.expression(&callee));
        for argument in call.arguments() {
            self.expression(&argument);
        }
        if call.method_name().is_some() {
            // `a:m()` calls a field of `a`, and fields aren't tracked yet
            return Values::unknown();
        }
        match callee {
            Some(Type { set, function: FunctionRef::One(id) }) if set.contains(TypeSet::FUNCTION) => self.returns_of(id),
            _ => Values::unknown(),
        }
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

    fn table_constructor(&mut self, node: &SyntaxNode) {
        for field in node.children().filter(|n| n.kind() == SyntaxKind::Field) {
            let mut in_brackets = false;
            for child in field.children_with_tokens() {
                match child {
                    NodeOrToken::Token(t) => match t.kind() {
                        SyntaxKind::LeftSquareBracket => in_brackets = true,
                        SyntaxKind::RightSquareBracket => in_brackets = false,
                        _ => {}
                    },
                    // `{name = value}`: the name is a key, not a variable
                    NodeOrToken::Node(n) if n.kind() == SyntaxKind::Identifier && !in_brackets => {}
                    NodeOrToken::Node(n) => {
                        if let Some(e) = Expression::cast(n) {
                            self.expression(&e);
                        }
                    }
                }
            }
        }
    }

    fn binary_expression(&mut self, b: &BinaryExpression) -> Type {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        for part in b.flatten() {
            match part {
                BinaryPart::Operand(e) => operands.push(self.expression(&e)),
                BinaryPart::Operator(op) => operators.push(op),
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

fn function_id(def: &FunctionDefinition) -> FunctionId {
    def.syntax().text_range().start()
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

    fn analyse(source: &str) -> FileTypes {
        let mut generator = Generator::new(source);
        let root = SyntaxNode::new_root(generator.process_all());
        FileTypes::analyse(&root)
    }

    /// Type at each use of `name`, in source order
    fn types_of(source: &str, name: &str) -> Vec<String> {
        let file = analyse(source);
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
        assert_eq!(types_of("local x = 1 while x do x = nil end", "x"), ["number", "number|nil", "nil"]);
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
        assert_eq!(y.types, Type::UNKNOWN);
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
        assert_eq!(types_of("local function f(x) if x then return 1 end end local y = f()", "y"), ["number|nil"]);
        assert_eq!(types_of("local function f(x) if x then return 1 else return 2 end end local y = f()", "y"), ["number"]);
        assert_eq!(types_of("local function f(x) if x then return 1 elseif z then return 2 end end local y = f()", "y"), ["number|nil"]);
        assert_eq!(types_of("local function f() do return 1 end end local y = f()", "y"), ["number"]);
        assert_eq!(types_of("local function f() while c do return 1 end end local y = f()", "y"), ["number|nil"]);
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
        assert_eq!(types_of("local t = {} function t:m() return 1 end local y = t:m()", "y"), ["unknown"]);
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
        assert_eq!(types_of("local function f(x) if x then return 1 end end", "f"), ["function(x) -> number|nil"]);
        assert_eq!(types_of("local function f() return 1 end local g = c and f", "g"), ["false|(function() -> number)|nil"]);
        assert_eq!(types_of("local function f() return f end", "f"), ["function() -> function", "function() -> function"]);
    }

    #[test]
    fn reference_lookup() {
        let source = "local x = 5\nlocal y = x";
        let file = analyse(source);
        let r = file.reference_at(TextSize::from(22)).expect("reference at x");
        assert_eq!(file.binding(r).name, "x");
        assert!(!r.write);
        assert!(file.reference_at(TextSize::from(3)).is_none());
    }
}
