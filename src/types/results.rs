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

//! What an analysis finds and keeps: functions' signatures and summaries,
//! bindings and references, and diagnostics

use super::*;

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
    /// How what it's passed reaches fields and other functions, and what it
    /// reads of fields reaches them (see `Analysis::field_loops`)
    pub loop_edges: Vec<LoopEdge>,
    /// The parameters it returns as they are, as `return text` does
    pub returned_parameters: Vec<usize>,
}

/// A step a value takes from one function's parameter to another's
/// through a field, for finding loops through fields (see
/// `Analysis::field_loops`). A field is by the tables it's on, or none for
/// one on a table that can't be identified, and its name
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoopEdge {
    /// `x.f = p`: parameter `p` written to field `f`
    ParameterToField(usize, Vec<TableId>, String),
    /// `x.f = g(...)`: what `g` returns written to field `f`, which is what
    /// `g` was passed when it returns a parameter
    ReturnToField(FunctionId, Vec<TableId>, String),
    /// `g(x.f)`: field `f` passed for `g`'s parameter
    FieldToParameter(Vec<TableId>, String, FunctionId, usize),
    /// `g(p)`: parameter `p` passed on for `g`'s parameter
    ParameterToParameter(usize, FunctionId, usize),
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
pub(super) struct Signature<'a>(&'a FunctionInfo);

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
    pub(super) fn signature(&self) -> Signature<'_> {
        Signature(self)
    }

    /// What the function requires of the argument at `index`
    pub(super) fn mandated(&self, index: usize) -> Option<Type> {
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
    pub(super) function: usize,
    pub(super) scope_depth: usize,
    pub(super) assigned: bool,
    /// Written after its declaration (so far, on this walk)
    pub(super) reassigned: bool,
    /// How many loops were being walked where it was declared
    pub(super) loop_depth: usize,
    /// The furthest place the function declaring it writes it, its
    /// declaration included
    pub(super) last_write: TextSize,
    /// Whether a function inside the one declaring it writes it
    pub(super) closure_written: bool,
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
    pub(super) access: crate::syntax::SyntaxNodePtr,
    /// The token naming it, kept to read the name from without finding it in
    /// the tree, which walks needed for every field reference
    pub(super) token: rowan::GreenToken,
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
    /// A read whose value is only tested or worked out from, as in `if
    /// t.f then` or `not t.f`, so a function it holds goes nowhere else
    pub tested: bool,
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
    pub(super) fn name(&self) -> &str {
        let text = self.token.text();
        // `"b"` in `a["b"]`
        string_field_name(text).unwrap_or(text)
    }

    /// How the field was reached, without whitespace or comments, like
    /// `addonTable.hiddenTable` or `addonTable["hiddenTable"]`
    /// `root` is its file's tree
    pub(super) fn path(&self, root: Option<&SyntaxNode>) -> String {
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
pub(super) fn add_sites(to: &mut Vec<Site>, sites: &[Site]) {
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
