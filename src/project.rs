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

//! Addons in a workspace, and the Lua files each loads, from its `.toc`.
//!
//! A `.toc` lists files one per line, in load order, relative to its folder,
//! with `#` lines for comments and `## Key: Value` metadata. A folder can have
//! several (`Addon.toc`, `Addon_Mainline.toc`, ...) for different game
//! versions; one is picked per folder: the one named after the folder, else
//! `_Mainline`, else the first by name.
//!
//! Not handled yet: XML files (and the Lua files they include with
//! `<Script file="..."/>`), and paths using `[Family]`-style variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::definitions::Definitions;
use crate::syntax::SyntaxNode;
use crate::syntax::syntax::Generator;
use crate::types::{Analysis, SourceFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addon {
    pub name: String,
    pub toc: PathBuf,
    /// Lua files in load order
    pub files: Vec<PathBuf>,
}

impl Addon {
    pub fn load(toc: &Path) -> std::io::Result<Addon> {
        let text = std::fs::read_to_string(toc)?;
        let directory = toc.parent().unwrap_or(Path::new(""));
        let files = toc_files(&text).into_iter().map(|file| normalise(&directory.join(file))).collect();
        let name = directory.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Ok(Addon { name, toc: normalise(toc), files })
    }

    pub fn contains(&self, file: &Path) -> bool {
        let file = normalise(file);
        self.files.iter().any(|f| *f == file)
    }

    /// Analyses the addon's files together. Open documents' text is used
    /// instead of what's on disk; files that can't be read are skipped. Returns
    /// the files analysed, whose positions are their `FileId`s
    pub fn analyse(&self, open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>) -> (Analysis, Vec<PathBuf>) {
        let mut sources = Vec::new();
        let mut paths = Vec::new();
        for path in &self.files {
            let text = match open.get(path) {
                Some(text) => text.clone(),
                None => match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
            };
            let root = SyntaxNode::new_root(Generator::new(&text).process_all());
            sources.push(SourceFile { root, in_addon: true });
            paths.push(path.clone());
        }
        (Analysis::analyse(&sources, definitions), paths)
    }
}

/// The Lua files a `.toc` lists, as written (relative to its folder)
pub fn toc_files(text: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue
        }
        // Loading conditions after the path, like `File.lua [AllowLoadGameType mainline]`
        let entry = line.split(" [").next().unwrap_or(line).trim();
        if entry.contains('[') || !entry.to_ascii_lowercase().ends_with(".lua") {
            continue
        }
        files.push(PathBuf::from(entry.replace('\\', "/")));
    }
    files
}

/// Every addon under `root`, by finding `.toc` files. Skips hidden folders
pub fn discover(root: &Path) -> Vec<Addon> {
    let mut tocs = Vec::new();
    find_tocs(root, &mut tocs);
    tocs.iter().filter_map(|toc| Addon::load(toc).ok()).collect()
}

fn find_tocs(directory: &Path, tocs: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let mut here = Vec::new();
    let mut subdirectories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue
        }
        if path.is_dir() {
            subdirectories.push(path);
        } else if name.to_ascii_lowercase().ends_with(".toc") {
            here.push(path);
        }
    }
    if let Some(toc) = pick_toc(directory, here) {
        tocs.push(toc);
    }
    subdirectories.sort();
    for subdirectory in subdirectories {
        find_tocs(&subdirectory, tocs);
    }
}

/// Of a folder's `.toc` files, the one to use
fn pick_toc(directory: &Path, mut tocs: Vec<PathBuf>) -> Option<PathBuf> {
    tocs.sort();
    let folder = directory.file_name()?.to_string_lossy().to_ascii_lowercase();
    let stem = |toc: &PathBuf| toc.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase());
    let preferred = [folder.clone(), format!("{folder}_mainline")];
    for name in preferred {
        if let Some(toc) = tocs.iter().find(|toc| stem(toc).as_deref() == Some(name.as_str())) {
            return Some(toc.clone())
        }
    }
    tocs.into_iter().next()
}

/// The first addon that loads `file`
pub fn addon_for<'a>(addons: &'a [Addon], file: &Path) -> Option<&'a Addon> {
    addons.iter().find(|addon| addon.contains(file))
}

/// Absolute and without `.`/`..` where possible, so paths from a `.toc` and
/// from the editor compare equal
pub fn normalise(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The path of a `file://` URI
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        if let (b'%', Some(Ok(byte))) = (bytes[i], hex.map(|h| u8::from_str_radix(h, 16))) {
            decoded.push(byte);
            i += 3;
            continue
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let mut path = String::from_utf8(decoded).ok()?;
    // `file:///C:/...` on Windows
    if path.len() > 2 && path.starts_with('/') && path.as_bytes()[2] == b':' {
        path.remove(0);
    }
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_toc_files() {
        let toc = "\u{feff}## Interface: 110000\n## Title: Test\n# a comment\n\nCore.lua\nModules\\Bags.lua\nFrames.xml\n  Spaced.lua  \nLater.lua [AllowLoadGameType mainline]\nLocales\\[TextLocale].lua\nUPPER.LUA\n";
        let files: Vec<String> = toc_files(toc).iter().map(|f| f.to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["Core.lua", "Modules/Bags.lua", "Spaced.lua", "Later.lua", "UPPER.LUA"]);
    }

    #[test]
    fn picks_a_toc_per_folder() {
        let directory = Path::new("/addons/MyAddon");
        let tocs = |names: &[&str]| names.iter().map(|n| directory.join(n)).collect::<Vec<_>>();
        assert_eq!(pick_toc(directory, tocs(&["MyAddon_Vanilla.toc", "MyAddon.toc"])), Some(directory.join("MyAddon.toc")));
        assert_eq!(pick_toc(directory, tocs(&["MyAddon_Vanilla.toc", "MyAddon_Mainline.toc"])), Some(directory.join("MyAddon_Mainline.toc")));
        assert_eq!(pick_toc(directory, tocs(&["Other.toc"])), Some(directory.join("Other.toc")));
        assert_eq!(pick_toc(directory, Vec::new()), None);
    }

    #[test]
    fn converts_uris() {
        assert_eq!(uri_to_path("file:///home/me/My%20Addon/a.lua"), Some(PathBuf::from("/home/me/My Addon/a.lua")));
        assert_eq!(uri_to_path("file:///C:/Games/a.lua"), Some(PathBuf::from("C:/Games/a.lua")));
        assert_eq!(uri_to_path("file:///c%3A/Games/a.lua"), Some(PathBuf::from("c:/Games/a.lua")));
        assert_eq!(uri_to_path("untitled:1"), None);
    }

    #[test]
    fn discovers_the_test_addon() {
        let addons = discover(Path::new("tests/addon"));
        assert_eq!(addons.len(), 1);
        assert_eq!(addons[0].name, "TestAddon");
        let names: Vec<_> = addons[0].files.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Core.lua", "Util.lua", "Main.lua"]);
    }
}
