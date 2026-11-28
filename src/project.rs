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
//! XML files are followed to the Lua and XML files they load
//! (`<Script file="..."/>`, `<Include file="..."/>`), in place.
//!
//! The game version is taken to be the current one: `[Family]` in a path is
//! `Mainline` and `[Game]` is `Standard`, and `[AllowLoadGameType ...]` and
//! `[ExcludeLoadGameType ...]` match `mainline` and `standard`. Lines with
//! other variables, like `[TextLocale]`, are skipped. Other conditions are
//! ignored, so their files load.
//!
//! The files other versions load are added too, where they'd load, so their
//! code is checked and what they define is seen: for a `[Family]` or `[Game]`
//! line, those for other values, like `Classic/A.lua` for `[Family]/A.lua`,
//! and the files of lines whose conditions leave them out. Each comes with
//! what it loads, if it's XML (see `Variant`). One is left out if it clashes
//! with the current version's files, or with one before it that's kept: the
//! game never loads them together, and their types would mix. XML clashes by
//! naming what's named in other XML, when the `.toc` is read. Lua clashes by
//! assigning a global or field the others assign (or those of the addons they
//! depend on), or one their XML names, when the addon is analysed; its XML's
//! templates and frames are still known then, as definitions come first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::definitions::Definitions;
use crate::syntax::SyntaxNode;
use crate::syntax::syntax::Generator;
use crate::types::{Analysis, FileId, Handler, HashSet, Layer, SourceFile, Written};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addon {
    pub name: String,
    pub toc: PathBuf,
    /// Lua files in load order, including those XML files load
    pub files: Vec<PathBuf>,
    /// XML files in load order, including those other XML files include
    pub xml: Vec<PathBuf>,
    /// The addons it needs or uses if they're there, which load before it, by
    /// name as written
    pub dependencies: Vec<String>,
    /// The globals the game saves between sessions (`## SavedVariables` and
    /// `## SavedVariablesPerCharacter`)
    pub saved_variables: Vec<String>,
    /// What only other game versions load, whose files are in `files` and
    /// `xml`, in load order, analysed only if they don't clash with the rest
    pub variants: Vec<Variant>,
}

/// A file only other game versions load, with what it loads if it's XML and
/// the current version doesn't: kept or left out together
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Variant {
    /// The Lua files, in load order
    pub files: Vec<PathBuf>,
    /// The XML files, in load order
    pub xml: Vec<PathBuf>,
}

