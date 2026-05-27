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

pub fn start_ls()  -> Result<(), Box<dyn Error + Sync + Send>> {
    // Note that  we must have our logging only write out to stderr.
    //eprintln!("Starting wow_ls");
    // Create the transport. Includes the stdio (stdin and stdout) versions but this could
    // also be implemented to use sockets or HTTP.
    let (connection, _io_threads) = Connection::stdio();

    // Run the server
    let (id, params) = connection.initialize_start()?;

    let init_params: InitializeParams = serde_json::from_value(params).unwrap();
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

    main_loop(connection)
}

fn main_loop(connection: Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
    let mut language: HashMap<String, String> = HashMap::new();
    let mut text: HashMap<String, String> = HashMap::new();

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
                            let n = node.ancestors().filter(|a| a.kind() != SyntaxKind::Identifier).nth(0);
                            let output = match n {
                                Some(n) => format!("{:?} {}", n.kind(), n.text()),
                                None => String::from("")
                            };
                            let (start, end) = (node.text_range().start(), node.text_range().end());
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
