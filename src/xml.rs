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

//! WoW's UI XML files: what they load, and the frames they define.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

/// A file an XML file loads, relative to the XML file's folder
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    /// `<Script file="..."/>`
    Lua(PathBuf),
    /// `<Include file="..."/>`
    Xml(PathBuf),
}

/// Parses an XML file's text. A byte order mark and anything before the first
/// `<` are skipped, as the game does, so positions in the document are from
/// `start(text)`
pub fn parse(text: &str) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    roxmltree::Document::parse_with_options(&text[start(text)..], options)
}

/// Where in `text` the document `parse` reads starts
pub fn start(text: &str) -> usize {
    text.find('<').unwrap_or(text.len())
}

/// Where in `text` (the whole file) a parse error is, as a byte offset
pub fn error_offset(text: &str, error: &roxmltree::Error) -> usize {
    let start = start(text);
    let position = error.pos();
    let document = &text[start..];
    let mut offset = 0;
    for (row, line) in document.split_inclusive('\n').enumerate() {
        if row + 1 == position.row as usize {
            let column = line.char_indices().nth(position.col.saturating_sub(1) as usize).map_or(line.len(), |(at, _)| at);
            return start + offset + column
        }
        offset += line.len();
    }
    text.len()
}

/// The text of a file that may not be UTF-8, as WoW's files sometimes aren't
pub fn read(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    })
}

/// The files `document` loads, in order
pub fn loads(document: &roxmltree::Document) -> Vec<Load> {
    document.descendants()
        .filter(|node| node.is_element())
        .filter_map(|node| {
            let path = PathBuf::from(node.attribute("file")?.trim().replace('\\', "/"));
            match node.tag_name().name() {
                "Script" => Some(Load::Lua(path)),
                "Include" => Some(Load::Xml(path)),
                _ => None,
            }
        })
        .collect()
}

/// What UI.xsd says about each element: the schema type of each, the group it
/// substitutes for (which says what kind of thing it is: `ui:FrameRef` for a
/// frame, `ButtonField` for a part of a button), and the type each schema type
/// extends
#[derive(Debug, Default)]
pub struct Schema {
    elements: HashMap<String, (String, String)>,
    extends: HashMap<String, String>,
}

/// An element's schema name, without its `ui:` namespace
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

impl Schema {
    pub fn parse(document: &roxmltree::Document) -> Schema {
        let mut schema = Schema::default();
        let xs = |node: &roxmltree::Node, name: &str| node.is_element() && node.tag_name().name() == name;
        for node in document.root_element().children() {
            if xs(&node, "element") {
                if let (Some(name), Some(ty)) = (node.attribute("name"), node.attribute("type")) {
                    let group = local_name(node.attribute("substitutionGroup").unwrap_or("")).to_string();
                    schema.elements.insert(name.to_string(), (local_name(ty).to_string(), group));
                }
            }
            if xs(&node, "complexType") {
                let (Some(name), Some(base)) = (
                    node.attribute("name"),
                    node.descendants().find(|n| xs(n, "extension")).and_then(|n| n.attribute("base")),
                ) else { continue };
                schema.extends.insert(name.to_string(), local_name(base).to_string());
            }
        }
        schema
    }

    /// Every element's name
    pub fn tags(&self) -> impl Iterator<Item = &str> {
        self.elements.keys().map(String::as_str)
    }

    /// The type an element `tag` makes, among `known` types: its schema
    /// type's name without `Type`, or failing that the first type it extends
    /// that's known. The second value is whether it may have methods beyond
    /// that type's: when the element is a kind of object of its own (a frame,
    /// region or animation) that isn't a known type
    pub fn object_type(&self, tag: &str, known: impl Fn(&str) -> bool) -> Option<(String, bool)> {
        let (ty, group) = self.elements.get(tag)?;
        let mut ty = ty.as_str();
        for _ in 0..16 {
            let name = ty.strip_suffix("Type").unwrap_or(ty);
            if known(name) {
                let own_kind = group.ends_with("Ref") && name != tag;
                return Some((name.to_string(), own_kind))
            }
            ty = self.extends.get(ty)?;
        }
        None
    }
}

/// An element that makes an object: a frame, texture, font string, animation
/// and so on, or a template for one
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Object {
    pub tag: String,
    pub name: Option<String>,
    pub is_virtual: bool,
    pub intrinsic: bool,
    pub inherits: Vec<String>,
    /// `mixin`, `secureMixin`, then each `<Mixin key="..."/>` in `<Mixins>`, in order
    pub mixins: Vec<String>,
    pub parent_key: Option<String>,
    pub parent_array: Option<String>,
    /// `parent="..."`, the frame it's made in when it isn't inside one
    pub parent: Option<String>,
    pub key_values: Vec<KeyValue>,
    /// The objects inside it, however deeply its other elements hold them
    pub children: Vec<Object>,
    /// Where the element is in its file
    pub range: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    /// As written; the schema's default is `string`
    pub kind: Option<String>,
}

