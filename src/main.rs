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

#![allow(clippy::print_stderr)]

use std::error::Error;
use std::env;

mod syntax;
mod lsp;
mod state;
mod diagnostics;
mod types;
mod definitions;
mod project;
mod ast;
mod completion;
mod xml;
mod generate;

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "evaluate" {
        let filename = args.get(2).map(String::as_str).unwrap_or("tests/functioncall.lua");
        //let s = std::fs::read_to_string("../wow-ui-source/full.lua")?;
        let s = std::fs::read_to_string(filename)?;
        let mut a = syntax::syntax::Generator::new(&s);
        let numbers = line_numbers::LinePositions::from(s.as_str());
        let before = std::time::Instant::now();
        let res = a.process_all();
        let root = syntax::syntax::SyntaxNode::new_root(res.clone());
        let dur  = std::time::Instant::now() - before;
        syntax::debug::print_tree(&res);
        //println!("{:#?}", res);
        let mut errors = a.errors().clone();
        errors.extend(syntax::checks::check(&root));
        println!("{:#?}", errors);
        //println!("{:?}", numbers.from_offset(a.errors()[0].start));
        //println!("syntax: {:?}", dur);
        Ok(())
    } else if args.len() > 2 && args[1] == "types" {
        // A `.lua` file on its own, or a `.toc` for every file of its addon together
        let definitions = std::sync::Arc::new(definitions::load(None));
        let target = std::path::Path::new(&args[2]);
        let before = std::time::Instant::now();
        // An addon sees what those it depends on (found beside it) make
        // visible, each analysed on its own first
        let (analysed, analysed_file, dependency_files);
        let (analysis, paths) = if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("toc")) {
            let addon = project::Addon::load(target)?;
            // With the templates and frames its XML, and its dependencies', define
            let xml = project::xml_files(std::slice::from_ref(&addon), &[]);
            let (definitions, errors) = project::with_xml(&definitions, &xml, &Default::default(), &mut Default::default());
            for error in errors {
                let text = xml::read(&error.path)?;
                let numbers = line_numbers::LinePositions::from(text.as_str());
                let (line, column) = numbers.from_offset(error.offset);
                println!("error {}:{}:{} {}", error.path.display(), line.0 + 1, column + 1, error.message);
            }
            analysed = project::analyse_addon(&addon, &[], &Default::default(), &definitions, &mut Default::default());
            dependency_files = analysed.involves.iter().filter(|p| !addon.files.contains(p)).count();
            (&analysed.analysis, analysed.paths.clone())
        } else {
            let s = std::fs::read_to_string(target)?;
            let root = syntax::syntax::SyntaxNode::new_root(syntax::syntax::Generator::new(&s).process_all());
            dependency_files = 0;
            analysed_file = types::Analysis::analyse_file(&root, &definitions);
            (&analysed_file, vec![target.to_path_buf()])
        };
        let dur = std::time::Instant::now() - before;
        // Everything the analysis knows, not just what hover shows
        let _made_with = types::MadeWith::shown();
        for (i, path) in paths.iter().enumerate() {
            let text = std::fs::read_to_string(path)?;
            let numbers = line_numbers::LinePositions::from(text.as_str());
            if paths.len() > 1 {
                println!("== {}", path.display());
            }
            for r in analysis.references.iter().filter(|r| r.file == types::FileId(i as u32)) {
                let binding = analysis.binding(r);
                let (line, column) = numbers.from_offset(usize::from(r.range.start()));
                let access = if r.write { "=" } else { ":" };
                println!("{}:{} {} {}{} {}", line.0 + 1, column + 1, binding.kind, binding.name, access, analysis.describe_callee(r.types, r.call_returns.as_ref()));
            }
            for f in analysis.field_references.iter().filter(|f| f.file == types::FileId(i as u32)) {
                let (line, column) = numbers.from_offset(usize::from(f.range.start()));
                let access = if f.write { "=" } else { ":" };
                println!("{}:{} field {}{} {}", line.0 + 1, column + 1, analysis.field_path(f), access, analysis.describe_callee(f.types, f.call_returns.as_ref()));
            }
            for d in analysis.diagnostics.iter().filter(|d| d.file == types::FileId(i as u32)) {
                let (line, column) = numbers.from_offset(usize::from(d.range.start()));
                println!("warning {}:{}:{} {}", path.display(), line.0 + 1, column + 1, d.message);
            }
        }
        let bodies = analysis.replayed + analysis.walked;
        let replayed = if bodies == 0 { 0 } else { analysis.replayed * 100 / bodies };
        let with = if dependency_files > 0 { format!(" (and {dependency_files} of addons it depends on, analysed first)") } else { String::new() };
        eprintln!("{} files{with}, {} bindings, {} references in {:?} ({} walks, {}% of function bodies replayed)", paths.len(), analysis.bindings.len(), analysis.references.len(), dur, analysis.walks, replayed);
        Ok(())
    } else if args.len() > 3 && args[1] == "generate-templates" {
        // wow_ls generate-templates <wow-ui-source> <out.json>
        let out = std::path::Path::new(&args[3]);
        let directory = definitions::locate(None).ok_or("no definitions directory found")?;
        generate::templates(std::path::Path::new(&args[2]), &directory, out)?;
        Ok(())
    } else if args.len() > 3 && args[1] == "generate-global-strings" {
        // wow_ls generate-global-strings <GlobalStrings.csv> <out.json>
        generate::global_strings(std::path::Path::new(&args[2]), std::path::Path::new(&args[3]))?;
        Ok(())
    } else {
        lsp::start_ls()
    }
}
