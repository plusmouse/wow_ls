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

//! Generates `blizzard_templates.json` from wow-ui-source: the frames and
//! templates Blizzard's XML defines, and the methods of the Lua mixins they use.
//!
//! Which wow_ls type an element makes comes from UI.xsd (see
//! `xml::Schema::object_type`): its schema type's name without `Type` when
//! that's a known WoW object type, else the first type it extends that is. A
//! kind of object of its own that isn't known (`ModelFFX`, `FogOfWarFrame`)
//! may have methods beyond that type's, so it's marked `extended`. An
//! intrinsic (`<Button name="ItemButton" intrinsic="true">`), usable as an
//! element of its own, gets a type of its own name, inheriting what its
//! element makes, with the methods of its mixins.
//!
//! Mixins' methods come from wow_ls's own analysis of Blizzard's addons, each
//! from its `.toc` over the addons it depends on. A method's parameters are
//! given without types, and its returns as the analysis infers them. A field
//! that may be a function the analysis can't pin down is a method taking and
//! giving anything. A mixin holding functions under computed keys may have
//! methods beyond those, so it's marked `open`.
//!
//! A `KeyValue` is typed by its `type`: boolean, number, string (also when it
//! has none) or function, or for `global`, the type of the global its value
//! names (`Enum.X.Y`), from the definitions or the analysis, else unknown.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::definitions::Definitions;
use crate::project::{self, Analysed};
use crate::types::{FunctionInfo, FunctionRef, Layer, Type, TypeSet};
use crate::xml::{self, Object, Schema};

/// What the elements of Blizzard's XML make: the type of each object element
/// (and whether it may have more methods than that type), for its tag
struct Kinds<'a> {
    schema: Schema,
    definitions: &'a Definitions,
    /// Intrinsic element names, with the tag each is defined by
    intrinsics: HashMap<String, String>,
}

impl Kinds<'_> {
    fn is_object_type(&self, name: &str) -> bool {
        self.definitions.named_type_id(name)
            .and_then(|id| self.definitions.named_type(id))
            .is_some_and(|named| named.has_methods && !named.namespace)
    }

    fn of(&self, tag: &str) -> Option<(String, bool)> {
        if self.intrinsics.contains_key(tag) {
            return Some((tag.to_string(), false))
        }
        self.schema.object_type(tag, |name| self.is_object_type(name))
    }

    /// The type an intrinsic inherits: what its own element makes
    fn intrinsic_base(&self, name: &str) -> Option<String> {
        let tag = self.intrinsics.get(name)?;
        if tag == name {
            return None
        }
        self.of(tag).map(|(ty, _)| ty)
    }
}

/// What the generator found that it couldn't type, for its report
#[derive(Default)]
struct Report {
    missing_mixins: BTreeMap<String, usize>,
    /// Mixins that may have methods beyond those found
    open_mixins: Vec<String>,
    unknown_globals: Vec<String>,
    duplicates: Vec<String>,
}

