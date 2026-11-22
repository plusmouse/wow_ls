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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::syntax::{
    syntax, SyntaxNode, SyntaxKind
};
use std::error::Error;
use lsp_types::{
    ClientCapabilities, Hover, HoverContents, HoverProviderCapability, InitializeParams, MarkedString, Position, Range, ServerCapabilities, notification, request,
    MarkupKind, MarkupContent,
};
use lsp_types::{TextDocumentSyncCapability, TextDocumentSyncKind};

use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use rowan::{TextSize, TokenAtOffset};

use crate::lsp::diagnostics;
use crate::types::{Analysis, FileId};
use crate::definitions::{self, Definitions};
use crate::project::{self, Addon};

pub fn start_ls()  -> Result<(), Box<dyn Error + Sync + Send>> {
    // Note that  we must have our logging only write out to stderr.
    //eprintln!("Starting wow_ls");
    // Create the transport. Includes the stdio (stdin and stdout) versions but this could
    // also be implemented to use sockets or HTTP.
    let (connection, _io_threads) = Connection::stdio();

    // Run the server
    let (id, params) = connection.initialize_start()?;

    let init_params: InitializeParams = serde_json::from_value(params).unwrap();
    // Editors can point at their own definitions with `initializationOptions: {"definitionsPath": "..."}`
    let definitions_path = init_params.initialization_options.as_ref()
        .and_then(|o| o.get("definitionsPath"))
        .and_then(|p| p.as_str())
        .map(PathBuf::from);
    // Where to look for addons (`.toc` files)
    #[allow(deprecated)]
    let root = init_params.workspace_folders.as_ref()
        .and_then(|folders| folders.first())
        .map(|folder| folder.uri.to_string())
        .or_else(|| init_params.root_uri.as_ref().map(|uri| uri.to_string()))
        .and_then(|uri| project::uri_to_path(&uri));
    let _client_capabilities: ClientCapabilities = init_params.capabilities;
    let server_capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    };

    let initialize_data = serde_json::json!({
        "capabilities": server_capabilities,
        "serverInfo": {
            "name": "wow_ls",
            "version": "0.1"
        }
    });

    connection.initialize_finish(id, initialize_data)?;

    main_loop(connection, definitions_path, root)
}

/// What's known about the workspace's addons, built when first needed
#[derive(Default)]
struct Workspace {
    root: Option<PathBuf>,
    /// Found the first time they're needed, and again after a `.toc` changes
    addons: Option<Vec<Addon>>,
    /// Text of open documents, which is used instead of what's on disk
    open: HashMap<PathBuf, String>,
    /// Each addon's analysis, by its `.toc`, until one of its files changes
    analyses: HashMap<PathBuf, Arc<(Analysis, Vec<PathBuf>)>>,
}

impl Workspace {
    fn addons(&mut self) -> &[Addon] {
        if self.addons.is_none() {
            self.addons = Some(self.root.as_deref().map(project::discover).unwrap_or_default());
        }
        self.addons.as_deref().unwrap_or_default()
    }

    /// A document's text changed (or was opened)
    fn changed(&mut self, path: PathBuf, text: String) {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toc")) {
            self.addons = None;
            self.analyses.clear();
        }
        let path = project::normalise(&path);
        let affected: Vec<PathBuf> = self.addons.iter().flatten()
            .filter(|addon| addon.contains(&path))
            .map(|addon| addon.toc.clone())
            .collect();
        for toc in affected {
            self.analyses.remove(&toc);
        }
        self.open.insert(path, text);
    }

    /// The analysis covering `path`, and its id in that analysis: its whole
    /// addon's if it's in one, otherwise just the file (`root`)
    fn analysis(&mut self, path: Option<&Path>, root: &SyntaxNode, definitions: &Arc<Definitions>) -> (Arc<(Analysis, Vec<PathBuf>)>, FileId) {
        if let Some(path) = path.map(project::normalise) {
            if let Some(addon) = project::addon_for(self.addons(), &path).cloned() {
                let open = &self.open;
                let analysis = self.analyses.entry(addon.toc.clone())
                    .or_insert_with(|| Arc::new(addon.analyse(open, definitions)))
                    .clone();
                if let Some(index) = analysis.1.iter().position(|p| *p == path) {
                    return (analysis, FileId(index as u32))
                }
            }
        }
        (Arc::new((Analysis::analyse_file(root, definitions), Vec::new())), FileId(0))
    }
}