impl Addon {
    pub fn load(toc: &Path) -> std::io::Result<Addon> {
        let text = std::fs::read_to_string(toc)?;
        let directory = toc.parent().unwrap_or(Path::new(""));
        let entries = toc_entries(&text);
        // What the current version loads, which no variant brings
        let (mut current_files, mut current_xml) = (Vec::new(), Vec::new());
        for entry in entries.iter().filter(|entry| entry.loads) {
            add_file(normalise(&directory.join(&entry.file)), &mut current_files, &mut current_xml);
        }
        let mut named: HashSet<String> = current_xml.iter().flat_map(|path| xml_names(path)).map(|(name, _)| name).collect();
        let (mut files, mut xml, mut variants) = (Vec::new(), Vec::new(), Vec::new());
        for entry in &entries {
            if entry.loads {
                add_file(normalise(&directory.join(&entry.file)), &mut files, &mut xml);
            }
            for path in other_versions(directory, entry) {
                let mut variant = Variant::default();
                add_file(normalise(&path), &mut variant.files, &mut variant.xml);
                variant.files.retain(|f| !current_files.contains(f) && !files.contains(f));
                variant.xml.retain(|f| !current_xml.contains(f) && !xml.contains(f));
                if variant.files.is_empty() && variant.xml.is_empty() {
                    continue
                }
                let names: Vec<String> = variant.xml.iter().flat_map(|path| xml_names(path)).map(|(name, _)| name).collect();
                if names.iter().any(|name| named.contains(name)) {
                    continue
                }
                named.extend(names);
                files.extend(variant.files.iter().cloned());
                xml.extend(variant.xml.iter().cloned());
                variants.push(variant);
            }
        }
        let name = directory.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Ok(Addon { name, toc: normalise(toc), files, xml, dependencies: toc_dependencies(&text), saved_variables: toc_saved_variables(&text), variants })
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
    /// skipped. After its Lua files come the script handlers its XML writes
    /// inline, each a file of its own. Returns the Lua files analysed, whose
    /// positions are their `FileId`s, and then a name for each handler, by
    /// its XML file and where in it it starts
    pub fn analyse(&self, layer: Arc<Layer>, open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>) -> (Analysis, Vec<PathBuf>, Vec<PathBuf>) {
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
            sources.push(SourceFile { root, addon: Some(0), handler: None });
            paths.push(path.clone());
        }
        let mut scripts = Vec::new();
        let is_object = |tag: &str| definitions.element(tag).is_some();
        for path in &self.xml {
            let Some(text) = open.get(path).cloned().or_else(|| crate::xml::read(path).ok()) else { continue };
            let Ok(document) = crate::xml::parse(&text) else { continue };
            let objects = crate::xml::objects(&document, &is_object);
            for script in crate::xml::inline_scripts(&objects, &text) {
                let Some(self_type) = definitions.xml_object(script.object.as_deref(), &script.tag) else { continue };
                let parameters = crate::xml::handler_parameters(&script.handler).iter().map(|&(name, types)| (name.to_string(), crate::types::Type::of(types))).collect();
                let root = SyntaxNode::new_root(Generator::new(&script.lua).process_all());
                sources.push(SourceFile { root, addon: None, handler: Some(Handler { self_type, parameters }) });
                scripts.push(PathBuf::from(format!("{}#{}", path.display(), script.start)));
            }
        }
        (Analysis::analyse_over(&sources, definitions, layer, &self.saved_variables), paths, scripts)
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
    /// The XML script handlers analysed, by `FileId` after `paths`
    pub scripts: Vec<PathBuf>,
    /// Every file the analysis depends on: its own, and its dependencies',
    /// however indirectly. A change to any of them makes it out of date
    pub involves: Vec<PathBuf>,
    /// What it makes visible to addons depending on it, made when one needs it
    layer: std::sync::OnceLock<Arc<Layer>>,
}

impl Analysed {
    /// What this addon (and those it depends on) make visible to its dependents
    pub fn layer(&self) -> Arc<Layer> {
        self.layer.get_or_init(|| Arc::new(Layer::of(&self.analysis, &[&self.paths[..], &self.scripts[..]].concat(), &self.addon.toc.to_string_lossy()))).clone()
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
    let (analysis, paths, scripts) = analyse_without_clashes(addon, Arc::new(layer), open, definitions);
    // Those left out too, as a change may stop them clashing
    for path in paths.iter().chain(addon.variants.iter().flat_map(|v| &v.files)) {
        if !involves.contains(path) {
            involves.push(path.clone());
        }
    }
    let analysed = Arc::new(Analysed { addon: addon.clone(), analysis, paths, scripts, involves, layer: Default::default() });
    cache.insert(addon.toc.clone(), analysed.clone());
    analysed
}

/// `addon.analyse`, leaving out the files other game versions load that
/// clash with the rest (see `clashing_variants`). Those clashing in one
/// analysis are left out of the next until none do
fn analyse_without_clashes(addon: &Addon, layer: Arc<Layer>, open: &HashMap<PathBuf, String>, definitions: &Arc<Definitions>) -> (Analysis, Vec<PathBuf>, Vec<PathBuf>) {
    let mut kept = addon.clone();
    loop {
        let (analysis, paths, scripts) = kept.analyse(layer.clone(), open, definitions);
        let clashing = clashing_variants(&analysis, &paths, &scripts, &kept);
        if clashing.is_empty() {
            return (analysis, paths, scripts)
        }
        for variant in clashing.iter().map(|&i| &kept.variants[i]) {
            kept.files.retain(|f| !variant.files.contains(f));
            kept.xml.retain(|f| !variant.xml.contains(f));
        }
        kept.variants = std::mem::take(&mut kept.variants).into_iter().enumerate()
            .filter(|(index, _)| !clashing.contains(index))
            .map(|(_, variant)| variant)
            .collect();
    }
}

/// Of `addon`'s variants, by position, those that assign a global or a field
/// of a table that the rest of its files (`paths`, then the handlers of its
/// XML, `scripts`) assign, or that the addons depended on do, or that a
/// variant before them that doesn't clash does. Its XML's frames count as
/// assigning their globals
fn clashing_variants(analysis: &Analysis, paths: &[PathBuf], scripts: &[PathBuf], addon: &Addon) -> Vec<usize> {
    if addon.variants.is_empty() {
        return Vec::new()
    }
    let in_xml = |script: &PathBuf, xml: &PathBuf| script.to_string_lossy().rsplit_once('#').is_some_and(|(path, _)| Path::new(path) == xml);
    let variant_of = |file: FileId| {
        let index = file.0 as usize;
        match paths.get(index) {
            Some(path) => addon.variants.iter().position(|v| v.files.contains(path)),
            None => {
                let script = scripts.get(index - paths.len())?;
                addon.variants.iter().position(|v| v.xml.iter().any(|xml| in_xml(script, xml)))
            }
        }
    };
    let mut taken: HashSet<Written> = HashSet::default();
    let mut by_variant: Vec<Vec<Written>> = vec![Vec::new(); addon.variants.len()];
    for (file, written) in analysis.writes() {
        match variant_of(file) {
            Some(variant) => by_variant[variant].push(written),
            None => { taken.insert(written); }
        }
    }
    for xml in &addon.xml {
        let globals = xml_names(xml).into_iter().filter(|&(_, global)| global).map(|(name, _)| Written::Global(name));
        match addon.variants.iter().position(|v| v.xml.contains(xml)) {
            Some(variant) => by_variant[variant].extend(globals),
            None => taken.extend(globals),
        }
    }
    let layer = &analysis.layer;
    taken.extend(layer.globals.keys().map(|name| Written::Global(name.clone())));
    for (&at, fields) in &layer.field_sites {
        taken.extend(fields.keys().map(|name| Written::Field(at, name.clone())));
    }
    let mut clashing = Vec::new();
    for (index, writes) in by_variant.into_iter().enumerate() {
        if writes.iter().any(|w| taken.contains(w)) {
            clashing.push(index);
        } else {
            taken.extend(writes);
        }
    }
    clashing
}

/// The names an XML file gives what it defines, with whether each is a
/// global (see `xml::defined_names`). None if it can't be read
fn xml_names(path: &Path) -> Vec<(String, bool)> {
    let Ok(text) = crate::xml::read(path) else { return Vec::new() };
    let Ok(document) = crate::xml::parse(&text) else { return Vec::new() };
    crate::xml::defined_names(&document)
}

/// An XML file that doesn't parse: where (a byte offset into its text), and why
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError {
    pub path: PathBuf,
    pub offset: usize,
    pub message: String,
}

/// The XML files of `addons` and of every addon they depend on, however
/// indirectly, each addon's after those of the addons it depends on
pub fn xml_files(addons: &[Addon], known: &[Addon]) -> Vec<PathBuf> {
    fn visit(addon: &Addon, known: &[Addon], visited: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) {
        if visited.contains(&addon.toc) {
            return
        }
        visited.push(addon.toc.clone());
        for dependency in addon.direct_dependencies(known) {
            visit(&dependency, known, visited, files);
        }
        for file in &addon.xml {
            if !files.contains(file) {
                files.push(file.clone());
            }
        }
    }
    let (mut visited, mut files) = (Vec::new(), Vec::new());
    for addon in addons {
        visit(addon, known, &mut visited, &mut files);
    }
    files
}

/// `base` with the templates and named frames of XML `files`, in that order
/// (see `Definitions::with_xml`). An open document's text is read rather
/// than the file. One that doesn't parse is read as it was the last time it
/// did, which `valid` keeps for each file, and its error is returned
pub fn with_xml(base: &Arc<Definitions>, files: &[PathBuf], open: &HashMap<PathBuf, String>, valid: &mut HashMap<PathBuf, String>) -> (Arc<Definitions>, Vec<XmlError>) {
    let is_object = |tag: &str| base.element(tag).is_some();
    let mut objects = Vec::new();
    let mut sites = Vec::new();
    let mut errors = Vec::new();
    for path in files {
        let text = match open.get(path) {
            Some(text) => text.clone(),
            None => match crate::xml::read(path) {
                Ok(text) => text,
                Err(_) => continue,
            },
        };
        let file = path.display().to_string();
        let mut read = |text: &str| {
            let Ok(document) = crate::xml::parse(text) else { return false };
            let found = crate::xml::objects(&document, &is_object);
            sites.extend(crate::xml::sites(&found, &file, text));
            objects.extend(found);
            true
        };
        if read(&text) {
            valid.insert(path.clone(), text);
            continue
        }
        if let Err(error) = crate::xml::parse(&text) {
            errors.push(XmlError { path: path.clone(), offset: crate::xml::error_offset(&text, &error), message: error.to_string() });
        }
        if let Some(text) = valid.get(path).cloned() {
            read(&text);
        }
    }
    if objects.is_empty() {
        return (base.clone(), errors)
    }
    (Arc::new(base.with_xml(&objects, sites)), errors)
}

/// A Lua or XML file to load, and for an XML file what it loads, in order,
/// unless it's already there
fn add_file(path: PathBuf, files: &mut Vec<PathBuf>, xml: &mut Vec<PathBuf>) {
    let path = ignoring_case(path);
    if !is_xml(&path) {
        if !files.contains(&path) {
            files.push(path);
        }
        return
    }
    if xml.contains(&path) {
        return
    }
    xml.push(path.clone());
    let Ok(text) = crate::xml::read(&path) else { return };
    let Ok(document) = crate::xml::parse(&text) else { return };
    let directory = path.parent().unwrap_or(Path::new(""));
    for load in crate::xml::loads(&document) {
        let (crate::xml::Load::Lua(file) | crate::xml::Load::Xml(file)) = load;
        add_file(normalise(&directory.join(file)), files, xml);
    }
}

/// The file `path` names when the game reads it, ignoring case as it does,
/// if it's there under another case and not under this one
fn ignoring_case(path: PathBuf) -> PathBuf {
    if path.exists() {
        return path
    }
    let mut found = PathBuf::new();
    for component in path.components() {
        let next = found.join(component);
        if next.exists() || !found.is_dir() {
            found = next;
            continue
        }
        let wanted = component.as_os_str().to_string_lossy().to_lowercase();
        let entries = std::fs::read_dir(&found).into_iter().flatten().flatten();
        match entries.map(|e| e.file_name()).find(|n| n.to_string_lossy().to_lowercase() == wanted) {
            Some(name) => found.push(name),
            None => return path,
        }
    }
    normalise(&found)
}

pub fn is_xml(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml"))
}

/// The Lua and XML files a `.toc` lists that load in the current game version,
/// as written (relative to its folder)
#[cfg(test)]
pub fn toc_files(text: &str) -> Vec<PathBuf> {
    toc_entries(text).into_iter().filter(|entry| entry.loads).map(|entry| entry.file).collect()
}

/// A Lua or XML file a `.toc` lists
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    /// As written (relative to the `.toc`'s folder), as the current game
    /// version reads it
    pub file: PathBuf,
    /// For one under `[Family]` or `[Game]`, as written with them left in, to
    /// find the files other versions load in its place by
    pub template: Option<PathBuf>,
    /// Whether its conditions let it load in the current version
    pub loads: bool,
}

/// The Lua and XML files a `.toc` lists, in order
pub fn toc_entries(text: &str) -> Vec<TocEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue
        }
        // Loading conditions after the path, like `File.lua [AllowLoadGameType mainline]`
        let split = line.char_indices()
            .find(|&(i, c)| c == '[' && line[..i].ends_with(char::is_whitespace))
            .map_or(line.len(), |(i, _)| i);
        let (written, conditions) = (line[..split].trim(), &line[split..]);
        let loads = conditions.split('[').all(loads_here);
        let entry = written.replace("[Family]", "Mainline").replace("[Game]", "Standard");
        let lower = entry.to_ascii_lowercase();
        if entry.contains('[') || !(lower.ends_with(".lua") || lower.ends_with(".xml")) {
            continue
        }
        let template = (entry != written).then(|| PathBuf::from(written.replace('\\', "/")));
        entries.push(TocEntry { file: PathBuf::from(entry.replace('\\', "/")), template, loads });
    }
    entries
}

