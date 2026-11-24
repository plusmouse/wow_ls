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
    // Where to look for addons (`.toc` files): every workspace folder, or the
    // root for editors without them
    #[allow(deprecated)]
    let roots: Vec<PathBuf> = match &init_params.workspace_folders {
        Some(folders) if !folders.is_empty() => folders.iter().filter_map(|f| project::uri_to_path(f.uri.as_str())).collect(),
        _ => init_params.root_uri.iter().filter_map(|uri| project::uri_to_path(uri.as_str())).collect(),
    };
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

    main_loop(connection, definitions_path, roots)
}

/// What's known about the workspace's addons, built when first needed
#[derive(Default)]
struct Workspace {
    /// The workspace folders, searched for addons
    roots: Vec<PathBuf>,
    /// Found the first time they're needed, and again after a `.toc` changes
    addons: Option<Vec<Addon>>,
    /// Text of open documents, which is used instead of what's on disk
    open: HashMap<PathBuf, String>,
    /// Each addon's analysis, of its own files over what those it depends on
    /// make visible, by its `.toc`, until one of the files it involves changes
    analyses: HashMap<PathBuf, Arc<project::Analysed>>,
    /// The URI each open document was opened with
    uris: HashMap<PathBuf, lsp_types::Uri>,
}

impl Workspace {
    fn addons(&mut self) -> &[Addon] {
        if self.addons.is_none() {
            self.addons = Some(self.roots.iter().flat_map(|root| project::discover(root)).collect());
        }
        self.addons.as_deref().unwrap_or_default()
    }

    /// Publishes diagnostics for every open document the change to `path` could
    /// affect: all of its addon's, or just it if it's not in one
    fn publish_diagnostics(&mut self, connection: &Connection, path: &Path, definitions: &Arc<Definitions>) {
        let path = project::normalise(path);
        let Some(text) = self.open.get(&path).cloned() else { return };
        let root = SyntaxNode::new_root(syntax::Generator::new(&text).process_all());
        let (analysis, file) = self.analysis(Some(&path), &root, definitions);
        // Files of the addon, or the one file analysed on its own. Not those of
        // addons it depends on, which have analyses of their own
        let files: Vec<(FileId, PathBuf)> = match &analysis {
            Analysed::Addon(analysed) => analysed.paths.iter().enumerate()
                .map(|(i, p)| (FileId(i as u32), p.clone()))
                .collect(),
            Analysed::File(_) => vec![(file, path.clone())],
        };
        let analysis = analysis.analysis();
        for (file, path) in files {
            let (Some(uri), Some(text)) = (self.uris.get(&path), self.open.get(&path)) else { continue };
            let warnings: Vec<_> = analysis.diagnostics.iter()
                .filter(|d| d.file == file)
                .map(|d| (d.range, d.message.clone()))
                .collect();
            diagnostics::publish(connection, uri.clone(), text, &warnings);
        }
    }

    /// A document's text changed (or was opened). Returns the other open
    /// documents whose diagnostics could change too: those of addons that
    /// depend on its addon
    fn changed(&mut self, path: PathBuf, text: String) -> Vec<PathBuf> {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toc")) {
            self.addons = None;
            self.analyses.clear();
        }
        let path = project::normalise(&path);
        // Opening a file as it is on disk changes nothing analysed
        let before = self.open.get(&path).cloned().or_else(|| std::fs::read_to_string(&path).ok());
        if before.as_deref() == Some(text.as_str()) {
            self.open.insert(path, text);
            return Vec::new()
        }
        // Every analysis that involves it: its addon's, and those of addons
        // depending on it however indirectly. Its addon's even if not analysed yet
        let affected: Vec<PathBuf> = self.analyses.iter()
            .filter(|(_, analysed)| analysed.involves.contains(&path))
            .map(|(toc, _)| toc.clone())
            .collect();
        let mut others: Vec<PathBuf> = self.addons.iter().flatten()
            .filter(|addon| addon.contains(&path))
            .flat_map(|addon| addon.files.iter().cloned())
            .collect();
        for toc in affected {
            if let Some(analysed) = self.analyses.remove(&toc) {
                others.extend(analysed.paths.iter().cloned());
            }
        }
        others.retain(|f| *f != path && self.open.contains_key(f));
        others.dedup();
        self.open.insert(path, text);
        others
    }

    /// The analysis covering `path`, and its id in that analysis: its whole
    /// addon's if it's in one, otherwise just the file (`root`)
    fn analysis(&mut self, path: Option<&Path>, root: &SyntaxNode, definitions: &Arc<Definitions>) -> (Analysed, FileId) {
        if let Some(path) = path.map(project::normalise) {
            if let Some(addon) = project::addon_for(self.addons(), &path).cloned() {
                let known = self.addons.as_deref().unwrap_or_default();
                let analysed = project::analyse_addon(&addon, known, &self.open, definitions, &mut self.analyses);
                if let Some(index) = analysed.paths.iter().position(|p| *p == path) {
                    return (Analysed::Addon(analysed), FileId(index as u32))
                }
            }
        }
        (Analysed::File(Analysis::analyse_file(root, definitions)), FileId(0))
    }
}

