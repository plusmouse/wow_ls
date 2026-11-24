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
//! An addon sees what the addons it depends on (`## Dependencies`,
//! `## RequiredDeps`, `## OptionalDeps`) make visible, as they load before it,
//! but only its own files are walked: each dependency is analysed on its own,
//! so only it types its API, whichever addon uses it. They're found by name
//! among the workspace's addons, or beside the addon's folder, where
//! `Interface/AddOns` would have them. Missing optional ones are left out.
//!
//! Not handled yet: XML files (and the Lua files they include with
//! `<Script file="..."/>`), and paths using `[Family]`-style variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::definitions::Definitions;
use crate::syntax::SyntaxNode;
use crate::syntax::syntax::Generator;
use crate::types::{Analysis, Layer, SourceFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addon {
    pub name: String,
    pub toc: PathBuf,
    /// Lua files in load order
    pub files: Vec<PathBuf>,
    /// The addons it needs or uses if they're there, which load before it, by
    /// name as written
    pub dependencies: Vec<String>,
}

impl Addon {
    pub fn load(toc: &Path) -> std::io::Result<Addon> {
        let text = std::fs::read_to_string(toc)?;
        let directory = toc.parent().unwrap_or(Path::new(""));
        let files = toc_files(&text).into_iter().map(|file| normalise(&directory.join(file))).collect();
        let name = directory.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Ok(Addon { name, toc: normalise(toc), files, dependencies: toc_dependencies(&text) })
    }

    /// The addons this one names as dependencies that can be found: among
    /// `known`, then beside this addon's folder
    pub fn direct_dependencies(&self, known: &[Addon]) -> Vec<Addon> {
        let mut found: Vec<Addon> = Vec::new();
        for name in &self.dependencies {
            let Some(dependency) = self.find_dependency(name, known) else { continue };
            if dependency.toc != self.toc && !found.iter().any(|a| a.toc == dependency.toc) {
                found.push(dependency);
            }
        }
        found
    }

    /// An addon by name, as WoW knows it: its folder's name
    fn find_dependency(&self, name: &str, known: &[Addon]) -> Option<Addon> {
        if let Some(addon) = known.iter().find(|a| a.name.eq_ignore_ascii_case(name)) {
            return Some(addon.clone())
        }
        // `Interface/AddOns` holds every addon side by side
        let siblings = self.toc.parent()?.parent()?;
        let entries = std::fs::read_dir(siblings).ok()?;
        let folder = entries.flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_dir() && path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name)))?;
        let tocs = std::fs::read_dir(&folder).ok()?.flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toc")))
            .collect();
        Addon::load(&pick_toc(&folder, tocs)?).ok()
    }

    pub fn contains(&self, file: &Path) -> bool {
        let file = normalise(file);
        self.files.iter().any(|f| *f == file)
    }

    /// Analyses the addon's own files together, seeing what the addons it
    /// depends on make visible (`layer`) but not walking them. Open documents'
    /// text is used instead of what's on disk; files that can't be read are
    /// skipped. Returns the files analysed, whose positions are their `FileId`s
    pub fn analyse(&self, layer: Arc<Layer>, open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>) -> (Analysis, Vec<PathBuf>) {
        let mut sources = Vec::new();
        let mut paths = Vec::new();
        for path in &self.files {
            if paths.contains(path) {
                continue
            }
            let text = match open.get(path) {
                Some(text) => text.clone(),
                None => match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
            };
            let root = SyntaxNode::new_root(Generator::new(&text).process_all());
            sources.push(SourceFile { root, addon: Some(0) });
            paths.push(path.clone());
        }
        (Analysis::analyse_over(&sources, definitions, layer), paths)
    }
}

/// An addon's analysis, of its own files, over what its dependencies make
/// visible
#[derive(Debug)]
pub struct Analysed {
    pub addon: Addon,
    pub analysis: Analysis,
    /// The files analysed, by `FileId`
    pub paths: Vec<PathBuf>,
    /// Every file the analysis depends on: its own, and its dependencies',
    /// however indirectly. A change to any of them makes it out of date
    pub involves: Vec<PathBuf>,
    /// What it makes visible to addons depending on it, made when one needs it
    layer: std::sync::OnceLock<Arc<Layer>>,
}