/// A comma separated list, as in `inherits="A, B"`
fn list(text: Option<&str>) -> Vec<String> {
    text.into_iter().flat_map(|t| t.split(',')).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// The objects at the top of `document`, each with those inside it. Which
/// elements make objects is up to `is_object`, by tag; the elements between
/// them (`Frames`, `Layers`, `Anchors`, ...) are looked through
pub fn objects(document: &roxmltree::Document, is_object: &impl Fn(&str) -> bool) -> Vec<Object> {
    let mut found = Vec::new();
    collect_objects(document.root_element(), is_object, &mut found);
    found
}

fn collect_objects(node: roxmltree::Node, is_object: &impl Fn(&str) -> bool, found: &mut Vec<Object>) {
    for child in node.children().filter(|n| n.is_element()) {
        if is_object(child.tag_name().name()) {
            found.push(object(child, is_object));
        } else {
            collect_objects(child, is_object, found);
        }
    }
}

fn object(node: roxmltree::Node, is_object: &impl Fn(&str) -> bool) -> Object {
    let attribute = |name: &str| node.attribute(name).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let flag = |name: &str| node.attribute(name).is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let mut mixins = list(node.attribute("mixin"));
    mixins.extend(list(node.attribute("secureMixin")));
    // `<Mixins><Mixin key="..."/></Mixins>`
    mixins.extend(node.children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Mixins")
        .flat_map(|n| n.children())
        .filter(|n| n.is_element() && n.tag_name().name() == "Mixin")
        .filter_map(|n| Some(n.attribute("key")?.trim().to_string())));
    let key_values = node.children()
        .filter(|n| n.is_element() && n.tag_name().name() == "KeyValues")
        .flat_map(|n| n.children())
        .filter(|n| n.is_element() && n.tag_name().name() == "KeyValue")
        .filter_map(|n| Some(KeyValue {
            key: n.attribute("key")?.to_string(),
            value: n.attribute("value").unwrap_or("").to_string(),
            kind: n.attribute("type").map(str::to_string),
        }))
        .collect();
    let mut children = Vec::new();
    collect_objects(node, is_object, &mut children);
    Object {
        tag: node.tag_name().name().to_string(),
        name: attribute("name"),
        is_virtual: flag("virtual"),
        intrinsic: flag("intrinsic"),
        inherits: list(node.attribute("inherits")),
        mixins,
        parent_key: attribute("parentKey"),
        parent_array: attribute("parentArray"),
        parent: attribute("parent"),
        key_values,
        children,
        range: node.range(),
    }
}

/// An object as definitions describe templates and frames: its `type` (and
/// `extended` when it may have methods beyond it), the templates it
/// `inherits`, its `mixins`, its `values` (KeyValues), `children` (by
/// parentKey) and `arrays` (by parentArray). `kind` gives the type each tag
/// makes; `global` the type of the global a `type="global"` KeyValue names
pub fn node(object: &Object, kind: &impl Fn(&str) -> Option<(String, bool)>, global: &mut impl FnMut(&str) -> Option<String>) -> Value {
    let mut node = Map::new();
    if let Some((ty, extended)) = kind(&object.tag) {
        node.insert("type".into(), json!(ty));
        if extended {
            node.insert("extended".into(), json!(true));
        }
    }
    if !object.inherits.is_empty() {
        node.insert("inherits".into(), json!(object.inherits));
    }
    if !object.mixins.is_empty() {
        node.insert("mixins".into(), json!(object.mixins));
    }
    if let Some(parent) = &object.parent {
        node.insert("parent".into(), json!(parent));
    }
    let mut values = Map::new();
    for key_value in &object.key_values {
        let ty = match key_value.kind.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("string") => "string".to_string(),
            Some("boolean") => "boolean".to_string(),
            Some("number") => "number".to_string(),
            Some("function") => "function".to_string(),
            Some("global") => global(key_value.value.trim()).unwrap_or_else(|| "unknown".to_string()),
            Some(_) => "unknown".to_string(),
        };
        values.insert(key_value.key.clone(), json!(ty));
    }
    if !values.is_empty() {
        node.insert("values".into(), Value::Object(values));
    }
    let mut children = Map::new();
    let mut arrays: Map<String, Value> = Map::new();
    for child in &object.children {
        if let Some(key) = &child.parent_key {
            children.insert(key.clone(), node_of(child, kind, global));
        }
        if let Some(key) = &child.parent_array {
            let child = node_of(child, kind, global);
            match arrays.get_mut(key) {
                Some(Value::Array(items)) => items.push(child),
                _ => { arrays.insert(key.clone(), json!([child])); }
            }
        }
    }
    if !children.is_empty() {
        node.insert("children".into(), Value::Object(children));
    }
    if !arrays.is_empty() {
        node.insert("arrays".into(), Value::Object(arrays));
    }
    Value::Object(node)
}

