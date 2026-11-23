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
    MarkupKind, MarkupContent, CompletionOptions, CompletionItem, CompletionItemKind, CompletionList, CompletionResponse,
};
use lsp_types::{TextDocumentSyncCapability, TextDocumentSyncKind};

use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use rowan::TokenAtOffset;

use crate::lsp::{diagnostics, position};
use crate::types::{Analysis, FileId};
use crate::definitions::{self, Definitions};
use crate::project::{self, Addon};
use crate::completion::{self, CandidateKind};

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
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
            ..CompletionOptions::default()
        }),
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
    /// The URI each open document was opened with
    uris: HashMap<PathBuf, lsp_types::Uri>,
}

impl Workspace {
    fn addons(&mut self) -> &[Addon] {
        if self.addons.is_none() {
            self.addons = Some(self.root.as_deref().map(project::discover).unwrap_or_default());
        }
        self.addons.as_deref().unwrap_or_default()
    }

    /// Publishes diagnostics for every open document the change to `path` could
    /// affect: all of its addon's, or just it if it's not in one
    fn publish_diagnostics(&mut self, connection: &Connection, path: &Path, definitions: &Arc<Definitions>) {
        let path = project::normalise(path);
        let Some(text) = self.open.get(&path).cloned() else { return };
        let root = SyntaxNode::new_root(syntax::Generator::new(&text).process_all());
        let (analysis, _) = self.analysis(Some(&path), &root, definitions);
        let (analysis, paths) = (&analysis.0, &analysis.1);
        // Files of the addon, or the one file analysed on its own
        let files: Vec<(FileId, PathBuf)> = if paths.is_empty() {
            vec![(FileId(0), path.clone())]
        } else {
            paths.iter().enumerate().map(|(i, p)| (FileId(i as u32), p.clone())).collect()
        };
        for (file, path) in files {
            let (Some(uri), Some(text)) = (self.uris.get(&path), self.open.get(&path)) else { continue };
            let warnings: Vec<_> = analysis.diagnostics.iter()
                .filter(|d| d.file == file)
                .map(|d| (d.range, d.message.clone()))
                .collect();
            diagnostics::publish(connection, uri.clone(), text, &warnings);
        }
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
                            // Not a document we were told about, so nothing to show
                            let Some(s) = text.get(&params.text_document_position_params.text_document.uri.to_string()) else {
                                let resp = Response { id, result: Some(serde_json::Value::Null), error: None };
                                connection.sender.send(Message::Response(resp))?;
                                continue;
                            };
                            let offset = position::offset_of(s, pos);
                            let mut lexer = syntax::Generator::new(&s);
                            let all = SyntaxNode::new_root(lexer.process_all());
                            let token = all.token_at_offset(offset);
                            let node;
                            match token.clone() {
                                TokenAtOffset::Single(t) => {
                                    node = t.parent().unwrap();
                                }
                                TokenAtOffset::Between(_, t2) => {
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
                            let (output, text_range) = match analysis.hover(file, offset) {
                                Some(hover) => hover,
                                None => {
                                    let n = node.ancestors().filter(|a| a.kind() != SyntaxKind::Identifier).nth(0);
                                    let output = match n {
                                        Some(n) => format!("{:?} {}", n.kind(), n.text()),
                                        None => String::from("")
                                    };
                                    (output, node.text_range())
                                }
                            };
                            let range: Option<Range> = Some(position::range_of(s, text_range));
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
                    "textDocument/completion" => {
                        if let Ok((id, params)) = cast_req::<request::Completion>(req) {
                            let position = params.text_document_position.position;
                            let uri = params.text_document_position.text_document.uri.to_string();
                            let result = match text.get(&uri) {
                                Some(s) => {
                                    let definitions = loaded_definitions
                                        .get_or_insert_with(|| Arc::new(definitions::load(definitions_path.as_deref())))
                                        .clone();
                                    Some(complete(&mut workspace, &uri, s, position, &definitions))
                                }
                                None => None,
                            };
                            let resp = Response { id, result: Some(serde_json::to_value(&result).unwrap()), error: None };
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
                            let path = project::uri_to_path(&params.text_document.uri.to_string());
                            if let (Some(path), Some(change)) = (&path, params.content_changes.first()) {
                                workspace.changed(path.clone(), change.text.clone());
                            }
                            if let Some(l) = language.get(&params.text_document.uri.to_string()) {
                                if l == "lua" {
                                    text.remove(&params.text_document.uri.to_string());
                                    text.insert(params.text_document.uri.to_string(), params.content_changes[0].text.clone());
                                    match &path {
                                        Some(path) => {
                                            let definitions = loaded_definitions
                                                .get_or_insert_with(|| Arc::new(definitions::load(definitions_path.as_deref())))
                                                .clone();
                                            workspace.publish_diagnostics(&connection, path, &definitions);
                                        }
                                        None => diagnostics::publish(&connection, params.text_document.uri, &params.content_changes[0].text, &[]),
                                    }
                                }
                            }
                        }
                    }
                    "textDocument/didOpen" => {
                        if let Ok(params) = cast_not::<notification::DidOpenTextDocument>(not) {
                            language.insert(params.text_document.uri.to_string(), params.text_document.language_id.clone());
                            text.insert(params.text_document.uri.to_string(), params.text_document.text.clone());
                            let path = project::uri_to_path(&params.text_document.uri.to_string());
                            if let Some(path) = &path {
                                workspace.uris.insert(project::normalise(path), params.text_document.uri.clone());
                                workspace.changed(path.clone(), params.text_document.text.clone());
                            }
                            if params.text_document.language_id == "lua" {
                                match &path {
                                    Some(path) => {
                                        let definitions = loaded_definitions
                                            .get_or_insert_with(|| Arc::new(definitions::load(definitions_path.as_deref())))
                                            .clone();
                                        workspace.publish_diagnostics(&connection, path, &definitions);
                                    }
                                    None => diagnostics::publish(&connection, params.text_document.uri, &params.text_document.text, &[]),
                                }
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

/// The completion list at `position` in the open document `uri`, whose text is `s`
fn complete(workspace: &mut Workspace, uri: &str, s: &str, position: Position, definitions: &Arc<Definitions>) -> CompletionResponse {
    let offset = position::offset_of(s, position);
    let root = SyntaxNode::new_root(syntax::Generator::new(s).process_all());
    let path = project::uri_to_path(uri);
    let (analysis, file) = workspace.analysis(path.as_deref(), &root, definitions);
    let analysis = &analysis.0;
    let completions = completion::complete(analysis, file, &root, s, offset);
    let items = completions.candidates.iter().map(|candidate| {
        let (kind, order) = match candidate.kind {
            CandidateKind::Local | CandidateKind::Parameter => (CompletionItemKind::VARIABLE, 0),
            CandidateKind::Field => (CompletionItemKind::FIELD, 0),
            CandidateKind::Method => (CompletionItemKind::METHOD, 0),
            CandidateKind::Global => (CompletionItemKind::VARIABLE, 1),
            CandidateKind::Keyword => (CompletionItemKind::KEYWORD, 2),
        };
        let kind = match candidate.kind {
            CandidateKind::Local | CandidateKind::Parameter | CandidateKind::Global if completion::is_function(candidate) => CompletionItemKind::FUNCTION,
            _ => kind,
        };
        CompletionItem {
            label: candidate.name.clone(),
            kind: Some(kind),
            detail: completion::detail(analysis, candidate),
            // Locals before globals before keywords, alphabetically within each
            sort_text: Some(format!("{order}{}", candidate.name)),
            ..CompletionItem::default()
        }
    }).collect();
    CompletionResponse::List(CompletionList { is_incomplete: completions.incomplete, items })
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
