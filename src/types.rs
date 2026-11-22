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
//! Known gaps: table fields and function return types are not tracked yet.

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

impl fmt::Display for TypeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let all_values = Self::UNKNOWN.truthy();
        if self.contains(all_values) {
            return match (self.contains(Self::NIL), self.contains(Self::FALSE)) {
                (true, true) => write!(f, "unknown"),
                (false, true) => write!(f, "unknown (not nil)"),
                (true, false) => write!(f, "unknown (not false)"),
                (false, false) => write!(f, "unknown (truthy)"),
            };
        }
        if *self == Self::NEVER {
            return write!(f, "never");
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
        write!(f, "{}", names.join("|"))
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
    pub types: TypeSet,
    function: usize,
    scope_depth: usize,
    assigned: bool,
}

#[derive(Debug)]
pub struct Reference {
    pub range: TextRange,
    pub binding: BindingId,
    /// The type at this point: the value read, or the value assigned for a write
    pub types: TypeSet,
    pub write: bool,
}

#[derive(Debug, Default)]
pub struct FileTypes {
    pub bindings: Vec<Binding>,
    /// Sorted by position
    pub references: Vec<Reference>,
}

impl FileTypes {
    pub fn analyse(root: &SyntaxNode) -> FileTypes {
        let mut walker = Walker::default();
        if let Some(block) = Block::cast(root.clone()) {
            walker.push_scope(false);
            walker.statements(&block);
            walker.pop_scope();
        }
        walker.finish()
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
struct Walker {
    bindings: Vec<Binding>,
    /// Type of each binding at the current point of the walk
    current: Vec<TypeSet>,
    references: Vec<Reference>,
    /// References that read an upvalue, resolved once all assignments are known
    deferred: Vec<usize>,
    scopes: Vec<Scope>,
    globals: HashMap<String, BindingId>,
    /// Names added to `globals`, in order, so a loop pass can be undone
    globals_created: Vec<String>,
    function: usize,
    function_count: usize,
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

impl Walker {
    fn finish(mut self) -> FileTypes {
        for binding in self.bindings.iter_mut() {
            if binding.kind == BindingKind::Global && !binding.assigned {
                binding.types = TypeSet::UNKNOWN;
            }
        }
        for &index in &self.deferred {
            let binding = self.references[index].binding;
            self.references[index].types = self.bindings[binding].types;
        }
        self.references.sort_by_key(|r| r.range.start());
        FileTypes { bindings: self.bindings, references: self.references }
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
                        *types = TypeSet::UNKNOWN;
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

    fn new_binding(&mut self, name: &str, kind: BindingKind, declaration: Option<TextRange>, types: TypeSet, function: usize, scope_depth: usize) -> BindingId {
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

    fn declare(&mut self, token: &SyntaxToken, kind: BindingKind, types: TypeSet) -> BindingId {
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
        let id = self.new_binding(name, BindingKind::Global, None, TypeSet::NEVER, MAIN_CHUNK, 0);
        self.current[id] = TypeSet::UNKNOWN;
        self.globals.insert(name.to_string(), id);
        self.globals_created.push(name.to_string());
        id
    }

    fn is_global(&self, name: &str) -> bool {
        !self.scopes.iter().any(|s| s.names.contains_key(name))
    }

    fn read(&mut self, token: &SyntaxToken) -> TypeSet {
        let id = self.resolve(token.text());
        let index = self.references.len();
        let types = if self.bindings[id].function == self.function {
            self.current[id]
        } else {
            self.deferred.push(index);
            TypeSet::UNKNOWN
        };
        self.references.push(Reference { range: token.text_range(), binding: id, types, write: false });
        types
    }

    fn write(&mut self, token: &SyntaxToken, types: TypeSet) {
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
                        w.declare(&name, BindingKind::Local, TypeSet::NUMBER);
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
                        w.declare(name, BindingKind::Local, types);
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
                        self.declare(&name, BindingKind::Local, TypeSet::FUNCTION);
                    }
                    self.function_body(def, false);
                } else if let Some(identifier) = def.identifier() {
                    match identifier.simple_name() {
                        Some(name) => {
                            self.write(&name, TypeSet::FUNCTION);
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
                for e in s.expressions() {
                    self.expression(&e);
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

    fn function_body(&mut self, def: &FunctionDefinition, has_self: bool) {
        let outer_function = self.function;
        self.function_count += 1;
        self.function = self.function_count;
        self.push_scope(true);
        if has_self {
            let depth = self.scopes.len() - 1;
            // Implicit, so there's no token to declare it at
            let id = self.new_binding("self", BindingKind::Parameter, None, TypeSet::TABLE, self.function, depth);
            self.bindings[id].assigned = true;
            self.scopes[depth].names.insert("self".to_string(), id);
        }
        if let Some(params) = def.params() {
            for param in params.parameter_tokens() {
                self.declare(&param, BindingKind::Parameter, TypeSet::UNKNOWN);
            }
        }
        if let Some(block) = def.block() {
            self.statements(&block);
        }
        self.pop_scope();
        self.function = outer_function;
    }

    /// Types for `count` targets from an expression list, following Lua's
    /// multiple-value rules: the last call/`...` expands, missing values are nil
    fn expression_list(&mut self, expressions: &[Expression], count: usize) -> Vec<TypeSet> {
        let mut types: Vec<TypeSet> = expressions.iter().map(|e| self.expression(e)).collect();
        let expands = matches!(expressions.last(), Some(Expression::FunctionCall(_) | Expression::VarArgs(_)));
        let fill = if expands { TypeSet::UNKNOWN } else { TypeSet::NIL };
        types.resize(count.max(types.len()), fill);
        types.truncate(count);
        types
    }

    fn expression(&mut self, expression: &Expression) -> TypeSet {
        match expression {
            Expression::Literal(l) => {
                if l.get_string().is_some() {
                    TypeSet::STRING
                } else if l.get_number().is_some() {
                    TypeSet::NUMBER
                } else if l.is_nil() {
                    TypeSet::NIL
                } else if let Some(b) = l.get_bool() {
                    if b.trim() == "true" { TypeSet::TRUE } else { TypeSet::FALSE }
                } else {
                    TypeSet::UNKNOWN
                }
            }
            Expression::Function(def) => {
                self.function_body(def, false);
                TypeSet::FUNCTION
            }
            Expression::TableConstructor(t) => {
                self.table_constructor(t.syntax());
                TypeSet::TABLE
            }
            Expression::FunctionCall(call) => self.function_call(call),
            Expression::Identifier(identifier) => match identifier.simple_name() {
                Some(name) => self.read(&name),
                None => {
                    self.identifier_parts(identifier.syntax());
                    TypeSet::UNKNOWN
                }
            },
            Expression::GroupedExpression(g) => match g.get_expression() {
                Some(e) => self.expression(&e),
                None => TypeSet::UNKNOWN,
            },
            Expression::UnaryExpression(u) => {
                if let Some(operand) = u.operand() {
                    self.expression(&operand);
                }
                match u.kind() {
                    Operator::Not => TypeSet::BOOLEAN,
                    _ => TypeSet::NUMBER,
                }
            }
            Expression::BinaryExpression(b) => self.binary_expression(b),
            Expression::VarArgs(_) => TypeSet::UNKNOWN,
        }
    }

    fn function_call(&mut self, call: &FunctionCall) -> TypeSet {
        if let Some(callee) = call.callee() {
            self.expression(&callee);
        }
        for argument in call.arguments() {
            self.expression(&argument);
        }
        // Return types aren't tracked yet
        TypeSet::UNKNOWN
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

    fn binary_expression(&mut self, b: &BinaryExpression) -> TypeSet {
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
            return TypeSet::UNKNOWN;
        }
        let mut position = 0;
        climb(&operands, &operators, &mut position, 0)
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

fn combine(op: Operator, left: TypeSet, right: TypeSet) -> TypeSet {
    match op {
        // The right side only runs if the left can be falsy (`or`) or truthy (`and`)
        Operator::Or if left.falsy() == TypeSet::NEVER => left,
        Operator::Or => left.truthy() | right,
        Operator::And if left.truthy() == TypeSet::NEVER => left,
        Operator::And => left.falsy() | right,
        Operator::Concatenate => TypeSet::STRING,
        Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals
            | Operator::GreaterThanOrEquals | Operator::NotEquals | Operator::Equals => TypeSet::BOOLEAN,
        // Ignores metatables (__add and friends)
        _ => TypeSet::NUMBER,
    }
}

/// Precedence climbing over operand types. Associativity doesn't change the
/// result type for any operator, so everything is treated as left associative.
fn climb(operands: &[TypeSet], operators: &[Operator], position: &mut usize, min_precedence: u8) -> TypeSet {
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
            .map(|r| r.types.to_string())
            .collect()
    }

    #[test]
    fn literals() {
        assert_eq!(types_of("local x = 5", "x"), ["number"]);
        assert_eq!(types_of("local x = 'a'", "x"), ["string"]);
        assert_eq!(types_of("local x = true", "x"), ["true"]);
        assert_eq!(types_of("local x = nil", "x"), ["nil"]);
        assert_eq!(types_of("local x = {}", "x"), ["table"]);
        assert_eq!(types_of("local x = function() end", "x"), ["function"]);
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
        assert!(y.types.contains(TypeSet::NUMBER | TypeSet::STRING), "{}", y.types);
        assert_eq!(y.types, TypeSet::UNKNOWN);
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
        assert_eq!(types_of("function Baz() end local y = Baz", "y"), ["function"]);
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
        assert_eq!(types_of("local function f(a) end", "f"), ["function"]);
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
    fn reference_lookup() {
        let source = "local x = 5\nlocal y = x";
        let file = analyse(source);
        let r = file.reference_at(TextSize::from(22)).expect("reference at x");
        assert_eq!(file.binding(r).name, "x");
        assert!(!r.write);
        assert!(file.reference_at(TextSize::from(3)).is_none());
    }
}