pub fn templates(source: &Path, definitions_directory: &Path, out: &Path, colors: Option<&Path>) -> Result<(), String> {
    // Absolute, as the paths of the files read are
    let source = &project::normalise(source);
    let source = source.as_path();
    let addons_directory = source.join("Interface/AddOns");
    let definitions = Arc::new(Definitions::load_directory_except(definitions_directory, out.file_name())?);
    let xsd_path = addons_directory.join("Blizzard_SharedXML/UI.xsd");
    let xsd = xml::read(&xsd_path).map_err(|e| format!("{}: {e}", xsd_path.display()))?;
    let schema = Schema::parse(&xml::parse(&xsd).map_err(|e| format!("{}: {e}", xsd_path.display()))?);

    let mut addons = project::discover(&addons_directory);
    // The client's global colors, made as if by Lua after `ColorUtil.lua`
    // defines `CreateColorFromHexString`, so they're typed as colors it makes
    let mut open: HashMap<PathBuf, String> = HashMap::new();
    let mut color_names: HashSet<String> = HashSet::new();
    if let Some(csv) = colors {
        let (path, text, names) = global_colors(csv, &mut addons)?;
        eprintln!("{} global colors, from {}", names.len(), csv.display());
        open.insert(path, text);
        color_names = names;
    }
    let mut xml_files: Vec<PathBuf> = Vec::new();
    for addon in &addons {
        for file in &addon.xml {
            if !xml_files.contains(file) {
                xml_files.push(file.clone());
            }
        }
    }
    let mut texts = Vec::new();
    for path in &xml_files {
        if let Ok(text) = xml::read(path) {
            texts.push((path.clone(), text));
        }
    }

    // Intrinsics first, as any file can use one as an element
    let mut kinds = Kinds { schema, definitions: &definitions, intrinsics: HashMap::new() };
    for (path, text) in &texts {
        let document = xml::parse(text).map_err(|e| format!("{}: {e}", path.display()))?;
        for node in document.root_element().descendants().filter(|n| n.is_element()) {
            if node.attribute("intrinsic").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
                if let Some(name) = node.attribute("name") {
                    kinds.intrinsics.insert(name.to_string(), node.tag_name().name().to_string());
                }
            }
        }
    }

    let mut report = Report::default();
    let mut templates: BTreeMap<String, Object> = BTreeMap::new();
    let mut frames: BTreeMap<String, Object> = BTreeMap::new();
    // Where each thing is, by name, within wow-ui-source, for go to definition
    let mut sites: BTreeMap<String, String> = BTreeMap::new();
    // Every object, in load order, for the analysis to type them by
    let mut all_objects: Vec<Object> = Vec::new();
    for (path, text) in &texts {
        let document = xml::parse(text).map_err(|e| format!("{}: {e}", path.display()))?;
        let objects = xml::objects(&document, &|tag: &str| kinds.of(tag).is_some());
        let file = path.strip_prefix(source).unwrap_or(path).display().to_string();
        sites.extend(xml::sites(&objects, &file, text));
        all_objects.extend(objects.iter().cloned());
        for object in objects {
            let Some(name) = object.name.clone() else { continue };
            if object.is_virtual || object.intrinsic {
                if templates.insert(name.clone(), object.clone()).is_some() {
                    report.duplicates.push(name.clone());
                }
            }
            // An intrinsic is an element to use, not a frame
            if !object.is_virtual && !object.intrinsic {
                let mut found = Vec::new();
                xml::global_frames(&object, None, &mut found);
                frames.extend(found);
            }
        }
    }

    // What each element makes, for reading addons' XML without the schema
    let mut elements = Map::new();
    let mut tags: Vec<&str> = kinds.schema.tags().chain(kinds.intrinsics.keys().map(String::as_str)).collect();
    tags.sort();
    tags.dedup();
    for tag in tags {
        match kinds.of(tag) {
            Some((ty, false)) => { elements.insert(tag.to_string(), json!(ty)); }
            Some((ty, true)) => { elements.insert(tag.to_string(), json!({ "type": ty, "extended": true })); }
            None => {}
        }
    }

    // Analysed knowing Blizzard's templates and frames, as an addon's code is
    // with its own: so the script handlers its XML writes inline are walked,
    // each with `self` the object it's on, and its Lua mixins' `self` is the
    // objects XML mixes them into
    let analysis_definitions = Arc::new(definitions
        .with_elements(kinds.schema.tags().chain(kinds.intrinsics.keys().map(String::as_str)).filter_map(|tag| Some((tag.to_string(), kinds.of(tag)?))))
        .with_xml(&all_objects, Vec::new()));
    eprintln!("analysing {} addons...", addons.len());
    let started = std::time::Instant::now();
    let mut cache: HashMap<PathBuf, Arc<Analysed>> = HashMap::new();
    let mut layer = Layer::default();
    for addon in &addons {
        let analysed = project::analyse_addon(addon, &addons, &open, &analysis_definitions, &mut cache);
        layer.add(&analysed.layer());
    }
    let handlers: usize = cache.values().map(|analysed| analysed.scripts.len()).sum();
    eprintln!("analysed in {:?}, with {handlers} script handlers written inline in XML", started.elapsed());

    let giving_methods = giving_methods(&layer);
    let generator = Generator { kinds: &kinds, definitions: &analysis_definitions, layer: &layer, templates: &templates, source, texts: Default::default(), giving_methods };
    let mut mixin_names: Vec<String> = Vec::new();
    for object in templates.values().chain(frames.values()) {
        collect_mixins(object, &mut mixin_names);
    }
    mixin_names.sort();
    mixin_names.dedup();
    let mut mixins = Map::new();
    for name in &mixin_names {
        match generator.mixin(name) {
            Some((functions, values, open)) => {
                let mut mixin = Map::new();
                if open {
                    report.open_mixins.push(name.clone());
                    mixin.insert("open".into(), json!(true));
                }
                mixin.insert("functions".into(), Value::Object(functions));
                if !values.is_empty() {
                    mixin.insert("values".into(), Value::Object(values));
                }
                mixins.insert(name.clone(), Value::Object(mixin));
            }
            None => { report.missing_mixins.insert(name.clone(), 0); }
        }
    }

    // Every global Blizzard's Lua assigns that nothing else defines: not the
    // API, global strings or hand-written definitions, nor a named XML frame
    let (mut functions, mut tables, mut values) = (Map::new(), Map::new(), Map::new());
    let mut names: Vec<&String> = layer.globals.keys()
        .filter(|name| is_name(name) && definitions.global(name).is_none() && !frames.contains_key(*name))
        .collect();
    names.sort();
    for name in names {
        if let Some(site) = layer.global_sites.get(name).and_then(|found| found.first()).and_then(|site| generator.place(site)) {
            sites.entry(name.clone()).or_insert(site);
        }
        // The client makes each global color, so it's never nil, though
        // `CreateColorFromHexString` may return nil for a string the wrong length
        let ty = match color_names.contains(name.as_str()) {
            true => layer.globals[name].without(crate::types::TypeSet::NIL),
            false => layer.globals[name],
        };
        match generator.member(ty, name, &mut sites, &mut Vec::new()) {
            Member::Function(signature) => { functions.insert(name.clone(), signature); }
            Member::Table(table) => { tables.insert(name.clone(), table); }
            Member::Value(ty) => { values.insert(name.clone(), json!(ty)); }
        }
    }

    // Each intrinsic's type, with every mixin it gets from its template
    let mut types = Map::new();
    for name in kinds.intrinsics.keys() {
        let mut mixins_of = Vec::new();
        generator.template_mixins(name, &mut mixins_of, 0);
        let known = |m: &String| mixins.get(m).is_some_and(|m| m.get("open").is_none());
        let missing: Vec<&String> = mixins_of.iter().filter(|m| !known(m)).collect();
        let mut ty = Map::new();
        ty.insert("base".into(), json!("table"));
        // Without all its mixins its methods aren't known, so it's left
        // without any, rather than having its missing ones reported
        if missing.is_empty() {
            if let Some(base) = kinds.intrinsic_base(name) {
                ty.insert("inherits".into(), json!([base]));
            }
            ty.insert("mixins".into(), json!(mixins_of));
        } else {
            eprintln!("intrinsic {name} has mixins whose methods aren't all known: {missing:?}");
        }
        types.insert(name.clone(), Value::Object(ty));
    }

    let mut template_values = Map::new();
    for (name, object) in &templates {
        template_values.insert(name.clone(), generator.node(object, &mut report));
    }
    let mut frame_values = Map::new();
    for (name, object) in &frames {
        frame_values.insert(name.clone(), generator.node(object, &mut report));
    }

    let version = std::fs::read_to_string(source.join("version.txt")).unwrap_or_default();
    let generated = format!("by `wow_ls generate-templates` from wow-ui-source {}; regenerate rather than editing. `elements` are the type each element makes (`extended` when it may have methods beyond it), `types` the intrinsic elements, `mixins` the methods of the Lua mixins Blizzard's XML uses (`open` when it holds functions under computed keys, so may have more), `templates` the virtual and intrinsic elements and `frames` the named frames, each with its type, the templates it inherits, its mixins, its `values` (KeyValues), `children` (by parentKey) and `arrays` (by parentArray). `functions`, `tables` and `values` are the other globals Blizzard's Lua assigns", version.trim());
    let text = layout(&generated, &[
        ("elements", &elements, Style::Line), ("types", &types, Style::Line), ("mixins", &mixins, Style::Functions),
        ("templates", &template_values, Style::Line), ("frames", &frame_values, Style::Line),
        ("functions", &functions, Style::Line), ("tables", &tables, Style::Tree), ("values", &values, Style::Line),
    ]);
    std::fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    // Beside it: where each thing is, and which checkout that's in
    let sites_directory = out.parent().unwrap_or(Path::new(".")).join("sites");
    std::fs::create_dir_all(&sites_directory).map_err(|e| format!("{}: {e}", sites_directory.display()))?;
    // By file, a line per file, each with the line of each thing in it
    let count = sites.len();
    let mut by_file: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (name, site) in sites {
        let Some((file, line)) = site.rsplit_once(':') else { continue };
        let Ok(line) = line.parse::<u32>() else { continue };
        by_file.entry(file.to_string()).or_default().insert(name, json!(line));
    }
    let by_file: Map<String, Value> = by_file.into_iter().map(|(file, names)| (file, Value::Object(names))).collect();
    let sites_text = layout(&format!("by `wow_ls generate-templates` from wow-ui-source {}: the line each global, member, template and frame is on, by the file within it, for go to definition", version.trim()), &[("files", &by_file, Style::Line)]);
    let sites_path = sites_directory.join("blizzard.json");
    std::fs::write(&sites_path, sites_text).map_err(|e| format!("{}: {e}", sites_path.display()))?;
    let source_path = out.parent().unwrap_or(Path::new(".")).join(crate::definitions::SOURCE_PATH_FILE);
    std::fs::write(&source_path, source.display().to_string()).map_err(|e| format!("{}: {e}", source_path.display()))?;
    eprintln!("{count} sites");

    eprintln!("{} templates, {} frames, {} intrinsics, {} of {} mixins found", templates.len(), frames.len(), types.len(), mixins.len(), mixin_names.len());
    eprintln!("globals: {} functions, {} tables, {} values", functions.len(), tables.len(), values.len());
    report.unknown_globals.sort();
    report.unknown_globals.dedup();
    eprintln!("{} globals named by KeyValues have no known type: {:?}", report.unknown_globals.len(), report.unknown_globals);
    if !report.duplicates.is_empty() {
        eprintln!("{} templates defined more than once (the last loaded is kept)", report.duplicates.len());
    }
    eprintln!("{} mixins may have methods beyond those found, held under computed keys: {:?}", report.open_mixins.len(), report.open_mixins);
    if !report.missing_mixins.is_empty() {
        let names: Vec<&String> = report.missing_mixins.keys().collect();
        eprintln!("mixins with no table in the analysis: {names:?}");
    }
    Ok(())
}