// `node`, through a reference to its closures, so recursing doesn't nest their types
fn node_of(object: &Object, kind: &dyn Fn(&str) -> Option<(String, bool)>, global: &mut dyn FnMut(&str) -> Option<String>) -> Value {
    node(object, &kind, &mut |path: &str| global(path))
}

/// Where in `text` (an XML file, called `file` in the sites) each template
/// and named frame `objects` define is, and each object inside them with a
/// `parentKey`: `template:T`, `template:T.Key`, `Frame`, `Frame.Key`, each
/// as `file:line`
pub fn sites(objects: &[Object], file: &str, text: &str) -> Vec<(String, String)> {
    let start = start(text);
    let line = |object: &Object| text[..(start + object.range.start).min(text.len())].matches('\n').count() + 1;
    fn children(object: &Object, path: &str, line: &dyn Fn(&Object) -> usize, file: &str, sites: &mut Vec<(String, String)>) {
        for child in &object.children {
            let Some(key) = &child.parent_key else { continue };
            let path = format!("{path}.{key}");
            sites.push((path.clone(), format!("{file}:{}", line(child))));
            children(child, &path, line, file, sites);
        }
    }
    let mut sites = Vec::new();
    for object in objects {
        if object.is_virtual || object.intrinsic {
            let Some(name) = &object.name else { continue };
            let path = format!("template:{name}");
            sites.push((path.clone(), format!("{file}:{}", line(object))));
            children(object, &path, &line, file, &mut sites);
            continue
        }
        let mut frames = Vec::new();
        global_frames(object, None, &mut frames);
        for (name, frame) in &frames {
            sites.push((name.clone(), format!("{file}:{}", line(frame))));
            children(frame, name, &line, file, &mut sites);
        }
    }
    sites
}

/// Each named frame among `object` and those inside it, by its name with
/// `$parent` replaced by the name of the nearest named object holding it
pub fn global_frames(object: &Object, parent: Option<&str>, frames: &mut Vec<(String, Object)>) {
    let name = object.name.as_deref().and_then(|name| resolve_parent(name, parent));
    if let Some(name) = &name {
        frames.retain(|(n, _)| n != name);
        frames.push((name.clone(), object.clone()));
    }
    for child in &object.children {
        global_frames(child, name.as_deref().or(parent), frames);
    }
}

