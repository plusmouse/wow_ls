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

use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::{request, Diagnostic, DiagnosticSeverity, PublishDiagnosticsParams, Range, Uri};

/// Publishes a document's syntax errors, along with `warnings` (byte ranges
/// and messages) found by type analysis
pub fn publish(connection: &Connection, uri: Uri, text: &str, warnings: &[(rowan::TextRange, String)]) {
    let mut parser = crate::syntax::syntax::Generator::new(text);
    let _green_tree = parser.process_all();
    let errors = parser.errors();

    let mut diagnostics: Vec<Diagnostic> = Vec::with_capacity(errors.len() + warnings.len());
    let range = |start: usize, end: usize| {
        let range = rowan::TextRange::new(rowan::TextSize::from(start as u32), rowan::TextSize::from(end as u32));
        crate::lsp::position::range_of(text, range)
    };
    let diagnostic = |range: Range, severity: DiagnosticSeverity, message: String| Diagnostic {
        range,
        severity: Some(severity),
        code: None,
        code_description: None,
        source: Some(String::from("wow_ls")),
        message,
        tags: None,
        related_information: None,
        data: None,
    };

    for e in errors {
        diagnostics.push(diagnostic(range(e.start, e.end), DiagnosticSeverity::ERROR, format!("{:?}", e.kind)));
    }
    for (text_range, message) in warnings {
        let span = range(usize::from(text_range.start()), usize::from(text_range.end()));
        diagnostics.push(diagnostic(span, DiagnosticSeverity::WARNING, message.clone()));
    }

    let params = PublishDiagnosticsParams {
        uri,
        version: None,
        diagnostics,
    };
    let Ok(encoded) = serde_json::to_value(params) else {
        return
    };
    let not = Notification {
        method: String::from("textDocument/publishDiagnostics"),
        params: encoded,
    };
    connection.sender.send(Message::Notification(not));
}