/// Generates `global_strings.json` from a GlobalStrings table exported as CSV
/// (`ID,BaseTag,TagText_lang,Flags`): each `BaseTag` is a global string, as the
/// client defines them. Tags that aren't Lua names (`8.0_WARFRONTS_...`) can
/// only be reached through `_G[...]`, so they're left out
pub fn global_strings(csv: &Path, out: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(csv).map_err(|e| format!("{}: {e}", csv.display()))?;
    let mut rows = csv_rows(&text).into_iter();
    let header = rows.next().unwrap_or_default();
    let column = header.iter().position(|h| h == "BaseTag").ok_or("no BaseTag column")?;
    let mut values = Map::new();
    let mut skipped = 0;
    for row in rows {
        let Some(name) = row.get(column) else { continue };
        if is_name(name) {
            values.insert(name.clone(), json!("string"));
        } else {
            skipped += 1;
        }
    }
    let name = csv.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let generated = format!("by `wow_ls generate-global-strings` from {name}; regenerate rather than editing");
    std::fs::write(out, layout(&generated, &[("values", &values, Style::Line)])).map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!("{} global strings, {skipped} left out as they aren't Lua names", values.len());
    Ok(())
}

/// The client's global colors, from the GlobalColor table exported as CSV
/// (`ID,LuaConstantName,Color`, as wago.tools gives it, the color a signed
/// 32-bit ARGB number), as Lua making each with `CreateColorFromHexString`
/// from its hex digits, and its `_CODE` markup string:
/// a file loaded by `Blizzard_SharedXML` straight after `ColorUtil.lua`,
/// which defines that. The file's path (beside `ColorUtil.lua`, though it
/// isn't there), its text, and the names of the colors it makes
fn global_colors(csv: &Path, addons: &mut [project::Addon]) -> Result<(PathBuf, String, HashSet<String>), String> {
    let text = std::fs::read_to_string(csv).map_err(|e| format!("{}: {e}", csv.display()))?;
    let mut rows = csv_rows(&text).into_iter();
    let header = rows.next().unwrap_or_default();
    let column = |name: &str| header.iter().position(|h| h == name).ok_or(format!("{}: no {name} column", csv.display()));
    let (name_column, color_column) = (column("LuaConstantName")?, column("Color")?);
    let mut lua = String::new();
    let mut names = HashSet::new();
    for row in rows {
        let (Some(name), Some(color)) = (row.get(name_column), row.get(color_column)) else { continue };
        if !is_name(name) {
            continue
        }
        let color: i64 = color.trim().parse().map_err(|_| format!("{}: {name}'s color `{color}` isn't a number", csv.display()))?;
        let hex = format!("{:08x}", color as u32);
        lua += &format!("{name} = CreateColorFromHexString(\"{hex}\");\n");
        // And its markup, as the client makes it: `"|cffffd200"`
        lua += &format!("{name}_CODE = \"|c{hex}\";\n");
        names.insert(name.clone());
    }
    let shared = addons.iter_mut()
        .find(|addon| addon.files.iter().any(|f| f.ends_with("Blizzard_SharedXML/ColorUtil.lua")))
        .ok_or("no addon loads Blizzard_SharedXML/ColorUtil.lua")?;
    let after = shared.files.iter().position(|f| f.ends_with("Blizzard_SharedXML/ColorUtil.lua")).expect("found above");
    let path = shared.files[after].with_file_name("GlobalColor.lua");
    shared.files.insert(after + 1, path.clone());
    Ok((path, lua, names))
}