/// `name` with `$parent` replaced by the parent's name, if it has one
fn resolve_parent(name: &str, parent: Option<&str>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let Some(at) = lower.find("$parent") else { return Some(name.to_string()) };
    let rest = resolve_parent(&name[at + "$parent".len()..], parent)?;
    Some(format!("{}{}{rest}", &name[..at], parent?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_a_file_loads() {
        let text = "\u{feff} <Ui xmlns=\"http://www.blizzard.com/wow/ui/\">\n<Script file=\"A.lua\"/>\n<Include file=\"Sub\\B.xml\"/>\n<Frame name=\"F\"><Scripts><OnLoad>print(1)</OnLoad></Scripts></Frame>\n<Script file=\"C.lua\"/>\n<Script>print(2)</Script></Ui>";
        let document = parse(text).unwrap();
        assert_eq!(loads(&document), [
            Load::Lua("A.lua".into()),
            Load::Xml("Sub/B.xml".into()),
            Load::Lua("C.lua".into()),
        ]);
    }

    #[test]
    fn reads_the_schema() {
        let xsd = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:ui="http://www.blizzard.com/wow/ui/">
            <xs:element name="Frame" type="FrameType" substitutionGroup="ui:FrameRef"/>
            <xs:element name="Button" type="ButtonType" substitutionGroup="ui:FrameRef"/>
            <xs:element name="ModelFFX" type="ModelType" substitutionGroup="ui:FrameRef"/>
            <xs:element name="FogOfWarFrame" type="FogOfWarFrameType" substitutionGroup="ui:FrameRef"/>
            <xs:element name="NormalTexture" type="ui:TextureType" substitutionGroup="ButtonField"/>
            <xs:element name="Size" type="Dimension" substitutionGroup="LayoutField"/>
            <xs:complexType name="ButtonType"><xs:complexContent><xs:extension base="ui:FrameType"/></xs:complexContent></xs:complexType>
            <xs:complexType name="FogOfWarFrameType"><xs:complexContent><xs:extension base="FrameType"/></xs:complexContent></xs:complexType>
        </xs:schema>"#;
        let schema = Schema::parse(&parse(xsd).unwrap());
        let known = |name: &str| ["Frame", "Button", "Model", "Texture"].contains(&name);
        assert_eq!(schema.object_type("Button", known), Some(("Button".into(), false)));
        // A kind of frame of its own that isn't known: may have more methods
        assert_eq!(schema.object_type("ModelFFX", known), Some(("Model".into(), true)));
        assert_eq!(schema.object_type("FogOfWarFrame", known), Some(("Frame".into(), true)));
        // A part of a button, which is a plain texture
        assert_eq!(schema.object_type("NormalTexture", known), Some(("Texture".into(), false)));
        assert_eq!(schema.object_type("Size", known), None);
        assert_eq!(schema.object_type("Unknown", known), None);
    }

    #[test]
    fn reads_objects() {
        let text = r#"<Ui>
            <Button name="T" virtual="true" inherits="A, B" mixin="M1, M2" secureMixin="S">
                <Mixins><Mixin key="E" source="secure"/></Mixins>
                <KeyValues><KeyValue key="flag" value="true" type="boolean"/><KeyValue key="label" value="x"/></KeyValues>
                <Layers><Layer level="ARTWORK">
                    <Texture parentKey="Icon"><Anchors><Anchor point="CENTER"/></Anchors></Texture>
                </Layer></Layers>
                <Frames><Frame parentArray="Tabs"/><Frame parentArray="Tabs"/></Frames>
            </Button>
            <Frame name="Intrinsic" intrinsic="true"/>
        </Ui>"#;
        let document = parse(text).unwrap();
        let objects = objects(&document, &|tag: &str| ["Button", "Frame", "Texture"].contains(&tag));
        assert_eq!(objects.len(), 2);
        let button = &objects[0];
        assert_eq!((button.tag.as_str(), button.name.as_deref(), button.is_virtual), ("Button", Some("T"), true));
        assert_eq!(button.inherits, ["A", "B"]);
        assert_eq!(button.mixins, ["M1", "M2", "S", "E"]);
        assert_eq!(button.key_values, [
            KeyValue { key: "flag".into(), value: "true".into(), kind: Some("boolean".into()) },
            KeyValue { key: "label".into(), value: "x".into(), kind: None },
        ]);
        let keys: Vec<_> = button.children.iter().map(|c| (c.tag.as_str(), c.parent_key.as_deref(), c.parent_array.as_deref())).collect();
        assert_eq!(keys, [("Texture", Some("Icon"), None), ("Frame", None, Some("Tabs")), ("Frame", None, Some("Tabs"))]);
        assert!(objects[1].intrinsic);
        assert!(text[objects[1].range.clone()].starts_with("<Frame name=\"Intrinsic\""));
    }

    #[test]
    fn reports_errors() {
        let text = "\u{feff}\n<Ui>\n  <Frame></Ui>";
        let error = parse(text).unwrap_err();
        // At `</Ui>`, counting from the start of the file
        assert_eq!(&text[error_offset(text, &error)..], "</Ui>");
    }

    #[test]
    fn sites_of_templates_and_frames() {
        let text = "\n<Ui>\n<Frame name=\"T\" virtual=\"true\">\n  <Frames><Button parentKey=\"Close\"/></Frames>\n</Frame>\n<Frame name=\"Named\">\n  <Frames><Frame name=\"$parentInner\" parentKey=\"Inner\"/></Frames>\n</Frame>\n</Ui>";
        let document = parse(text).unwrap();
        let objects = objects(&document, &|tag: &str| ["Frame", "Button"].contains(&tag));
        let site = |key: &str, line: u32| (key.to_string(), format!("F.xml:{line}"));
        assert_eq!(sites(&objects, "F.xml", text), [
            site("template:T", 3), site("template:T.Close", 4),
            // A named frame, its child by key, and by its own name
            site("Named", 6), site("Named.Inner", 7), site("NamedInner", 7),
        ]);
    }

    #[test]
    fn resolves_parent_names() {
        let child = Object { name: Some("$parentClose".into()), ..Default::default() };
        let inner = Object { name: Some("$parent_Inner".into()), ..Default::default() };
        let unnamed = Object { children: vec![inner], ..Default::default() };
        let top = Object { name: Some("Top".into()), children: vec![child, unnamed], ..Default::default() };
        let mut frames = Vec::new();
        global_frames(&top, None, &mut frames);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Top", "TopClose", "Top_Inner"]);
        assert_eq!(resolve_parent("$ParentIcon", Some("Bag1")), Some("Bag1Icon".to_string()));
        // No parent to name it after
        assert_eq!(resolve_parent("$parentX", None), None);
    }
}