impl Analysed {
    /// What this addon (and those it depends on) make visible to its dependents
    pub fn layer(&self) -> Arc<Layer> {
        self.layer.get_or_init(|| Arc::new(Layer::of(&self.analysis, &self.paths, &self.addon.toc.to_string_lossy()))).clone()
    }
}

/// `addon`'s analysis, from `cache` if it's there. Its dependencies are
/// analysed first, each on its own and cached too, so an addon shared by
/// several is analysed once, and only the addon itself types its API
pub fn analyse_addon(addon: &Addon, known: &[Addon], open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>, cache: &mut HashMap<PathBuf, Arc<Analysed>>) -> Arc<Analysed> {
    analyse_visiting(addon, known, open, definitions, cache, &mut Vec::new())
}

fn analyse_visiting(addon: &Addon, known: &[Addon], open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>, cache: &mut HashMap<PathBuf, Arc<Analysed>>, visiting: &mut Vec<PathBuf>) -> Arc<Analysed> {
    if let Some(analysed) = cache.get(&addon.toc) {
        return analysed.clone()
    }
    visiting.push(addon.toc.clone());
    let mut layer = Layer::default();
    let mut involves = Vec::new();
    for dependency in addon.direct_dependencies(known) {
        // One that depends on this one back: the game would refuse the loop
        if visiting.contains(&dependency.toc) {
            continue
        }
        let analysed = analyse_visiting(&dependency, known, open, definitions, cache, visiting);
        layer.add(&analysed.layer());
        for path in &analysed.involves {
            if !involves.contains(path) {
                involves.push(path.clone());
            }
        }
    }
    visiting.pop();
    let (analysis, paths) = addon.analyse(Arc::new(layer), open, definitions);
    for path in &paths {
        if !involves.contains(path) {
            involves.push(path.clone());
        }
    }
    let analysed = Arc::new(Analysed { addon: addon.clone(), analysis, paths, involves, layer: Default::default() });
    cache.insert(addon.toc.clone(), analysed.clone());
    analysed
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

/// The addons a `.toc` says this one needs or uses: `## Dependencies`,
/// `## RequiredDeps` (or any `## Dep...`), and `## OptionalDeps`
pub fn toc_dependencies(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(metadata) = line.trim_start_matches('\u{feff}').trim().strip_prefix("##") else { continue };
        let Some((key, value)) = metadata.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        if !(key.starts_with("dep") || key == "requireddeps" || key == "optionaldeps") {
            continue
        }
        for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if !names.iter().any(|n: &String| n.eq_ignore_ascii_case(name)) {
                names.push(name.to_string());
            }
        }
    }
    names
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