/// Whether `text` is a Lua name
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The rows of a CSV file: fields separated by commas, and quoted when they
/// hold commas, quotes (doubled) or line breaks
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => { chars.next(); field.push('"'); }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// How a section of the generated file is laid out
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// A line per entry
    Line,
    /// A line per entry, and per function of each (`mixins`)
    Functions,
    /// Each table's functions and values a line each, and its tables
    /// likewise, indented (`tables`)
    Tree,
}

fn json_string(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}

/// The file's text: a line per entry of each section, and for `mixins` and
/// `tables`, a line per function, so a regenerated file's changes are easy
/// to read
fn layout(generated: &str, sections: &[(&str, &Map<String, Value>, Style)]) -> String {
    let string = json_string;
    let mut text = format!("{{\n  \"generated\": {}", string(generated));
    for (name, entries, style) in sections {
        text += &format!(",\n  {}: {{", string(name));
        for (i, (key, value)) in entries.iter().enumerate() {
            let separator = if i == 0 { "" } else { "," };
            if *style == Style::Tree {
                text += &format!("{separator}\n    {}: {}", string(key), tree(value, 2));
                continue
            }
            let listed = |section: &str| value.get(section).and_then(Value::as_object).filter(|entries| !entries.is_empty());
            match (*style == Style::Functions, listed("functions").or(listed("values"))) {
                (true, Some(_)) => {
                    // Its other keys, like `"open": true`, first, then a line
                    // per function and per value
                    let others: String = value.as_object().into_iter().flatten()
                        .filter(|(k, _)| *k != "functions" && *k != "values")
                        .map(|(k, v)| format!("{}: {v}, ", string(k)))
                        .collect();
                    text += &format!("{separator}\n    {}: {{{others}", string(key));
                    let sections: Vec<String> = ["functions", "values"].iter()
                        .filter_map(|section| Some((section, value.get(*section)?.as_object()?)))
                        .map(|(section, entries)| {
                            let lines: Vec<String> = entries.iter()
                                .map(|(name, entry)| format!("\n        {}: {entry}", string(name)))
                                .collect();
                            format!("{}: {{{}\n    }}", string(section), lines.join(","))
                        })
                        .collect();
                    text += &sections.join(", ");
                    text += "}";
                }
                _ => text += &format!("{separator}\n    {}: {value}", string(key)),
            }
        }
        text += "\n  }";
    }
    text + "\n}\n"
}

