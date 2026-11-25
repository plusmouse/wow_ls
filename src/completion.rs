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

//! What could be typed at a position: the names in scope, or after `a.` and
//! `a:`, the members of whatever `a` is

use std::collections::HashMap;

use rowan::{NodeOrToken, TextSize};

use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use crate::types::{Analysis, BindingKind, FileId, TableId, Type, TypeSet, Values};

const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Local,
    Parameter,
    Global,
    /// A member reached with `.`
    Field,
    /// A function member reached with `:`
    Method,
    Keyword,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub kind: CandidateKind,
    /// Unknown for a keyword
    pub types: Type,
}

#[derive(Debug, Default)]
pub struct Completions {
    pub candidates: Vec<Candidate>,
    /// Typing more could bring in names that aren't here, so the list should
    /// be asked for again rather than just filtered. True when globals were
    /// narrowed down to the ones matching what's been typed
    pub incomplete: bool,
}

/// What could be typed at `offset` in `file`. `root` and `text` must be the
/// text `analysis` was made from
pub fn complete(analysis: &Analysis, file: FileId, root: &SyntaxNode, text: &str, offset: TextSize) -> Completions {
    let offset_index = usize::from(offset).min(text.len());
    if inside_string_or_comment(root, offset) {
        return Completions::default()
    }
    let before = &text[..offset_index];
    let prefix_start = before.rfind(|c: char| !is_name_char(c)).map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
    let prefix = &before[prefix_start..];
    // The digits of a number, not a name
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return Completions::default()
    }
    let separator_at = before[..prefix_start].trim_end_matches([' ', '\t', '\r', '\n']).len();
    let separator = before[..separator_at].chars().next_back();
    match separator {
        // `..` is concatenation, so what follows is an expression
        Some('.') if before[..separator_at - 1].ends_with('.') => names(analysis, file, offset, prefix),
        Some(c @ ('.' | ':')) => {
            let candidates = base_type(analysis, file, root, TextSize::from((separator_at - 1) as u32))
                .map(|base| members(analysis, base, c == ':'))
                .unwrap_or_default();
            Completions { candidates, incomplete: false }
        }
        _ => names(analysis, file, offset, prefix),
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Inside a string or comment, where nothing is completed. At the very end of
/// a string, past its closing quote, isn't inside
fn inside_string_or_comment(root: &SyntaxNode, offset: TextSize) -> bool {
    root.token_at_offset(offset).any(|token| {
        let range = token.text_range();
        match token.kind() {
            SyntaxKind::Comment => range.start() < offset,
            SyntaxKind::String => range.start() < offset && offset < range.end(),
            _ => false,
        }
    })
}

fn matches(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len() && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The locals in scope, then the globals and keywords starting with `prefix`
fn names(analysis: &Analysis, file: FileId, offset: TextSize, prefix: &str) -> Completions {
    // The innermost declaration of each name, as it hides the others
    let mut locals: HashMap<&str, (TextSize, Candidate)> = HashMap::new();
    for binding in &analysis.bindings {
        let Some(visible) = binding.visible else { continue };
        if binding.file != file || visible.is_empty() || !visible.contains_inclusive(offset) || binding.kind == BindingKind::Global {
            continue
        }
        let kind = if binding.kind == BindingKind::Parameter { CandidateKind::Parameter } else { CandidateKind::Local };
        let candidate = Candidate { name: binding.name.to_string(), kind, types: binding.types };
        match locals.get(&*binding.name) {
            Some((start, _)) if *start > visible.start() => {}
            _ => { locals.insert(&binding.name, (visible.start(), candidate)); }
        }
    }
    let mut candidates: Vec<Candidate> = locals.into_values().map(|(_, c)| c).collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    // Thousands of globals are known, so only those matching what's typed are
    // given, and none until something is
    let incomplete = true;
    if prefix.is_empty() {
        return Completions { candidates, incomplete }
    }
    let mut seen: std::collections::HashSet<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let mut globals: Vec<Candidate> = Vec::new();
    // Globals the files assign, then the preset ones they don't
    for binding in &analysis.bindings {
        if binding.kind == BindingKind::Global && binding.assigned() && matches(&binding.name, prefix) && seen.insert(binding.name.to_string()) {
            globals.push(Candidate { name: binding.name.to_string(), kind: CandidateKind::Global, types: binding.types });
        }
    }
    // Those of addons depended on before the preset ones, which they can replace
    for (name, &types) in &analysis.layer.globals {
        if matches(name, prefix) && seen.insert(name.clone()) {
            globals.push(Candidate { name: name.clone(), kind: CandidateKind::Global, types });
        }
    }
    for (name, types) in analysis.definitions.globals() {
        if matches(name, prefix) && seen.insert(name.to_string()) {
            globals.push(Candidate { name: name.to_string(), kind: CandidateKind::Global, types });
        }
    }
    globals.sort_by(|a, b| a.name.cmp(&b.name));
    candidates.extend(globals);
    candidates.extend(KEYWORDS.iter()
        .filter(|k| matches(k, prefix) && !seen.contains(**k))
        .map(|k| Candidate { name: k.to_string(), kind: CandidateKind::Keyword, types: Type::UNKNOWN }));
    Completions { candidates, incomplete }
}

/// The type of the code ending just before the `.` or `:` at `separator`: a
/// name (`frame.`, `a.b.`), a call (`CreateFrame("Frame"):`) or an index
/// (`colors[1]:`)
fn base_type(analysis: &Analysis, file: FileId, root: &SyntaxNode, separator: TextSize) -> Option<Type> {
    let mut token = root.token_at_offset(separator).left_biased()?;
    // Whitespace (or the separator itself, if the offset is inside it) before it
    while token.text_range().end() > separator || matches!(token.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment) {
        token = token.prev_token()?;
    }
    match token.kind() {
        SyntaxKind::Name => name_type(analysis, file, &token).map(|(ty, _)| ty),
        SyntaxKind::RightBracket => call_type(analysis, file, &token),
        SyntaxKind::RightSquareBracket => index_type(analysis, file, root, &token),
        _ => None,
    }
}

/// What indexing gives, for the index ending at `bracket`: the field a
/// string key names (`t["name"]`), else what the table holds
fn index_type(analysis: &Analysis, file: FileId, root: &SyntaxNode, bracket: &SyntaxToken) -> Option<Type> {
    // Its `[`, past any brackets inside the key
    let mut depth = 0;
    let mut open = bracket.prev_token()?;
    loop {
        match open.kind() {
            SyntaxKind::RightSquareBracket => depth += 1,
            SyntaxKind::LeftSquareBracket if depth == 0 => break,
            SyntaxKind::LeftSquareBracket => depth -= 1,
            _ => {}
        }
        open = open.prev_token()?;
    }
    // A string key names a field, which the analysis recorded at the key
    let key = open.next_token().filter(|t| t.kind() == SyntaxKind::String && t.next_token().as_ref() == Some(bracket));
    if let Some(field) = key.and_then(|key| analysis.field_reference_at(file, key.text_range().start()).filter(|f| f.range == key.text_range())) {
        return Some(field.types)
    }
    let base = base_type(analysis, file, root, open.text_range().start())?;
    let contents = analysis.contents(base.named.identity()?)?;
    Some(contents.values)
}

/// The type at a name the analysis recorded, and for a callee, what calling it there returns
fn name_type(analysis: &Analysis, file: FileId, token: &SyntaxToken) -> Option<(Type, Option<Values>)> {
    let range = token.text_range();
    if let Some(r) = analysis.reference_at(file, range.start()).filter(|r| r.range == range) {
        return Some((r.types, r.call_returns.clone()))
    }
    analysis.field_reference_at(file, range.start())
        .filter(|f| f.range == range)
        .map(|f| (f.types, f.call_returns.clone()))
}

/// The first value returned by the call whose argument list ends at `bracket`
fn call_type(analysis: &Analysis, file: FileId, bracket: &SyntaxToken) -> Option<Type> {
    let arguments = bracket.parent().filter(|n| n.kind() == SyntaxKind::ArgumentList)?;
    let call = arguments.parent().filter(|n| n.kind() == SyntaxKind::FunctionCall)?;
    // The method name of `a:b()`, or the last name of the callee `a.b()`
    let callee = call.children_with_tokens()
        .take_while(|e| e.as_node() != Some(&arguments))
        .filter_map(|e| match e {
            NodeOrToken::Token(t) if t.kind() == SyntaxKind::Name => Some(t),
            NodeOrToken::Node(n) if n.kind() == SyntaxKind::Identifier => last_name(&n),
            _ => None,
        })
        .last()?;
    let (ty, call_returns) = name_type(analysis, file, &callee)?;
    if let Some(returns) = call_returns {
        return Some(returns.first())
    }
    analysis.function_info(ty.function).map(|f| f.returns.first())
}

fn last_name(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::Name)
        .last()
}

/// The members of a value of type `ty`: those its definitions give, and the
/// fields the files give it (or its mixins). Only functions for a `:` call
fn members(analysis: &Analysis, ty: Type, methods_only: bool) -> Vec<Candidate> {
    // A string's members are the `string` library's
    let ty = analysis.definitions.member_source(ty);
    // Any of several objects has the members all of them have
    let objects = match ty.named {
        crate::types::NamedRef::Several(_) => ty.named.alternatives(),
        named => vec![named],
    };
    let mut found: Option<HashMap<String, Type>> = None;
    for object in objects {
        let mut own: HashMap<String, Type> = HashMap::new();
        if let Some(at) = object.identity() {
            own_fields(analysis, at, &mut own, 0);
        }
        if let Some(id) = object.named_id() {
            for (name, types) in analysis.definitions.members(id) {
                own.entry(name.to_string()).or_insert(types);
            }
        }
        found = Some(match found {
            None => own,
            Some(found) => found.into_iter()
                .filter_map(|(name, types)| Some((name.clone(), types | *own.get(&name)?)))
                .collect(),
        });
    }
    let found = found.unwrap_or_default();
    let mut candidates: Vec<Candidate> = found.into_iter()
        .filter(|(_, types)| !methods_only || types.set.contains(TypeSet::FUNCTION))
        .map(|(name, types)| {
            let kind = if methods_only { CandidateKind::Method } else { CandidateKind::Field };
            Candidate { name, kind, types }
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates
}

/// A table's own fields, then those of its mixins, last mixin first as later
/// ones overwrite earlier ones
fn own_fields(analysis: &Analysis, at: TableId, found: &mut HashMap<String, Type>, depth: usize) {
    // Deep enough for any real chain of mixins, and stops a loop
    if depth > 16 {
        return
    }
    // What the files give it, then what an addon depended on gives it
    let layer = &analysis.layer;
    for (name, &types) in analysis.fields.get(&at).into_iter().chain(layer.fields.get(&at)).flatten() {
        found.entry(name.clone()).or_insert(types);
    }
    for mixin in analysis.mixins.get(&at).or_else(|| layer.mixins.get(&at)).into_iter().flatten().rev() {
        if let Some(mixin) = mixin.named.identity() {
            own_fields(analysis, mixin, found, depth + 1);
        }
    }
    // Then what the metatable's `__index` table gives, as a class's methods
    let field = |table: TableId, name: &str| analysis.fields.get(&table).and_then(|f| f.get(name))
        .or_else(|| layer.fields.get(&table)?.get(name))
        .copied();
    let index = analysis.metatables.get(&at).or_else(|| layer.metatables.get(&at))
        .and_then(|metatable| metatable.named.identity())
        .and_then(|metatable| field(metatable, "__index"))
        .and_then(|index| index.named.identity());
    if let Some(index) = index {
        own_fields(analysis, index, found, depth + 1);
    }
}

/// A function's signature, or a value's type, for showing beside a candidate
pub fn detail(analysis: &Analysis, candidate: &Candidate) -> Option<String> {
    match candidate.kind {
        CandidateKind::Keyword => None,
        _ => Some(analysis.describe(candidate.types)),
    }
}

/// Whether a candidate is sure to be a function, for how it's shown
pub fn is_function(candidate: &Candidate) -> bool {
    candidate.types.set == TypeSet::FUNCTION
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::definitions::Definitions;
    use crate::syntax::syntax::Generator;

    /// Completions at `|` in `source`
    fn complete_at(source: &str) -> Completions {
        complete_with(source, &Arc::new(crate::definitions::load(None)))
    }

    fn complete_with(source: &str, definitions: &Arc<Definitions>) -> Completions {
        let offset = source.find('|').expect("no cursor");
        let text = source.replacen('|', "", 1);
        let root = SyntaxNode::new_root(Generator::new(&text).process_all());
        let analysis = Analysis::analyse_file(&root, definitions);
        complete(&analysis, FileId(0), &root, &text, TextSize::from(offset as u32))
    }

    fn names_of(completions: &Completions) -> Vec<&str> {
        completions.candidates.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn locals_in_scope() {
        let c = complete_at("local a = 1\nlocal function f(p)\n  local b = 2\n  |\nend\nlocal later = 3\n");
        let names = names_of(&c);
        assert!(names.contains(&"a") && names.contains(&"b") && names.contains(&"p") && names.contains(&"f"), "{names:?}");
        assert!(!names.contains(&"later"), "{names:?}");
    }

    #[test]
    fn locals_end_with_their_scope() {
        let c = complete_at("do\n  local inner = 1\nend\n|");
        assert!(!names_of(&c).contains(&"inner"));
    }

    #[test]
    fn local_not_visible_in_own_initialiser() {
        let c = complete_at("local x = |");
        assert!(!names_of(&c).contains(&"x"));
    }

    #[test]
    fn innermost_local_wins() {
        let c = complete_at("local x = 'a'\ndo\n  local x = 1\n  |\nend");
        let x: Vec<_> = c.candidates.iter().filter(|c| c.name == "x").collect();
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].types.set, TypeSet::NUMBER);
    }

    #[test]
    fn no_globals_until_typed() {
        let c = complete_at("|");
        assert!(c.candidates.iter().all(|c| c.kind != CandidateKind::Global));
        assert!(c.incomplete);
    }

    #[test]
    fn globals_matching_prefix() {
        let c = complete_at("createf|");
        let names = names_of(&c);
        assert!(names.contains(&"CreateFrame"), "{names:?}");
        assert!(c.candidates.iter().all(|c| c.name.to_lowercase().starts_with("createf")), "{names:?}");
    }

    #[test]
    fn assigned_globals_and_keywords() {
        let c = complete_at("MyAddonGlobal = 1\nlocal x = My|");
        assert!(names_of(&c).contains(&"MyAddonGlobal"));
        let c = complete_at("loc|");
        assert!(names_of(&c).contains(&"local"));
    }

    #[test]
    fn table_fields() {
        let c = complete_at("local t = { a = 1 }\nt.b = 'x'\nlocal y = t.|");
        assert_eq!(names_of(&c), ["a", "b"]);
        assert!(!c.incomplete);
    }

    #[test]
    fn fields_on_half_typed_statement() {
        let c = complete_at("local t = { a = 1 }\nt.|\nlocal z = 2");
        assert_eq!(names_of(&c), ["a"]);
    }

    #[test]
    fn nested_fields() {
        let c = complete_at("local t = { inner = { deep = 1 } }\nprint(t.inner.|)");
        assert_eq!(names_of(&c), ["deep"]);
    }

    #[test]
    fn methods_only_after_colon() {
        let c = complete_at("local t = { n = 1 }\nfunction t:Go() end\nt:|");
        assert_eq!(names_of(&c), ["Go"]);
        assert_eq!(c.candidates[0].kind, CandidateKind::Method);
    }

    #[test]
    fn object_methods_from_definitions() {
        let c = complete_at("local f = CreateFrame(\"Frame\")\nf:|\n");
        let names = names_of(&c);
        assert!(names.contains(&"SetPoint") && names.contains(&"Show"), "{names:?}");
    }

    #[test]
    fn data_table_fields_from_definitions() {
        let c = complete_at("local info = C_Container.GetContainerItemInfo(0, 1)\ninfo.|");
        let names = names_of(&c);
        assert!(names.contains(&"stackCount") && names.contains(&"hyperlink"), "{names:?}");
    }

    #[test]
    fn call_result_members() {
        let c = complete_at("CreateFrame(\"Frame\"):|");
        assert!(names_of(&c).contains(&"SetPoint"));
    }

    #[test]
    fn namespace_members() {
        let c = complete_at("string.|");
        assert!(names_of(&c).contains(&"format"));
    }

    #[test]
    fn class_methods_through_metatable() {
        let c = complete_at("local C = {} C.__index = C\nfunction C:go() end\nlocal o = setmetatable({}, C)\no:|");
        assert!(names_of(&c).contains(&"go"), "{:?}", names_of(&c));
    }

    #[test]
    fn string_members() {
        let c = complete_at("local s = 'x'\ns:|");
        let names = names_of(&c);
        assert!(names.contains(&"upper") && names.contains(&"format"), "{names:?}");
    }

    #[test]
    fn members_all_of_several_objects_have() {
        let c = complete_at("local a = CreateFrame('Button') local b = CreateFrame('Frame') a.both = 1 b.both = 2 a.one = 3\nlocal x = c and a or b\nx:|");
        let names = names_of(&c);
        assert!(names.contains(&"SetPoint") && !names.contains(&"Click"));
        let c = complete_at("local a = CreateFrame('Button') local b = CreateFrame('Frame') a.both = 1 b.both = 2 a.one = 3\nlocal x = c and a or b\nx.|");
        let names = names_of(&c);
        assert!(names.contains(&"both") && !names.contains(&"one"));
    }

    #[test]
    fn mixin_fields() {
        let c = complete_at("local M = {}\nfunction M:Hello() end\nlocal t = CreateFromMixins(M)\nt:|");
        assert!(names_of(&c).contains(&"Hello"), "{:?}", names_of(&c));
    }

    #[test]
    fn methods_of_tables_blizzards_functions_make() {
        // Each `CreateColor` call a new table with `ColorMixin`: any of them
        // has its methods, however many
        let colors: Vec<String> = (0..9).map(|i| format!("[{i}] = CreateColor({i}/255, 1, 1)")).collect();
        let c = complete_at(&format!("local qualityColors = {{ {} }}\nlocal a = qualityColors[1]\na:GetRG|", colors.join(", ")));
        assert!(names_of(&c).contains(&"GetRGB"), "{:?}", names_of(&c));
    }

    #[test]
    fn members_of_what_a_table_holds() {
        let colors: Vec<String> = (0..9).map(|i| format!("[{i}] = CreateColor({i}/255, 1, 1)")).collect();
        let c = complete_at(&format!("local qualityColors = {{ {} }}\nqualityColors[0]:|", colors.join(", ")));
        assert!(names_of(&c).contains(&"GetRGB"), "{:?}", names_of(&c));
        // Through fields, calls and other indexes, and a key naming a field
        let c = complete_at("local ns = { list = { { a = 1 } } }\nlocal function f() return ns.list end\nns.list[1].|");
        assert_eq!(names_of(&c), ["a"]);
        let c = complete_at("local ns = { list = { { a = 1 } } }\nlocal function f() return ns.list end\nf()[ns.list[1].a].|");
        assert_eq!(names_of(&c), ["a"]);
        let c = complete_at("local t = { inner = { b = 1 } }\nt[\"inner\"].|");
        assert_eq!(names_of(&c), ["b"]);
    }

    #[test]
    fn nothing_in_strings_comments_or_after_concat_dots() {
        assert!(complete_at("local s = \"a.|\"").candidates.is_empty());
        assert!(complete_at("-- t.|").candidates.is_empty());
        assert!(complete_at("local n = 1.|").candidates.is_empty());
        let c = complete_at("local a = 1\nlocal s = 'x' .. |");
        assert!(names_of(&c).contains(&"a"));
    }
}