/// The `file://` URI of `path`, the inverse of `uri_to_path`
pub fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy();
    // Windows: `\\?\C:\...` from `canonicalize`, or `C:\...`
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).replace('\\', "/");
    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
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
    fn reads_toc_dependencies() {
        let toc = "## Interface: 110000\n## OptionalDeps: Syndicator, Masque\n## RequiredDeps: LibA\n## Dependencies: LibB, liba\n## DepsExtra: LibC\n## SavedVariables: X\nCore.lua\n";
        assert_eq!(toc_dependencies(toc), ["Syndicator", "Masque", "LibA", "LibB", "LibC"]);
    }

    #[test]
    fn finds_direct_dependencies() {
        let addon = |name: &str, dependencies: &[&str]| Addon {
            name: name.to_string(),
            toc: PathBuf::from(format!("/nowhere/{name}/{name}.toc")),
            files: Vec::new(),
            dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
        };
        let known = [addon("Core", &[]), addon("Lib", &["Core"]), addon("Main", &["Lib", "core", "Missing", "Main", "Lib"])];
        let names: Vec<String> = known[2].direct_dependencies(&known).into_iter().map(|a| a.name).collect();
        // Missing ones, itself and repeats left out; Core, needed through Lib, not
        assert_eq!(names, ["Lib", "Core"]);
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
        assert_eq!(path_to_uri(Path::new("/home/me/My Addon/a.lua")), "file:///home/me/My%20Addon/a.lua");
        assert_eq!(path_to_uri(Path::new(r"\\?\C:\Games\a.lua")), "file:///C:/Games/a.lua");
        for path in ["/home/me/My Addon/é#1.lua", "C:/Games/a.lua"] {
            assert_eq!(uri_to_path(&path_to_uri(Path::new(path))), Some(PathBuf::from(path)));
        }
    }

    /// The type of what's assigned to `name` in `analysed`'s own files
    fn assigned(analysed: &Analysed, name: &str) -> String {
        let analysis = &analysed.analysis;
        analysis.references.iter()
            .find(|r| r.write && analysis.binding(r).name == name)
            .map(|r| analysis.describe(r.types))
            .unwrap()
    }

    /// How the function a global's field holds is described
    fn described_field(analysed: &Analysed, path: &str) -> String {
        let analysis = &analysed.analysis;
        analysis.field_references.iter()
            .find(|f| analysis.field_path(f) == path)
            .map(|f| analysis.describe(f.types))
            .unwrap()
    }

    #[test]
    fn analyses_addons_with_what_they_depend_on() {
        let user = Addon::load(Path::new("tests/dependencies/User/User.toc")).unwrap();
        // Found beside it, with the missing optional one left out
        let names: Vec<String> = user.direct_dependencies(&[]).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Lib"]);
        let definitions = Arc::new(Definitions::default());
        let mut cache = HashMap::new();
        let analysed = analyse_addon(&user, &[], &HashMap::new(), &definitions, &mut cache);
        // Only its own files are walked, though it involves Lib's too
        assert_eq!(analysed.paths.len(), 1);
        assert_eq!(analysed.involves.len(), 2);
        // Lib, analysed on its own first
        assert_eq!(cache.len(), 2);
        // Lib's globals, loaded first
        assert_eq!(assigned(&analysed, "fromLib"), "string");
        // Each addon has its own `...` namespace, so Lib's field isn't User's
        assert_eq!(assigned(&analysed, "mine"), "unknown");
        // Lib types its own API: User calling it with a number changes nothing
        let lib = cache.values().find(|a| a.addon.name == "Lib").unwrap().clone();
        assert_eq!(described_field(&analysed, "LibGlobal.Take"), described_field(&lib, "LibGlobal.Take"));
        // What Lib makes visible reaches Top through User, as do User's globals
        let top = Addon::load(Path::new("tests/dependencies/Top/Top.toc")).unwrap();
        let top = analyse_addon(&top, &[], &HashMap::new(), &definitions, &mut cache);
        assert_eq!(assigned(&top, "throughUser"), "string");
        assert_eq!(assigned(&top, "fromUser"), "number");
        assert_eq!(top.involves.len(), 3);
        // Alone, Lib's global is unknown to User
        let (alone, _) = user.analyse(Arc::default(), &HashMap::new(), &definitions);
        let from_lib = alone.references.iter().find(|r| r.write && alone.binding(r).name == "fromLib").unwrap();
        assert_eq!(alone.describe(from_lib.types), "unknown");
    }

    #[test]
    fn definitions_in_dependencies() {
        let user = Addon::load(Path::new("tests/dependencies/User/User.toc")).unwrap();
        let definitions = Arc::new(Definitions::default());
        let analysed = analyse_addon(&user, &[], &HashMap::new(), &definitions, &mut HashMap::new());
        let source = std::fs::read_to_string(&analysed.paths[0]).unwrap();
        let lib_path = normalise(Path::new("tests/dependencies/Lib/Lib.lua"));
        let lib = std::fs::read_to_string(&lib_path).unwrap();
        // Where in Lib's file what's at the first `name` in User's is defined
        let definition = |name: &str| -> Vec<String> {
            let offset = rowan::TextSize::from(source.find(name).unwrap() as u32);
            analysed.analysis.definition(crate::types::FileId(0), offset).iter().map(|site| {
                assert_eq!(crate::types::layer_file_key(site.file).map(PathBuf::from), Some(lib_path.clone()));
                lib[usize::from(site.range.start())..].lines().next().unwrap().to_string()
            }).collect()
        };
        assert_eq!(definition("LibGlobal"), ["LibGlobal = {}"]);
        assert_eq!(definition("Get"), ["Get()"]);
        // Each addon's namespace is its own, so Lib's field isn't User's
        assert!(definition("secret").is_empty());
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