/// A table as `tables` holds it, at `depth` levels of indent: a line for each
/// of its functions and values, and each of its own tables laid out the same
fn tree(table: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut parts = Vec::new();
    if let Some(open) = table.get("open") {
        parts.push(format!("\n{indent}  \"open\": {open}"));
    }
    for key in ["functions", "values", "tables"] {
        let Some(entries) = table.get(key).and_then(Value::as_object).filter(|e| !e.is_empty()) else { continue };
        let lines: Vec<String> = entries.iter()
            .map(|(name, value)| {
                let value = if key == "tables" { tree(value, depth + 2) } else { value.to_string() };
                format!("\n{indent}    {}: {value}", json_string(name))
            })
            .collect();
        parts.push(format!("\n{indent}  {}: {{{}\n{indent}  }}", json_string(key), lines.join(",")));
    }
    if parts.is_empty() {
        return "{}".to_string()
    }
    format!("{{{}\n{indent}}}", parts.join(","))
}

fn collect_mixins(object: &Object, names: &mut Vec<String>) {
    names.extend(object.mixins.iter().cloned());
    for child in &object.children {
        collect_mixins(child, names);
    }
}

/// A global, or a member of one, as the definitions give it
enum Member {
    Function(Value),
    /// Its `functions`, `tables` and `values`
    Table(Value),
    /// A type
    Value(String),
}

/// The names of the methods of Blizzard's Lua that give fields what they're
/// passed (`field_writes`), or call one on `self` that does, however
/// indirectly: what a method calling one on `self` (a `ForwardedCall`) can
/// do on each call, for an addon. Others are left out of the definitions,
/// which would otherwise list most of Blizzard's calls of methods on `self`
fn giving_methods(layer: &Layer) -> std::collections::HashSet<String> {
    // The names each function is stored under
    let mut names: HashMap<crate::types::FunctionId, Vec<&str>> = HashMap::new();
    for fields in layer.fields.values() {
        for (name, ty) in fields {
            if let FunctionRef::One(id) = ty.function {
                names.entry(id).or_default().push(name);
            }
        }
    }
    let mut giving: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let before = giving.len();
        for (id, info) in &layer.functions {
            let gives = !info.field_writes.is_empty() || info.forwarded_calls.iter()
                .any(|call| call.on.contains(&crate::types::On::SelfValue) && giving.contains(&call.method));
            if gives {
                giving.extend(names.get(id).into_iter().flatten().map(|name| name.to_string()));
            }
        }
        if giving.len() == before {
            return giving
        }
    }
}

struct Generator<'a> {
    kinds: &'a Kinds<'a>,
    /// What the analysis was over, whose types it gives: with Blizzard's
    /// templates and frames
    definitions: &'a Definitions,
    layer: &'a Layer,
    templates: &'a BTreeMap<String, Object>,
    /// wow-ui-source, which sites are given within
    source: &'a Path,
    /// Files' text, read once each, for the lines sites are on
    texts: std::cell::RefCell<HashMap<String, String>>,
    /// The names of methods that give fields (see `giving_methods`)
    giving_methods: std::collections::HashSet<String>,
}