/// The files there are that other game versions load for a `.toc` entry (in
/// `directory`'s `.toc`) and the current version doesn't: for one under
/// `[Family]` or `[Game]`, those for other values, and for one whose
/// conditions leave it out, its own
fn other_versions(directory: &Path, entry: &TocEntry) -> Vec<PathBuf> {
    let current = ignoring_case(directory.join(&entry.file));
    match &entry.template {
        Some(template) => variant_files(directory, template).into_iter()
            .filter(|path| !entry.loads || path.to_string_lossy().to_lowercase() != current.to_string_lossy().to_lowercase())
            .collect(),
        None if !entry.loads && current.is_file() => vec![current],
        None => Vec::new(),
    }
}

/// The Lua and XML files there are for a `.toc` entry under `[Family]` or
/// `[Game]` (`template`, relative to `directory`) with the variables standing
/// for any name, as `Classic/A.lua` and `Mainline/A.lua` are for
/// `[Family]/A.lua`, ignoring case as the game does. Sorted by name
fn variant_files(directory: &Path, template: &Path) -> Vec<PathBuf> {
    let mut found = vec![directory.to_path_buf()];
    for component in template.components() {
        let part = component.as_os_str().to_string_lossy();
        if !part.contains("[Family]") && !part.contains("[Game]") {
            found = found.into_iter().map(|path| ignoring_case(path.join(&*part))).collect();
            continue
        }
        let pieces: Vec<String> = part.replace("[Family]", "\0").replace("[Game]", "\0").to_lowercase().split('\0').map(str::to_string).collect();
        let mut next = Vec::new();
        for parent in found {
            let mut names: Vec<String> = std::fs::read_dir(&parent).into_iter().flatten().flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') && matches_pieces(&pieces, &name.to_lowercase()))
                .collect();
            names.sort();
            next.extend(names.iter().map(|name| parent.join(name)));
        }
        found = next;
    }
    found.retain(|path| path.is_file() && (is_xml(path) || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lua"))));
    found
}

/// Whether `name` is `pieces` with something (at least a character) between
/// each two
fn matches_pieces(pieces: &[String], name: &str) -> bool {
    let Some((first, rest)) = pieces.split_first() else { return name.is_empty() };
    let Some(name) = name.strip_prefix(first.as_str()) else { return false };
    if rest.is_empty() {
        return name.is_empty()
    }
    name.char_indices().skip(1).map(|(i, _)| i).chain([name.len()]).filter(|&i| i > 0)
        .any(|i| matches_pieces(rest, &name[i..]))
}

/// Whether a `.toc` loading condition, like `AllowLoadGameType mainline, classic]`
/// (after its `[`), lets the file load in the current game version
fn loads_here(condition: &str) -> bool {
    let condition = condition.trim().trim_end_matches(']');
    let (key, values) = condition.split_once(char::is_whitespace).unwrap_or((condition, ""));
    let current = values.split(',').any(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "mainline" | "standard"));
    match key.to_ascii_lowercase().as_str() {
        "allowloadgametype" => current,
        "excludeloadgametype" => !current,
        _ => true,
    }
}

/// The addons a `.toc` says this one needs or uses: `## Dependencies`,
/// `## RequiredDeps` (or any `## Dep...`), and `## OptionalDeps`
/// The globals a toc's `## SavedVariables` lines (per account, per character
/// and any other kind) name
pub fn toc_saved_variables(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(metadata) = line.trim_start_matches('\u{feff}').trim().strip_prefix("##") else { continue };
        let Some((key, value)) = metadata.split_once(':') else { continue };
        if !key.trim().to_ascii_lowercase().starts_with("savedvariables") {
            continue
        }
        for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if !names.iter().any(|n: &String| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

pub fn toc_dependencies(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(metadata) = line.trim_start_matches('\u{feff}').trim().strip_prefix("##") else { continue };
        let Some((key, value)) = metadata.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        if !(key.starts_with("dep") || key == "requireddeps" || key == "optionaldeps") {
            continue
        }
        // Loading conditions after the names, like `Lib [AllowLoadGameType mainline]`
        let (value, conditions) = value.split_once('[').map_or((value, ""), |(v, c)| (v, c));
        if !conditions.split('[').all(loads_here) {
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
        let toc = "\u{feff}## Interface: 110000\n## Title: Test\n# a comment\n\nCore.lua\nModules\\Bags.lua\nFrames.xml\n  Spaced.lua  \nLater.lua [AllowLoadGameType mainline]\nLocales\\[TextLocale].lua\nUPPER.LUA\nReadme.txt\n";
        let files: Vec<String> = toc_files(toc).iter().map(|f| f.to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["Core.lua", "Modules/Bags.lua", "Frames.xml", "Spaced.lua", "Later.lua", "UPPER.LUA"]);
    }

    #[test]
    fn loads_files_for_the_current_game_version() {
        let toc = "[Family]\\A.lua\n[Game]\\B.lua [AllowLoadGameType camelot]\nC.lua\t[AllowLoadGameType standard, camelot]\nD.lua [AllowLoadGameType classic]\nE.lua [ExcludeLoadGameType classic, camelot]\nF.lua [ExcludeLoadGameType mainline]\nG.lua [AllowLoadGameType mists, mainline] [AllowLoad Game]\nH.lua [Bootstrap]\n";
        let files: Vec<String> = toc_files(toc).iter().map(|f| f.to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["Mainline/A.lua", "C.lua", "E.lua", "G.lua", "H.lua"]);
    }

    #[test]
    fn adds_other_versions_files_that_dont_clash() {
        let directory = std::env::temp_dir().join(format!("wow_ls_variants_{}", std::process::id())).join("Variants");
        let write = |path: &str, text: &str| {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Variants.toc", "Core.lua\n[Family]\\Bags.lua\nLoot_[Game].lua\n[Family]\\Frames.xml\nClassicOnly.lua [AllowLoadGameType classic]\nClash.lua [ExcludeLoadGameType mainline]\nLater.lua\n");
        write("Core.lua", "local _, ns = ... ns.Shared = {}");
        write("Mainline/Bags.lua", "local _, ns = ... function ns.Shared.Open() end MainlineOnly = 1");
        // Assigns what the mainline file does
        write("Classic/Bags.lua", "local _, ns = ... function ns.Shared.Open() end");
        // Assigns only its own
        write("Vanilla/Bags.lua", "local _, ns = ... function ns.Shared.VanillaOpen() end VanillaOnly = 1");
        write("Vanilla/Bags.xml", "<Ui/>");
        // No mainline file (it is listed, but not read): the first kept, the one after assigning the same left out
        write("Loot_Mists.lua", "LootHelper = 1 function GetLoot() return LootHelper end");
        write("Loot_Wrath.lua", "LootHelper = 2");
        write("Loot_.lua", "Empty = 1");
        write("Mainline/Frames.xml", r#"<Ui><Script file="Frames.lua"/><Frame name="MainFrame"/><Frame name="Template" virtual="true"><Frames><Frame name="$parentChild"/></Frames></Frame></Ui>"#);
        write("Mainline/Frames.lua", "");
        // Names a template the mainline XML does: left out with its Lua
        write("Classic/Frames.xml", r#"<Ui><Script file="ClassicFrames.lua"/><Frame name="Template" virtual="true"/></Ui>"#);
        write("Classic/ClassicFrames.lua", "ClassicFrames = 1");
        // Its own names, and a Lua file of its own: kept together. A child
        // made from its parent's name isn't a clash
        write("Vanilla/Frames.xml", r#"<Ui><Script file="VanillaFrames.lua"/><Include file="..\Mainline\Frames.xml"/><Frame name="VanillaFrame"><Frames><Frame name="$parentChild"/></Frames></Frame></Ui>"#);
        write("Vanilla/VanillaFrames.lua", "VanillaFrames = VanillaFrame");
        // Names a global the mainline Lua assigns, in a handler
        write("Mists/Frames.xml", r#"<Ui><Script file="MistsFrames.lua"/><Frame name="MistsFrame"><Scripts><OnLoad>MainlineOnly = 2</OnLoad></Scripts></Frame></Ui>"#);
        write("Mists/MistsFrames.lua", "MistsFrames = 1");
        // Its frame is a global the mainline XML makes
        write("Wrath/Frames.xml", r#"<Ui><Script file="WrathFrames.lua"/></Ui>"#);
        write("Wrath/WrathFrames.lua", "MainFrame = 1");
        // Lines the current version leaves out
        write("ClassicOnly.lua", "ClassicOnly = 1");
        write("Clash.lua", "VanillaOnly = 2");
        write("Later.lua", "local _, ns = ... ns.Shared.VanillaOpen()");
        let addon = Addon::load(&directory.join("Variants.toc")).unwrap();
        let relative = |paths: &[PathBuf]| -> Vec<String> {
            let root = normalise(&directory);
            paths.iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect()
        };
        assert_eq!(relative(&addon.files), [
            "Core.lua", "Mainline/Bags.lua", "Classic/Bags.lua", "Vanilla/Bags.lua", "Loot_Standard.lua", "Loot_Mists.lua", "Loot_Wrath.lua",
            "Mainline/Frames.lua", "Mists/MistsFrames.lua", "Vanilla/VanillaFrames.lua", "Wrath/WrathFrames.lua", "ClassicOnly.lua", "Clash.lua", "Later.lua",
        ]);
        assert_eq!(relative(&addon.xml), ["Mainline/Frames.xml", "Mists/Frames.xml", "Vanilla/Frames.xml", "Wrath/Frames.xml"]);
        let variants: Vec<Vec<String>> = addon.variants.iter().map(|v| [relative(&v.xml), relative(&v.files)].concat()).collect();
        assert_eq!(variants, [
            vec!["Classic/Bags.lua"], vec!["Vanilla/Bags.lua"], vec!["Loot_Mists.lua"], vec!["Loot_Wrath.lua"],
            // Not the mainline XML the Vanilla one includes
            vec!["Mists/Frames.xml", "Mists/MistsFrames.lua"], vec!["Vanilla/Frames.xml", "Vanilla/VanillaFrames.lua"], vec!["Wrath/Frames.xml", "Wrath/WrathFrames.lua"],
            vec!["ClassicOnly.lua"], vec!["Clash.lua"],
        ]);
        let definitions = Arc::new(Definitions::from_json(r#"{"types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}}, "method_tables": {"FrameAPI": {"functions": {"Show": {}}}}, "elements": {"Frame": "Frame"}}"#).unwrap());
        let (definitions, errors) = with_xml(&definitions, &addon.xml, &HashMap::new(), &mut HashMap::new());
        assert!(errors.is_empty());
        let analysed = analyse_addon(&addon, &[], &HashMap::new(), &definitions, &mut HashMap::new());
        assert_eq!(relative(&analysed.paths), ["Core.lua", "Mainline/Bags.lua", "Vanilla/Bags.lua", "Loot_Mists.lua", "Mainline/Frames.lua", "Vanilla/VanillaFrames.lua", "ClassicOnly.lua", "Later.lua"]);
        // Not the handler of the XML left out
        assert_eq!(analysed.scripts.len(), 0);
        // A change to one left out may stop it clashing
        assert!(analysed.involves.contains(&addon.variants[0].files[0]));
        // What the ones kept make is seen by the rest
        assert_eq!(assigned(&analysed, "VanillaOnly"), "number");
        assert_eq!(assigned(&analysed, "VanillaFrames"), "Frame");
        std::fs::remove_dir_all(directory.parent().unwrap()).unwrap();
    }

    #[test]
    fn reads_toc_dependencies() {
        let toc = "## Interface: 110000\n## OptionalDeps: Syndicator, Masque\n## RequiredDeps: LibA\n## Dependencies: LibB, liba\n## DepsExtra: LibC\n## SavedVariables: X\nCore.lua\n";
        assert_eq!(toc_dependencies(toc), ["Syndicator", "Masque", "LibA", "LibB", "LibC"]);
        let toc = "## SavedVariables: A, B\n## SavedVariablesPerCharacter: C\n## SavedVariables: A\n";
        assert_eq!(toc_saved_variables(toc), ["A", "B", "C"]);
        // With loading conditions, as Blizzard's have
        let toc = "## Dep: Blizzard_EditMode [AllowLoadGameType mainline]\n## Dep: Blizzard_Classic [AllowLoadGameType classic]\n## Dep: Blizzard_Colors\n";
        assert_eq!(toc_dependencies(toc), ["Blizzard_EditMode", "Blizzard_Colors"]);
    }

    #[test]
    fn finds_direct_dependencies() {
        let addon = |name: &str, dependencies: &[&str]| Addon {
            name: name.to_string(),
            toc: PathBuf::from(format!("/nowhere/{name}/{name}.toc")),
            files: Vec::new(),
            xml: Vec::new(),
            dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
            saved_variables: Vec::new(),
            variants: Vec::new(),
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
    fn finds_files_ignoring_case() {
        let found = ignoring_case(PathBuf::from("tests/ADDON/testaddon/core.LUA"));
        assert_eq!(found, normalise(Path::new("tests/addon/TestAddon/Core.lua")));
        // Missing either way
        assert_eq!(ignoring_case(PathBuf::from("tests/addon/Nothing.lua")), PathBuf::from("tests/addon/Nothing.lua"));
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
            .find(|r| r.write && analysis.binding(r).name.as_ref() == name)
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
        let (alone, _, _) = user.analyse(Arc::default(), &HashMap::new(), &definitions);
        let from_lib = alone.references.iter().find(|r| r.write && alone.binding(r).name.as_ref() == "fromLib").unwrap();
        assert_eq!(alone.describe(from_lib.types), "unknown");
    }

    #[test]
    fn xml_types_from_the_last_text_that_parsed() {
        let base = Arc::new(Definitions::from_json(r#"{
            "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}},
            "method_tables": {"FrameAPI": {"functions": {"Show": {}}}},
            "elements": {"Frame": "Frame"}
        }"#).unwrap());
        let path = PathBuf::from("/nowhere/Frames.xml");
        let files = [path.clone()];
        let mut open = HashMap::new();
        let mut valid = HashMap::new();
        open.insert(path.clone(), r#"<Ui><Frame name="T" virtual="true"/></Ui>"#.to_string());
        let (definitions, errors) = with_xml(&base, &files, &open, &mut valid);
        assert!(errors.is_empty() && definitions.template("T").is_some());
        // Half typed: the error is reported, and the last text that parsed typed
        let broken = r#"<Ui><Frame name="T" virtual="true"/><Frame name="U" virtual="true"></Ui>"#;
        open.insert(path.clone(), broken.to_string());
        let (definitions, errors) = with_xml(&base, &files, &open, &mut valid);
        assert_eq!(errors.len(), 1);
        assert_eq!(&broken[errors[0].offset..], "</Ui>");
        assert!(definitions.template("T").is_some() && definitions.template("U").is_none());
        // Never parsed: nothing to type from
        let (definitions, errors) = with_xml(&base, &files, &open, &mut HashMap::new());
        assert!(errors.len() == 1 && definitions.template("T").is_none());
    }

    #[test]
    fn xml_files_of_dependencies_first() {
        let user = Addon::load(Path::new("tests/dependencies/User/User.toc")).unwrap();
        assert_eq!(xml_files(std::slice::from_ref(&user), &[]), [
            normalise(Path::new("tests/dependencies/Lib/Lib.xml")),
            normalise(Path::new("tests/dependencies/User/User.xml")),
        ]);
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
    fn inline_xml_scripts_are_functions_of_their_object() {
        let definitions = Arc::new(Definitions::from_json(r#"{
            "types": {"Button": {"base": "table", "method_tables": ["ButtonAPI"]}, "Frame": {"base": "table", "method_tables": ["ButtonAPI"]}},
            "method_tables": {"ButtonAPI": {"functions": {"Show": {}}}},
            "functions": {"Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]}},
            "elements": {"Button": "Button", "Frame": "Frame"}
        }"#).unwrap());
        let xml = PathBuf::from("/nowhere/Inline/Frames.xml");
        let lua = PathBuf::from("/nowhere/Inline/Mixins.lua");
        let mut open = HashMap::new();
        open.insert(xml.clone(), r#"<Ui>
  <Button name="T" virtual="true">
    <Scripts>
      <OnLoad inherit="prepend">
        if 1 &lt; 2 then Mixin(self, M) end
      </OnLoad>
      <OnClick>clicked = button</OnClick>
    </Scripts>
    <Frames>
      <Frame parentKey="Child"><Scripts><OnShow><![CDATA[Mixin(self, C)]]></OnShow></Scripts></Frame>
    </Frames>
  </Button>
</Ui>"#.to_string());
        open.insert(lua.clone(), "M = {} function M:Get() return self end C = {} function C:Get() return self end".to_string());
        let addon = Addon { name: "Inline".into(), toc: "/nowhere/Inline/Inline.toc".into(), files: vec![lua.clone()], xml: vec![xml.clone()], dependencies: Vec::new(), saved_variables: Vec::new(), variants: Vec::new() };
        let (definitions, errors) = with_xml(&definitions, std::slice::from_ref(&xml), &open, &mut HashMap::new());
        assert!(errors.is_empty());
        let analysed = analyse_addon(&addon, &[], &open, &definitions, &mut HashMap::new());
        // Each handler is analysed after the Lua files, named by where it starts
        assert_eq!(analysed.paths, [lua]);
        assert_eq!(analysed.scripts.len(), 3);
        let analysis = &analysed.analysis;
        let selves = || -> Vec<String> {
            analysis.references.iter()
                .filter(|r| r.file == crate::types::FileId(0) && analysis.binding(r).name.as_ref() == "self")
                .map(|r| analysis.describe(r.types))
                .collect()
        };
        // Shown by its native type, not the template or mixin it's made with
        assert_eq!(selves(), ["Button", "Frame"]);
        // `self` in each mixin's methods is the object the XML mixes it into
        let _made_with = crate::types::MadeWith::shown();
        assert_eq!(selves(), ["T & M", "Frame & C"]);
        // The game's names for a handler's arguments are its parameters, of
        // the types it passes
        assert_eq!(assigned(&analysed, "clicked"), "string");
        assert!(analysis.bindings.iter().any(|b| b.name.as_ref() == "button" && b.kind == crate::types::BindingKind::Parameter));
    }

    #[test]
    fn discovers_the_test_addon() {
        let addons = discover(Path::new("tests/addon"));
        assert_eq!(addons.len(), 1);
        assert_eq!(addons[0].name, "TestAddon");
        let names: Vec<_> = addons[0].files.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        // With the Lua file its XML file loads, in its place
        assert_eq!(names, ["Core.lua", "Util.lua", "Frames.lua", "Main.lua"]);
        let xml: Vec<_> = addons[0].xml.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(xml, ["Frames.xml"]);
    }
}

