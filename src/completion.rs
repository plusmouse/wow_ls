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

use std::cell::Cell;
use std::collections::HashMap;

use rowan::{NodeOrToken, TextRange, TextSize};

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

/// How the text being completed differs from the text the analysis was made
/// from, so an analysis that's a few keystrokes old can still answer: one
/// span replaced, what's left between their common start and common end.
/// Positions outside it map back to the analysis's text; those inside it are
/// text the analysis never saw
#[derive(Debug)]
pub struct Edit {
    /// Where the span starts, in both texts
    start: usize,
    /// Where it ends in the analysis's text
    old_end: usize,
    /// Where it ends in the text now
    new_end: usize,
    /// The text now between them: what's been typed since the analysis
    typed: String,
    /// Whether something the completion needed was inside the span, so the
    /// analysis couldn't say what it is
    missed: Cell<bool>,
}

impl Edit {
    /// No change: the analysis was made from the text being completed
    pub fn none() -> Edit {
        Edit { start: usize::MAX, old_end: usize::MAX, new_end: usize::MAX, typed: String::new(), missed: Cell::new(false) }
    }

    /// The change from `old`, which the analysis was made from, to `new`
    pub fn between(old: &str, new: &str) -> Edit {
        let (o, n) = (old.as_bytes(), new.as_bytes());
        let mut start = o.iter().zip(n).take_while(|(a, b)| a == b).count();
        while !(old.is_char_boundary(start) && new.is_char_boundary(start)) {
            start -= 1;
        }
        // The common end can't reach back into the common start
        let most = o.len().min(n.len()) - start;
        let mut end = o.iter().rev().zip(n.iter().rev()).take(most).take_while(|(a, b)| a == b).count();
        while !(old.is_char_boundary(o.len() - end) && new.is_char_boundary(n.len() - end)) {
            end -= 1;
        }
        let new_end = n.len() - end;
        Edit { start, old_end: o.len() - end, new_end, typed: new[start..new_end].to_string(), missed: Cell::new(false) }
    }

    /// Whether the analysis couldn't answer, as what was asked about is newer
    /// than it. Then only a new analysis can
    pub fn missed(&self) -> bool {
        self.missed.get()
    }

    /// The cursor's place in the analysis's text. Inside the span it's where
    /// the span starts, which is in the same scopes as far as the analysis knows
    fn cursor(&self, offset: TextSize) -> TextSize {
        let offset = usize::from(offset);
        let mapped = if offset <= self.start {
            offset
        } else if offset >= self.new_end {
            offset - self.new_end + self.old_end
        } else {
            self.start
        };
        TextSize::from(mapped as u32)
    }

    /// Where the code at `range` was in the analysis's text, unless it's
    /// changed since: overlaps the span. Then the analysis can't answer
    fn range(&self, range: TextRange) -> Option<TextRange> {
        let mapped = self.unchanged(range);
        if mapped.is_none() {
            self.missed.set(true);
        }
        mapped
    }

    /// Whether what's been typed since the analysis names `name` other than
    /// at `at`, so it may declare or assign it, which the analysis doesn't know
    fn may_declare(&self, name: &str, at: TextRange) -> bool {
        let own = usize::from(at.start()).checked_sub(self.start).filter(|_| self.unchanged(at).is_none());
        self.typed.match_indices(name).any(|(i, _)| {
            let whole = !self.typed[..i].ends_with(is_name_char) && !self.typed[i + name.len()..].starts_with(is_name_char);
            whole && Some(i) != own
        })
    }

    /// As `range`, for code that can be understood some other way if it's changed
    fn unchanged(&self, range: TextRange) -> Option<TextRange> {
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        if end <= self.start {
            Some(range)
        } else if start >= self.new_end {
            let shift = |at: usize| TextSize::from((at - self.new_end + self.old_end) as u32);
            Some(TextRange::new(shift(start), shift(end)))
        } else {
            None
        }
    }
}