/// An analysis of an addon, or of a file in none
enum Analysed {
    Addon(Arc<project::Analysed>),
    File(Analysis),
}

impl Analysed {
    fn analysis(&self) -> &Analysis {
        match self {
            Analysed::Addon(analysed) => &analysed.analysis,
            Analysed::File(analysis) => analysis,
        }
    }
}

fn main_loop(connection: Connection, definitions_path: Option<PathBuf>, roots: Vec<PathBuf>) -> Result<(), Box<dyn Error + Sync + Send>> {
    // Loaded the first time types are needed, not at startup
    let mut loaded_definitions: Option<Arc<Definitions>> = None;
    let mut language: HashMap<String, String> = HashMap::new();
    let mut text: HashMap<String, String> = HashMap::new();
    let mut workspace = Workspace { roots, ..Workspace::default() };

    // Files whose diagnostics are out of date. They're published once no
    // messages are waiting, so a burst of keystrokes is analysed once, for the
    // last of them, rather than once each
    let mut owed: Vec<PathBuf> = Vec::new();
    // The transport hands over one message at a time, and only when asked, so
    // a thread asks for them as they come and queues them here. Otherwise
    // keystrokes typed during an analysis would wait in the transport, out of
    // sight, and each would be analysed in turn
    let (queue, inbox) = std::sync::mpsc::channel::<Message>();
    let transport = connection.receiver.clone();
    std::thread::spawn(move || {
        for msg in transport {
            if queue.send(msg).is_err() {
                break
            }
        }
    });
    // A message taken from the queue while checking whether any were waiting
    let mut next: Option<Message> = None;
    let mut shutting_down = false;
    loop {
        let msg = match next.take().map(Ok).unwrap_or_else(|| inbox.try_recv()) {
            Ok(msg) => msg,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if !owed.is_empty() {
                    let definitions = loaded_definitions
                        .get_or_insert_with(|| Arc::new(definitions::load(definitions_path.as_deref())))
                        .clone();
                    while let Some(path) = owed.first().cloned() {
                        owed.remove(0);
                        workspace.publish_diagnostics(&connection, &path, &definitions);
                        // Something arrived meanwhile: handle it before the rest
                        if let Ok(msg) = inbox.try_recv() {
                            next = Some(msg);
                            break
                        }
                    }
                    continue
                }
                match inbox.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            }
        };
        //eprintln!("got msg: {msg:?}");
        match msg {
            Message::Request(req) => {
                // As `Connection::handle_shutdown`, which can't be used as the
                // queue's thread is the one reading the transport
                if req.method == "shutdown" {
                    connection.sender.send(Message::Response(Response::new_ok(req.id.clone(), ())))?;
                    shutting_down = true;
                    continue;
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
                            let analysis = analysis.analysis();
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
            Message::Notification(not) if not.method == "exit" => {
                return if shutting_down { Ok(()) } else { Err("exit without shutdown".into()) };
            }
            Message::Notification(not) => {
                //eprint!("got not {}", &*not.method);
                match &*not.method {
                    "textDocument/didChange" => {
                        if let Ok(params) = cast_not::<notification::DidChangeTextDocument>(not) {
                            let path = project::uri_to_path(&params.text_document.uri.to_string());
                            if let (Some(path), Some(change)) = (&path, params.content_changes.first()) {
                                for other in workspace.changed(path.clone(), change.text.clone()) {
                                    if !owed.contains(&other) {
                                        owed.push(other);
                                    }
                                }
                            }
                            if let Some(l) = language.get(&params.text_document.uri.to_string()) {
                                if l == "lua" {
                                    text.remove(&params.text_document.uri.to_string());
                                    text.insert(params.text_document.uri.to_string(), params.content_changes[0].text.clone());
                                    match &path {
                                        // Before the addons depending on it
                                        Some(path) => {
                                            owed.retain(|owed| owed != path);
                                            owed.insert(0, path.clone());
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
                                for other in workspace.changed(path.clone(), params.text_document.text.clone()) {
                                    if !owed.contains(&other) {
                                        owed.push(other);
                                    }
                                }
                            }
                            if params.text_document.language_id == "lua" {
                                match &path {
                                    // Before the addons depending on it
                                    Some(path) => {
                                        owed.retain(|owed| owed != path);
                                        owed.insert(0, path.clone());
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
    let analysis = analysis.analysis();
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