fn main_loop(connection: Connection, definitions_path: Option<PathBuf>, root: Option<PathBuf>) -> Result<(), Box<dyn Error + Sync + Send>> {
    // Loaded the first time types are needed, not at startup
    let mut loaded_definitions: Option<Arc<Definitions>> = None;
    let mut language: HashMap<String, String> = HashMap::new();
    let mut text: HashMap<String, String> = HashMap::new();
    let mut workspace = Workspace { root, ..Workspace::default() };

    for msg in &connection.receiver {
        //eprintln!("got msg: {msg:?}");
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                //eprint!("got req {}", &*req.method);
                match &*req.method {
                    "textDocument/hover" => {
                        if let Ok((id, params)) = cast_req::<request::HoverRequest>(req) {
                            let pos = params.text_document_position_params.position;
                            let s = text.get(&params.text_document_position_params.text_document.uri.to_string()).expect("missing");
                            let lines: Vec<_> = s.lines().collect();
                            let mut offset = 0;
                            let wanted_line = usize::try_from(pos.line).unwrap();
                            for i in 0..lines.len() {
                                if i == wanted_line {
                                    offset = offset + pos.character;
                                    break
                                } else {
                                    offset = offset + u32::try_from(lines[i].len()).unwrap() + 1;
                                }
                            }
                            let mut lexer = syntax::Generator::new(&s);
                            let all = SyntaxNode::new_root(lexer.process_all());
                            let token = all.token_at_offset(TextSize::from(offset));
                            let numbers = line_numbers::LinePositions::from(s.as_str());
                            let node;
                            match token.clone() {
                                TokenAtOffset::Single(t) => {
                                    node = t.parent().unwrap();
                                }
                                TokenAtOffset::Between(t1, t2) => {
                                    node = t2.parent().unwrap();
                                }
                                TokenAtOffset::None => {
                                    let result = Some(Hover{contents: HoverContents::Scalar(MarkedString::String(String::from("ERROR: UNKNOWN"))), range: None});
                                    let result = serde_json::to_value(&result).unwrap();
                                    let resp = Response {
                                        id,
                                        result: Some(result),
                                        error: None,
                                    };
                                    connection.sender.send(Message::Response(resp))?;
                                    continue;
                                }
                            }
                            let definitions = loaded_definitions
                                .get_or_insert_with(|| Arc::new(definitions::load(definitions_path.as_deref())))
                                .clone();
                            let uri = params.text_document_position_params.text_document.uri.to_string();
                            let path = project::uri_to_path(&uri);
                            let (analysis, file) = workspace.analysis(path.as_deref(), &all, &definitions);
                            let analysis = &analysis.0;
                            let (output, text_range) = match analysis.reference_at(file, TextSize::from(offset)) {
                                Some(r) => {
                                    let binding = analysis.binding(r);
                                    (format!("{} {}: {}", binding.kind, binding.name, analysis.describe(r.types)), r.range)
                                }
                                None => {
                                    let n = node.ancestors().filter(|a| a.kind() != SyntaxKind::Identifier).nth(0);
                                    let output = match n {
                                        Some(n) => format!("{:?} {}", n.kind(), n.text()),
                                        None => String::from("")
                                    };
                                    (output, node.text_range())
                                }
                            };
                            let (start, end) = (text_range.start(), text_range.end());
                            let (start, end) = (numbers.from_offset(usize::from(start)), numbers.from_offset(usize::from(end)));
                            let range: Option<Range> = Some(Range{start: Position{line: start.0.0, character: start.1.try_into().unwrap()}, end: Position{line: end.0.0, character: end.1.try_into().unwrap()}});
                            let result = Some(Hover{contents: HoverContents::Markup(MarkupContent{kind: MarkupKind::PlainText, value: output}), range});
                            let result = serde_json::to_value(&result).unwrap();
                            let resp = Response {
                                id,
                                result: Some(result),
                                error: None,
                            };
                            connection.sender.send(Message::Response(resp))?;
                        }
                    }
                    _ => {
                    }
                };
                // ...
            }
            Message::Response(resp) => {
                //eprintln!("got response: {resp:?}");
            }
            Message::Notification(not) => {
                //eprint!("got not {}", &*not.method);
                match &*not.method {
                    "textDocument/didChange" => {
                        if let Ok(params) = cast_not::<notification::DidChangeTextDocument>(not) {
                            if let (Some(path), Some(change)) = (project::uri_to_path(&params.text_document.uri.to_string()), params.content_changes.first()) {
                                workspace.changed(path, change.text.clone());
                            }
                            if let Some(l) = language.get(&params.text_document.uri.to_string()) {
                                if l == "lua" {
                                    text.remove(&params.text_document.uri.to_string());
                                    text.insert(params.text_document.uri.to_string(), params.content_changes[0].text.clone());
                                    diagnostics::get(&connection, params.text_document.uri, &params.content_changes[0].text);
                                }
                            }
                        }
                    }
                    "textDocument/didOpen" => {
                        if let Ok(params) = cast_not::<notification::DidOpenTextDocument>(not) {
                            language.insert(params.text_document.uri.to_string(), params.text_document.language_id.clone());
                            text.insert(params.text_document.uri.to_string(), params.text_document.text.clone());
                            if let Some(path) = project::uri_to_path(&params.text_document.uri.to_string()) {
                                workspace.changed(path, params.text_document.text.clone());
                            }
                            if params.text_document.language_id == "lua" {
                                diagnostics::get(&connection, params.text_document.uri, &params.text_document.text);
                            }
                        }
                    }
                    _ => {
                        //eprintln!("fallback")
                    }
                }
            }
        }
    }
    Ok(())
}

fn cast_req<R>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn cast_not<N>(not: Notification) -> Result<N::Params, ExtractError<Notification>>
where
    N: lsp_types::notification::Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD)
}