impl Generator<'_> {
    /// Every mixin a frame made from template `name` gets, the one whose
    /// methods win first: its own (the last listed winning), then those of the
    /// templates it inherits (the last winning)
    fn template_mixins(&self, name: &str, mixins: &mut Vec<String>, depth: usize) {
        let Some(object) = self.templates.get(name) else { return };
        if depth > 32 {
            return
        }
        for mixin in object.mixins.iter().rev() {
            if !mixins.contains(mixin) {
                mixins.push(mixin.clone());
            }
        }
        for inherited in object.inherits.iter().rev() {
            self.template_mixins(inherited, mixins, depth + 1);
        }
    }

    /// The functions of the global table `name`, as its file and the mixins
    /// mixed into it define them, the fields that may hold one but may hold
    /// something else too (`unknown`, or a function or number), with their
    /// types, and whether it may have others: when it or one mixed into it
    /// holds functions under computed keys, which can be methods
    /// (`handler[accessor] = function(self) ... end`)
    fn mixin(&self, name: &str) -> Option<(Map<String, Value>, Map<String, Value>, bool)> {
        let table = self.layer.globals.get(name)?.named.identity()?;
        let mut fields = HashMap::new();
        let open = self.table_fields(table, &mut fields, 0);
        let mut functions: BTreeMap<String, Value> = BTreeMap::new();
        let mut values: BTreeMap<String, Value> = BTreeMap::new();
        for (field, ty) in fields {
            if !ty.set.contains(TypeSet::FUNCTION) {
                continue
            }
            // A field of a type that isn't known (`self.count = count`) isn't
            // a method, though it might hold one
            if ty.set != TypeSet::FUNCTION {
                values.insert(field, json!(self.type_text(ty)));
                continue
            }
            // One the analysis can't pin down, like a wrapped function
            // (`M.F = Wrap(M.F)`), still is a method, taking and giving anything
            let signature = match ty.function {
                FunctionRef::One(id) => self.layer.functions.get(&id).map(|info| self.signature(info)),
                _ => None,
            };
            functions.insert(field, signature.unwrap_or_else(|| json!({"parameters": ["..."], "returns": ["unknown"], "rest": "unknown"})));
        }
        Some((functions.into_iter().collect(), values.into_iter().collect(), open))
    }

    /// A table's fields: those of its mixins, in the order they were mixed
    /// in, and then its own, so later ones win as they do in Lua
    /// Also whether any of them holds functions under computed keys
    fn table_fields(&self, table: crate::types::TableId, fields: &mut HashMap<String, Type>, depth: usize) -> bool {
        if depth > 16 {
            return true
        }
        let mut open = self.layer.entries.get(&table).is_some_and(|e| e.keys.set.contains(TypeSet::STRING) && e.values.set.contains(TypeSet::FUNCTION));
        for mixin in self.layer.mixins.get(&table).into_iter().flatten() {
            if let Some(at) = mixin.named.identity() {
                open |= self.table_fields(at, fields, depth + 1);
            }
        }
        for (name, &ty) in self.layer.fields.get(&table).into_iter().flatten() {
            fields.insert(name.clone(), ty);
        }
        open
    }

    /// A global (or a field of one) of type `ty`: a function with its
    /// signature, a table made in Blizzard's Lua with its members, or a value
    /// of a type. `within` are the tables it's inside, which a table holding
    /// itself is just a `table` in
    fn member(&self, ty: Type, path: &str, sites: &mut BTreeMap<String, String>, within: &mut Vec<crate::types::TableId>) -> Member {
        let only = |set: TypeSet| ty.set.without(set) == TypeSet::NEVER && ty.set != TypeSet::NEVER;
        if only(TypeSet::FUNCTION) {
            let info = match ty.function {
                FunctionRef::One(id) => self.layer.functions.get(&id),
                FunctionRef::Builtin(id) => self.definitions.function(id),
                _ => None,
            };
            return Member::Function(info.map(|info| self.signature(info))
                .unwrap_or_else(|| json!({"parameters": ["..."], "returns": ["unknown"], "rest": "unknown"})))
        }
        let table = match ty.named {
            crate::types::NamedRef::Table(at) if only(TypeSet::TABLE) => at,
            _ => return Member::Value(self.type_text(ty)),
        };
        // Deep enough for any real namespace, and stops a loop
        if within.contains(&table) || within.len() > 8 {
            return Member::Value("table".to_string())
        }
        within.push(table);
        let mut fields = HashMap::new();
        let open = self.table_fields(table, &mut fields, 0);
        let mut names: Vec<&String> = fields.keys().filter(|name| is_name(name)).collect();
        names.sort();
        let (mut functions, mut tables, mut values) = (Map::new(), Map::new(), Map::new());
        for name in names {
            let member = format!("{path}.{name}");
            if let Some(site) = self.field_site(table, name, 0).and_then(|site| self.place(&site)) {
                sites.insert(member.clone(), site);
            }
            match self.member(fields[name], &member, sites, within) {
                Member::Function(signature) => { functions.insert(name.clone(), signature); }
                Member::Table(table) => { tables.insert(name.clone(), table); }
                Member::Value(ty) => { values.insert(name.clone(), json!(ty)); }
            }
        }
        within.pop();
        let mut members = Map::new();
        // Functions under computed keys may be more of its methods
        if open {
            members.insert("open".into(), json!(true));
        }
        for (key, entries) in [("functions", functions), ("tables", tables), ("values", values)] {
            if !entries.is_empty() {
                members.insert(key.into(), Value::Object(entries));
            }
        }
        Member::Table(Value::Object(members))
    }

    /// Where field `name` of a table Blizzard's Lua makes is set: on it, or
    /// on a mixin of it (the last first)
    fn field_site(&self, table: crate::types::TableId, name: &str, depth: usize) -> Option<crate::types::Site> {
        if let Some(site) = self.layer.field_sites.get(&table).and_then(|fields| fields.get(name)).and_then(|found| found.first()) {
            return Some(*site)
        }
        if depth > 16 {
            return None
        }
        self.layer.mixins.get(&table).into_iter().flatten().rev()
            .find_map(|mixin| self.field_site(mixin.named.identity()?, name, depth + 1))
    }

    /// A site as `path:line`, with the path within wow-ui-source
    fn place(&self, site: &crate::types::Site) -> Option<String> {
        let file = crate::types::layer_file_key(site.file)?;
        // Not one of wow-ui-source's, like the global colors' (see `global_colors`)
        if !Path::new(&file).exists() {
            return None
        }
        let relative = Path::new(&file).strip_prefix(self.source).ok()?.display().to_string();
        let mut texts = self.texts.borrow_mut();
        if !texts.contains_key(&file) {
            texts.insert(file.clone(), xml::read(Path::new(&file)).unwrap_or_default());
        }
        let text = &texts[&file];
        let offset = usize::from(site.range.start()).min(text.len());
        Some(format!("{relative}:{}", text[..offset].matches('\n').count() + 1))
    }

    fn signature(&self, info: &FunctionInfo) -> Value {
        let mut parameters: Vec<String> = info.parameters.iter().map(|p| p.name.clone()).collect();
        // `function M.F(self, x)`, called as a method like the rest
        let skipped = !info.method && parameters.first().is_some_and(|p| p == "self");
        if skipped {
            parameters.remove(0);
        }
        if info.varargs.is_some() {
            parameters.push("...".to_string());
        }
        let mut function = Map::new();
        if !parameters.is_empty() {
            function.insert("parameters".into(), json!(parameters));
        }
        let returns: Vec<String> = info.returns.values.iter().map(|&ty| self.type_text(ty)).collect();
        if !returns.is_empty() {
            function.insert("returns".into(), json!(returns));
        }
        if info.returns.rest.set != TypeSet::NIL && info.returns.rest.set != TypeSet::NEVER {
            function.insert("rest".into(), json!(self.type_text(info.returns.rest)));
        }
        // New plain tables each call, with mixins: parameters, by the
        // positions of the parameters written, and globals. Not a pool's,
        // whose objects aren't written
        let fresh: Vec<Value> = info.fresh_returns.iter().zip(&info.returns.values).enumerate()
            .map(|(slot, (fresh, &value))| {
                let Some(fresh) = fresh.as_ref().filter(|f| f.pool_mixed_parameters.is_none()) else { return Value::Null };
                let Some(made) = fresh.made.filter(|_| self.type_text(value) == "table") else { return Value::Null };
                let Some(positions) = fresh.mixed_parameters.iter().map(|&p| p.checked_sub(usize::from(skipped))).collect::<Option<Vec<usize>>>() else { return Value::Null };
                let mut mixins = Vec::new();
                // The fields each call gives it, which `field_writes` say
                let given: Vec<&str> = info.field_writes.iter()
                    .filter(|write| write.on == crate::types::On::Returned(slot))
                    .map(|write| write.field.as_str())
                    .collect();
                let mut fields = BTreeMap::new();
                if !self.global_mixins(made, &mut mixins, &mut fields, 0) {
                    return Value::Null
                }
                // The fields it has besides those each call gives it: given
                // after it's made (`column.width = w` where columns are looped
                // over), or by a mixin no name gives, so a call's has them too
                let mut values = Map::new();
                for (name, ty) in fields {
                    if !given.contains(&name.as_str()) {
                        values.insert(name, json!(self.type_text(ty)));
                    }
                }
                let mut entry = Map::new();
                if !positions.is_empty() {
                    entry.insert("parameters".into(), json!(positions));
                }
                if !mixins.is_empty() {
                    entry.insert("mixins".into(), json!(mixins));
                }
                if !values.is_empty() {
                    entry.insert("values".into(), Value::Object(values));
                }
                if entry.is_empty() { Value::Null } else { Value::Object(entry) }
            })
            .collect();
        if fresh.iter().any(|f| !f.is_null()) {
            function.insert("fresh_returns".into(), Value::Array(fresh));
        }
        // Methods it calls on what it's passed, by the positions of the
        // parameters written
        let position = |p: usize| p.checked_sub(usize::from(skipped));
        let argument = |argument: crate::types::Forwarded| match argument {
            crate::types::Forwarded::Parameter(p) => position(p).map(|p| json!(p)),
            crate::types::Forwarded::Truthy(p) => position(p).map(|p| json!({"truthy": p})),
            crate::types::Forwarded::Rest => Some(json!("...")),
            crate::types::Forwarded::Value(ty) => Some(json!(self.type_text(ty))),
        };
        // `self`, a parameter, or a return. `function M.F(self, x)`'s `self` is
        // written as a method's is
        let on = |on: crate::types::On| match on {
            crate::types::On::SelfValue => Some(json!("self")),
            crate::types::On::Parameter(0) if skipped => Some(json!("self")),
            crate::types::On::Parameter(p) => position(p).map(|p| json!(p)),
            crate::types::On::Returned(slot) => Some(json!({"returned": slot})),
        };
        let method_calls: Vec<Value> = info.forwarded_calls.iter()
            // On `self`, only of methods that give fields: nothing else they
            // do is any use to an addon
            .filter(|call| !call.on.contains(&crate::types::On::SelfValue) || self.giving_methods.contains(&call.method))
            .filter_map(|call| {
                let on: Vec<Value> = call.on.iter().map(|&o| on(o)).collect::<Option<_>>()?;
                let arguments: Vec<Value> = call.arguments.iter().map(|&a| argument(a)).collect::<Option<_>>()?;
                Some(json!({"on": on, "method": call.method, "arguments": arguments}))
            })
            .collect();
        if !method_calls.is_empty() {
            function.insert("method_calls".into(), Value::Array(method_calls));
        }
        // Fields it gives what it's passed
        let field_writes: Vec<Value> = info.field_writes.iter()
            .filter_map(|write| {
                let value: Vec<Value> = write.value.iter().map(|&a| argument(a)).collect::<Option<_>>()?;
                Some(json!({"on": on(write.on)?, "field": write.field, "value": value}))
            })
            .collect();
        if !field_writes.is_empty() {
            function.insert("field_writes".into(), Value::Array(field_writes));
        }
        // The fields of `self` or a parameter it returns
        let returned: Vec<Value> = info.returned_fields.iter()
            .map(|field| field.as_ref().and_then(|field| Some(json!({"on": on(field.on)?, "field": field.field}))).unwrap_or(Value::Null))
            .collect();
        if returned.iter().any(|r| !r.is_null()) {
            function.insert("returned_fields".into(), Value::Array(returned));
        }
        Value::Object(function)
    }

    /// The globals mixed into `table`, made in Blizzard's Lua, by name: its
    /// mixins that are global tables, and theirs, through tables it's made
    /// from (another function's new table at a call). Not the mixins that
    /// are the parameters of the function making a table, which are each
    /// call's own. The fields of `table`, and of mixins that are no global
    /// (any of several tables, or one made at a call), which a name can't
    /// give, go in `fields`. False if it has a mixin that's no table made in
    /// Blizzard's Lua, which can't be named: left out, its methods would be
    /// reported missing
    fn global_mixins(&self, table: crate::types::TableId, names: &mut Vec<String>, fields: &mut BTreeMap<String, Type>, depth: usize) -> bool {
        if depth > 8 {
            return false
        }
        for (name, &ty) in self.layer.fields.get(&table).into_iter().flatten() {
            *fields.entry(name.clone()).or_default() |= ty;
        }
        // The types of the parameters mixed in by the function that makes it
        let parameters: Vec<Type> = self.layer.functions.values()
            .flat_map(|info| info.fresh_returns.iter().flatten().filter(|f| f.made == Some(table)).flat_map(|f| f.mixed_parameters.iter().filter_map(|&p| info.parameters.get(p).map(|p| p.types))))
            .collect();
        for &mixin in self.layer.mixins.get(&table).into_iter().flatten() {
            if parameters.contains(&mixin) {
                continue
            }
            let tables = mixin.named.identities();
            if tables.is_empty() {
                return false
            }
            let global = match tables.as_slice() {
                [at] => self.layer.globals.iter()
                    .filter(|(name, ty)| ty.named.identity() == Some(*at) && is_name(name))
                    .map(|(name, _)| name)
                    .min(),
                _ => None,
            };
            match global {
                Some(name) => if !names.contains(name) { names.push(name.clone()) },
                None => for at in tables {
                    if !self.global_mixins(at, names, fields, depth + 1) {
                        return false
                    }
                },
            }
        }
        true
    }

    fn type_text(&self, ty: Type) -> String {
        self.definitions.written_type_text(ty)
    }

    /// The type of the global a `type="global"` KeyValue names, like
    /// `Enum.ItemQuality.Poor` or `BACKDROP_TOOLTIP_16_16_5555`
    fn global_type(&self, path: &str) -> Option<String> {
        if let Some(ty) = self.kinds.definitions.at_path(path) {
            return Some(self.type_text(ty))
        }
        let mut parts = path.split('.');
        let mut ty = *self.layer.globals.get(parts.next()?)?;
        for part in parts {
            ty = *self.layer.fields.get(&ty.named.identity()?)?.get(part)?;
        }
        Some(self.type_text(ty))
    }

    /// An object as the generated file gives it
    fn node(&self, object: &Object, report: &mut Report) -> Value {
        xml::node(object, &|tag: &str| self.kinds.of(tag), &mut |path: &str| {
            let found = self.global_type(path);
            if found.is_none() {
                report.unknown_globals.push(path.to_string());
            }
            found
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lays_out_a_line_per_entry() {
        let types = json!({"A": {"base": "table"}, "B": {"base": "table"}});
        let mixins = json!({"M": {"open": true, "functions": {"F": {}, "G": {"returns": ["number"]}}}, "Empty": {"functions": {}}});
        let text = layout("here", &[("types", types.as_object().unwrap(), Style::Line), ("mixins", mixins.as_object().unwrap(), Style::Functions)]);
        assert_eq!(text, "{\n  \"generated\": \"here\",\n  \"types\": {\n    \"A\": {\"base\":\"table\"},\n    \"B\": {\"base\":\"table\"}\n  },\n  \"mixins\": {\n    \"Empty\": {\"functions\":{}},\n    \"M\": {\"open\": true, \"functions\": {\n        \"F\": {},\n        \"G\": {\"returns\":[\"number\"]}\n    }}\n  }\n}\n");
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["mixins"], mixins);
        // Values a line each too
        let mixins = json!({"M": {"functions": {"F": {}}, "values": {"n": "unknown", "m": "number|function"}}});
        let text = layout("here", &[("mixins", mixins.as_object().unwrap(), Style::Functions)]);
        assert_eq!(text, "{\n  \"generated\": \"here\",\n  \"mixins\": {\n    \"M\": {\"functions\": {\n        \"F\": {}\n    }, \"values\": {\n        \"m\": \"number|function\",\n        \"n\": \"unknown\"\n    }}\n  }\n}\n");
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["mixins"], mixins);
        // Tables a line per member, nested ones indented
        let tables = json!({"T": {"functions": {"F": {}}, "values": {"v": "number"}, "tables": {"Inner": {"functions": {"G": {}}}, "Empty": {}}}, "O": {"open": true}});
        let text = layout("here", &[("tables", tables.as_object().unwrap(), Style::Tree)]);
        assert_eq!(text, "{\n  \"generated\": \"here\",\n  \"tables\": {\n    \"O\": {\n      \"open\": true\n    },\n    \"T\": {\n      \"functions\": {\n        \"F\": {}\n      },\n      \"values\": {\n        \"v\": \"number\"\n      },\n      \"tables\": {\n        \"Empty\": {},\n        \"Inner\": {\n          \"functions\": {\n            \"G\": {}\n          }\n        }\n      }\n    }\n  }\n}\n");
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["tables"], tables);
    }

    #[test]
    fn reads_csv() {
        let text = "ID,BaseTag,TagText_lang,Flags\r\n1,ACCEPT,\"Accept\",1\n2,QUOTE,\"Say \"\"hi\"\", then\nleave\",3\n3,LAST,x,1";
        assert_eq!(csv_rows(text), [
            vec!["ID", "BaseTag", "TagText_lang", "Flags"],
            vec!["1", "ACCEPT", "Accept", "1"],
            vec!["2", "QUOTE", "Say \"hi\", then\nleave", "3"],
            vec!["3", "LAST", "x", "1"],
        ]);
        assert!(is_name("ACCEPT") && is_name("_x1"));
        assert!(!is_name("8.0_WARFRONTS") && !is_name("90POA_BOSS_01") && !is_name(""));
    }
}