/// What could be typed at `offset` in `file`. `root` and `text` are the text
/// being completed, and `edit` how it differs from what `analysis` was made from
pub fn complete(analysis: &Analysis, file: FileId, root: &SyntaxNode, text: &str, offset: TextSize, edit: &Edit) -> Completions {
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
        Some('.') if before[..separator_at - 1].ends_with('.') => names(analysis, file, edit.cursor(offset), prefix),
        Some(c @ ('.' | ':')) => {
            let candidates = base_type(analysis, file, root, TextSize::from((separator_at - 1) as u32), edit)
                .map(|base| members(analysis, base, c == ':'))
                .unwrap_or_default();
            Completions { candidates, incomplete: false }
        }
        _ => names(analysis, file, edit.cursor(offset), prefix),
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

/// The locals in scope, then the globals and keywords starting with
/// `prefix`. `offset` is in the text `analysis` was made from
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
fn base_type(analysis: &Analysis, file: FileId, root: &SyntaxNode, separator: TextSize, edit: &Edit) -> Option<Type> {
    let mut token = root.token_at_offset(separator).left_biased()?;
    // Whitespace (or the separator itself, if the offset is inside it) before it
    while token.text_range().end() > separator || matches!(token.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment) {
        token = token.prev_token()?;
    }
    match token.kind() {
        SyntaxKind::Name => name_type(analysis, file, &token, edit).map(|(ty, _)| ty),
        SyntaxKind::RightBracket => call_type(analysis, file, &token, edit),
        SyntaxKind::RightSquareBracket => index_type(analysis, file, root, &token, edit),
        _ => None,
    }
}

/// What indexing gives, for the index ending at `bracket`: the field a
/// string key names (`t["name"]`), else what the table holds
fn index_type(analysis: &Analysis, file: FileId, root: &SyntaxNode, bracket: &SyntaxToken, edit: &Edit) -> Option<Type> {
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
    let key = key.and_then(|key| edit.range(key.text_range()));
    if let Some(field) = key.and_then(|key| analysis.field_reference_at(file, key.start()).filter(|f| f.range == key)) {
        return Some(field.types)
    }
    let base = base_type(analysis, file, root, open.text_range().start(), edit)?;
    let contents = analysis.contents(base.named.identity()?)?;
    Some(contents.values)
}

/// The type at a name the analysis recorded, and for a callee, what calling it there returns
fn name_type(analysis: &Analysis, file: FileId, token: &SyntaxToken, edit: &Edit) -> Option<(Type, Option<Values>)> {
    if let Some(range) = edit.unchanged(token.text_range()) {
        if let Some(r) = analysis.reference_at(file, range.start()).filter(|r| r.range == range) {
            return Some((r.types, r.call_returns.clone()))
        }
        if let Some(f) = analysis.field_reference_at(file, range.start()).filter(|f| f.range == range) {
            return Some((f.types, f.call_returns.clone()))
        }
    }
    // Nothing recorded there: typed since the analysis, or not an expression
    // then (a name on its own, before the `.` after it was typed, isn't a
    // statement). So what the name means there
    // Unless it's been declared or assigned since, which only analysing
    // again would find
    let found = by_name(analysis, file, token, edit);
    if found.is_none() && edit.may_declare(token.text(), token.text_range()) {
        edit.missed.set(true);
    }
    found.map(|ty| (ty, None))
}

/// What the name `token` means where it is, found by name rather than by a
/// reference recorded there: after `.` or `:`, the member of that name of what
/// comes before; else the innermost local of that name in scope, else the
/// global. A local's or field's type anywhere, not just there
fn by_name(analysis: &Analysis, file: FileId, token: &SyntaxToken, edit: &Edit) -> Option<Type> {
    let name = token.text();
    let mut before = token.prev_token();
    while let Some(t) = before.as_ref().filter(|t| matches!(t.kind(), SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment)) {
        before = t.prev_token();
    }
    if let Some(separator) = before.filter(|t| matches!(t.text(), "." | ":")) {
        let root = token.parent()?.ancestors().last()?;
        let base = base_type(analysis, file, &root, separator.text_range().start(), edit)?;
        return members(analysis, base, false).into_iter().find(|c| c.name == name).map(|c| c.types)
    }
    let at = edit.cursor(token.text_range().start());
    let local = analysis.bindings.iter()
        .filter(|b| b.file == file && b.kind != BindingKind::Global && &*b.name == name)
        .filter_map(|b| b.visible.filter(|v| !v.is_empty() && v.contains_inclusive(at)).map(|v| (v.start(), b.types)))
        .max_by_key(|&(start, _)| start);
    if let Some((_, types)) = local {
        return Some(types)
    }
    analysis.bindings.iter()
        .find(|b| b.kind == BindingKind::Global && b.assigned() && &*b.name == name)
        .map(|b| b.types)
        .or_else(|| analysis.layer.globals.get(name).copied())
        .or_else(|| analysis.definitions.global(name))
}

/// The first value returned by the call whose argument list ends at `bracket`
fn call_type(analysis: &Analysis, file: FileId, bracket: &SyntaxToken, edit: &Edit) -> Option<Type> {
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
    let (ty, call_returns) = name_type(analysis, file, &callee, edit)?;
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
        complete(&analysis, FileId(0), &root, &text, TextSize::from(offset as u32), &Edit::none())
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

    /// Completions at `|` in `source`, from an analysis of `analysed` (the
    /// text before the latest keystrokes), and whether it couldn't answer
    fn complete_stale(analysed: &str, source: &str) -> (Completions, bool) {
        let definitions = Arc::new(crate::definitions::load(None));
        let old = SyntaxNode::new_root(Generator::new(analysed).process_all());
        let analysis = Analysis::analyse_file(&old, &definitions);
        let offset = source.find('|').expect("no cursor");
        let text = source.replacen('|', "", 1);
        let root = SyntaxNode::new_root(Generator::new(&text).process_all());
        let edit = Edit::between(analysed, &text);
        let c = complete(&analysis, FileId(0), &root, &text, TextSize::from(offset as u32), &edit);
        (c, edit.missed())
    }

    #[test]
    fn from_an_analysis_of_earlier_text() {
        // A name being typed: the locals in scope where it is
        let (c, missed) = complete_stale("local apple = 1\nlocal function f()\n  local banana = 2\n  a\nend\nlocal cherry = 3", "local apple = 1\nlocal function f()\n  local banana = 2\n  app|\nend\nlocal cherry = 3");
        assert!(!missed);
        assert!(names_of(&c).contains(&"apple") && names_of(&c).contains(&"banana"), "{:?}", names_of(&c));
        assert!(!names_of(&c).contains(&"cherry"), "{:?}", names_of(&c));
        // Members of what was there before
        let (c, missed) = complete_stale("local t = { a = 1 }\nprint(t)", "local t = { a = 1 }\nprint(t.|)");
        assert!(!missed);
        assert_eq!(names_of(&c), ["a"]);
        // A name on its own isn't an expression, so the analysis has nothing
        // at it, but knows what the name means there
        let (c, missed) = complete_stale("local t = { a = 1 }\nt", "local t = { a = 1 }\nt.|");
        assert!(!missed);
        assert_eq!(names_of(&c), ["a"]);
        // And after an edit above it, which moved it
        let (c, missed) = complete_stale("local t = { a = 1 }\nlocal x = 2\nt.", "local t = { a = 1 }\nlocal xyz = 2\nt.|");
        assert!(!missed);
        assert_eq!(names_of(&c), ["a"]);
        // Names typed since, by what they mean where they are
        let (c, missed) = complete_stale("local t = { a = 1 }\n", "local t = { a = 1 }\nt.|");
        assert!(!missed);
        assert_eq!(names_of(&c), ["a"]);
        let (c, missed) = complete_stale("local te = { a = 1 }\nlocal tea = { b = 1 }\nte", "local te = { a = 1 }\nlocal tea = { b = 1 }\ntea.|");
        assert!(!missed);
        assert_eq!(names_of(&c), ["b"]);
        // The innermost local of the name, and fields of fields
        let (c, missed) = complete_stale("local t = { a = 1 }\nlocal function f()\n  local t = { inner = { b = 1 } }\n  \nend", "local t = { a = 1 }\nlocal function f()\n  local t = { inner = { b = 1 } }\n  t.inner.|\nend");
        assert!(!missed);
        assert_eq!(names_of(&c), ["b"]);
        // Globals, and those the definitions give
        let (c, missed) = complete_stale("G = { a = 1 }\n", "G = { a = 1 }\nG.|");
        assert!(!missed);
        assert_eq!(names_of(&c), ["a"]);
        let (c, missed) = complete_stale("", "C_Timer.|");
        assert!(!missed);
        assert!(names_of(&c).contains(&"After"), "{:?}", names_of(&c));
        // But not a local declared in what was typed since
        let (_, missed) = complete_stale("", "local t = { a = 1 }\nt.|");
        assert!(missed);
        let (_, missed) = complete_stale("local t = {}\n", "local t = {}\nt.b = {}\nt.b.|");
        assert!(missed);
        // While one the analysis doesn't know, nothing typed since could have
        // made known
        let (c, missed) = complete_stale("local t = {}\n", "local t = {}\nt.unknown.|");
        assert!(!missed);
        assert!(c.candidates.is_empty());
    }

    #[test]
    fn edit_between_texts() {
        let edit = Edit::between("abcdef", "abXYdef");
        assert_eq!((edit.start, edit.old_end, edit.new_end), (2, 3, 4));
        assert_eq!(edit.cursor(TextSize::from(4)), TextSize::from(3));
        assert_eq!(edit.cursor(TextSize::from(6)), TextSize::from(5));
        assert_eq!(edit.range(TextRange::new(0.into(), 2.into())), Some(TextRange::new(0.into(), 2.into())));
        assert_eq!(edit.range(TextRange::new(4.into(), 7.into())), Some(TextRange::new(3.into(), 6.into())));
        assert_eq!(edit.range(TextRange::new(1.into(), 3.into())), None);
        // Not splitting a character
        let edit = Edit::between("aé", "aè");
        assert_eq!((edit.start, edit.old_end, edit.new_end), (1, 3, 3));
        let edit = Edit::between("same", "same");
        assert_eq!(edit.range(TextRange::new(1.into(), 3.into())), Some(TextRange::new(1.into(), 3.into())));
    }
}
