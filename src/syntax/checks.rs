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

//! Errors Lua reports when it compiles a file that parses: rules about where
//! things can be rather than what they look like

use rowan::NodeOrToken;

use super::syntax::{Error, ErrorKind, SyntaxElement, SyntaxKind, SyntaxNode};

/// Every such error in the tree, in source order
pub fn check(root: &SyntaxNode) -> Vec<Error> {
    let mut errors = Vec::new();
    for element in root.descendants_with_tokens() {
        match element {
            NodeOrToken::Node(node) => match node.kind() {
                SyntaxKind::Block => last_statements(&node, &mut errors),
                SyntaxKind::ForCountLoop => for_bounds(&node, &mut errors),
                _ => {}
            },
            NodeOrToken::Token(token) => match token.kind() {
                SyntaxKind::BreakKeyword if !in_loop(&token.parent()) => {
                    errors.push(error(&NodeOrToken::Token(token), ErrorKind::BreakOutsideLoop));
                }
                // A function's own `...` is a ParameterVarArgs, so this is a use
                SyntaxKind::TripleDot if !varargs_available(&token.parent()) => {
                    errors.push(error(&NodeOrToken::Token(token), ErrorKind::VarArgsOutsideVarArgFunction));
                }
                _ => {}
            },
        }
    }
    errors.sort_by_key(|e| (e.start, e.end));
    errors
}

fn error(element: &SyntaxElement, kind: ErrorKind) -> Error {
    let range = element.text_range();
    Error { start: usize::from(range.start()), end: usize::from(range.end()), kind }
}

/// `return` and `break` can only end a block, as anything after them could
/// never run
fn last_statements(block: &SyntaxNode, errors: &mut Vec<Error>) {
    let mut significant = block.children_with_tokens().filter(|e| !matches!(e.kind(),
        SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment | SyntaxKind::Semicolon
        // Already reported by the parser
        | SyntaxKind::Invalid));
    while let Some(element) = significant.next() {
        let kind = match element.kind() {
            SyntaxKind::ReturnStatement => ErrorKind::CodeAfterReturn,
            SyntaxKind::BreakKeyword => ErrorKind::CodeAfterBreak,
            _ => continue,
        };
        if let Some(after) = significant.next() {
            errors.push(error(&after, kind));
        }
        return
    }
}

/// `for i = start, limit [, step]`
fn for_bounds(node: &SyntaxNode, errors: &mut Vec<Error>) {
    let Some(bounds) = node.children().find(|n| n.kind() == SyntaxKind::ExpressionList) else { return };
    let count = bounds.children().count();
    if !(2..=3).contains(&count) {
        errors.push(error(&NodeOrToken::Node(bounds), ErrorKind::ExpectingForLimit));
    }
}

/// Inside a loop, without a function boundary in between
fn in_loop(start: &Option<SyntaxNode>) -> bool {
    start.iter().flat_map(|n| n.ancestors())
        .take_while(|n| n.kind() != SyntaxKind::FunctionDefinition)
        .any(|n| matches!(n.kind(), SyntaxKind::WhileLoop | SyntaxKind::RepeatUntilLoop | SyntaxKind::ForCountLoop | SyntaxKind::ForInLoop))
}

/// In the main chunk, which always has `...`, or a function declared with it
fn varargs_available(start: &Option<SyntaxNode>) -> bool {
    let function = start.iter().flat_map(|n| n.ancestors()).find(|n| n.kind() == SyntaxKind::FunctionDefinition);
    match function {
        None => true,
        Some(function) => function.children()
            .filter(|n| n.kind() == SyntaxKind::ParameterList)
            .flat_map(|list| list.children_with_tokens())
            .any(|e| e.kind() == SyntaxKind::ParameterVarArgs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::syntax::Generator;

    fn kinds(source: &str) -> Vec<ErrorKind> {
        let mut generator = Generator::new(source);
        let root = SyntaxNode::new_root(generator.process_all());
        assert!(generator.errors().is_empty(), "parse errors in {source:?}: {:?}", generator.errors());
        check(&root).into_iter().map(|e| e.kind).collect()
    }

    #[test]
    fn valid_code_is_fine() {
        for source in [
            "do return end print(1)",
            "local function f() return 1; end",
            "while true do break end",
            "while true do break; end",
            "for i = 1, 2 do if i then break end end",
            "repeat break until true",
            "for k in pairs({}) do break end",
            "local function f(...) return ... end",
            "local function f(a, ...) local t = { ... } end",
            "local t = { ... }",
            "for i = 1, 10, 2 do end",
            "return",
        ] {
            assert_eq!(kinds(source), [], "{source}");
        }
    }

    #[test]
    fn code_after_return_or_break() {
        assert_eq!(kinds("return 1 print(2)"), [ErrorKind::CodeAfterReturn]);
        assert_eq!(kinds("local function f() return; local x = 1 end"), [ErrorKind::CodeAfterReturn]);
        assert_eq!(kinds("while true do break print(1) end"), [ErrorKind::CodeAfterBreak]);
    }

    #[test]
    fn break_outside_a_loop() {
        assert_eq!(kinds("break"), [ErrorKind::BreakOutsideLoop]);
        assert_eq!(kinds("if true then break end"), [ErrorKind::BreakOutsideLoop]);
        // A function in a loop is its own body
        assert_eq!(kinds("while true do local f = function() break end end"), [ErrorKind::BreakOutsideLoop]);
    }

    #[test]
    fn varargs_need_a_vararg_function() {
        assert_eq!(kinds("local function f() return ... end"), [ErrorKind::VarArgsOutsideVarArgFunction]);
        // The enclosing function's `...` doesn't reach into another
        assert_eq!(kinds("local function f(...) return function() return ... end end"), [ErrorKind::VarArgsOutsideVarArgFunction]);
    }

    #[test]
    fn numeric_for_bounds() {
        assert_eq!(kinds("for i = 1 do end"), [ErrorKind::ExpectingForLimit]);
        assert_eq!(kinds("for i = 1, 2, 3, 4 do end"), [ErrorKind::ExpectingForLimit]);
    }
}
