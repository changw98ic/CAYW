use crate::{
    budget::{Budget, Result, Sink},
    error,
};
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};
use serde_json::{Map, Value, json};

struct Element {
    name: String,
    attributes: Map<String, Value>,
    children: Vec<Value>,
}

impl Element {
    fn finish(self) -> Value {
        json!({"name":self.name,"attributes":self.attributes,"children":self.children})
    }
}

pub(crate) fn parse(text: &str, budget: &mut Budget<'_>) -> Result<Value> {
    valid_chars(text)?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().enable_all_checks(true);
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    loop {
        budget.checkpoint()?;
        let event = reader
            .read_event()
            .map_err(|e| error("syntax", e.to_string()))?;
        match event {
            Event::Start(start) => {
                if stack.is_empty() && root.is_some() {
                    return Err(error("syntax", "XML has multiple root elements"));
                }
                let depth = stack.len() * 2 + 1;
                stack.push(element(start, budget, depth)?);
            }
            Event::Empty(start) => {
                let depth = stack.len() * 2 + 1;
                let value = element(start, budget, depth)?.finish();
                append_element(value, &mut stack, &mut root, budget)?;
            }
            Event::End(_) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| error("syntax", "unmatched XML end tag"))?;
                append_element(node.finish(), &mut stack, &mut root, budget)?;
            }
            Event::Text(text) => {
                let content = text.xml_content(XmlVersion::Implicit1_0);
                if content.contains("]]>") {
                    return Err(error("syntax", "XML text cannot contain ]]>"));
                }
                append_text(&content, &mut stack, budget)?;
            }
            Event::CData(text) => {
                if stack.is_empty() {
                    return Err(error("syntax", "CDATA must be inside the root"));
                }
                append_text(
                    &text.xml_content(XmlVersion::Implicit1_0),
                    &mut stack,
                    budget,
                )?;
            }
            Event::GeneralRef(reference) => {
                let content = if let Some(ch) = reference
                    .resolve_char_ref()
                    .map_err(|e| error("syntax", e.to_string()))?
                {
                    ch.to_string()
                } else {
                    quick_xml::escape::resolve_xml_entity(reference.as_ref())
                        .ok_or_else(|| error("syntax", "unknown XML entity"))?
                        .to_owned()
                };
                valid_chars(&content)?;
                if stack.is_empty() {
                    return Err(error("syntax", "XML references must be inside root"));
                }
                append_text(&content, &mut stack, budget)?;
            }
            Event::DocType(_) => {
                return Err(error(
                    "unsupported",
                    "XML DTD and external entities are forbidden",
                ));
            }
            Event::Comment(_) | Event::PI(_) | Event::Decl(_) => {
                return Err(error(
                    "unsupported",
                    "XML declarations, comments and processing instructions are not in the node model",
                ));
            }
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(error("syntax", "unclosed XML element"));
    }
    root.ok_or_else(|| error("syntax", "XML requires one root element"))
}

fn element(start: BytesStart<'_>, budget: &mut Budget<'_>, depth: usize) -> Result<Element> {
    budget.node(depth)?;
    for _ in 0..6 {
        budget.node(depth + 1)?;
    }
    let name = start.name().as_ref().to_owned();
    valid_name(&name)?;
    attribute_syntax(start.as_ref(), name.len())?;
    let mut attributes = Map::new();
    for attr in start.attributes() {
        budget.node(depth + 2)?;
        budget.node(depth + 2)?;
        let attr = attr.map_err(|e| error("syntax", e.to_string()))?;
        let key = attr.key.as_ref();
        valid_name(key)?;
        if attr.value.contains('<') {
            return Err(error("syntax", "unescaped < in XML attribute"));
        }
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| error("syntax", e.to_string()))?
            .into_owned();
        valid_chars(&value)?;
        if attributes
            .insert(key.to_owned(), Value::String(value))
            .is_some()
        {
            return Err(error("syntax", "duplicate XML attribute"));
        }
    }
    Ok(Element {
        name,
        attributes,
        children: Vec::new(),
    })
}

fn attribute_syntax(raw: &str, name_len: usize) -> Result<()> {
    let bytes = raw.as_bytes();
    let mut i = name_len;
    while i < bytes.len() {
        let start = i;
        while bytes
            .get(i)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            i += 1;
        }
        if i == bytes.len() {
            return Ok(());
        }
        if i == start {
            return Err(error(
                "syntax",
                "XML attributes must be separated by whitespace",
            ));
        }
        while bytes
            .get(i)
            .is_some_and(|b| !matches!(b, b'=' | b' ' | b'\t' | b'\r' | b'\n'))
        {
            i += 1;
        }
        while bytes
            .get(i)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            return Err(error("syntax", "XML attribute requires ="));
        }
        i += 1;
        while bytes
            .get(i)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            i += 1;
        }
        let quote = bytes
            .get(i)
            .copied()
            .filter(|b| matches!(b, b'\'' | b'"'))
            .ok_or_else(|| error("syntax", "XML attribute requires quotes"))?;
        i += 1;
        while bytes.get(i).is_some_and(|b| *b != quote) {
            i += 1;
        }
        if i == bytes.len() {
            return Err(error("syntax", "unclosed XML attribute"));
        }
        i += 1;
    }
    Ok(())
}

fn append_element(
    value: Value,
    stack: &mut [Element],
    root: &mut Option<Value>,
    budget: &Budget<'_>,
) -> Result<()> {
    if let Some(parent) = stack.last_mut() {
        budget.rows(parent.children.len() + 1)?;
        parent.children.push(value);
    } else if root.replace(value).is_some() {
        return Err(error("syntax", "XML has multiple root elements"));
    }
    Ok(())
}

fn append_text(text: &str, stack: &mut [Element], budget: &mut Budget<'_>) -> Result<()> {
    valid_chars(text)?;
    let depth = stack.len() * 2 + 1;
    if let Some(parent) = stack.last_mut() {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(Value::String(previous)) =
            parent.children.last_mut().and_then(|v| v.get_mut("text"))
        {
            previous.push_str(text);
        } else {
            budget.node(depth)?;
            budget.node(depth + 1)?;
            budget.node(depth + 1)?;
            budget.rows(parent.children.len() + 1)?;
            parent.children.push(json!({"text":text}));
        }
    } else if !text
        .chars()
        .all(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
    {
        return Err(error("syntax", "text outside XML root"));
    }
    Ok(())
}

fn name_start(ch: char) -> bool {
    matches!(ch, ':' | '_' | 'A'..='Z' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}
fn valid_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    if !chars.next().is_some_and(name_start) || !chars.all(|ch| name_start(ch) || matches!(ch, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')) {
        return Err(error("syntax", "invalid XML 1.0 name"));
    }
    Ok(())
}
fn valid_chars(text: &str) -> Result<()> {
    if text.chars().any(|ch| !matches!(ch, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')) {
        return Err(error("syntax", "invalid XML 1.0 character"));
    }
    Ok(())
}

pub(crate) fn serialize(value: &Value, budget: &Budget<'_>) -> Result<String> {
    let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    write(value, &mut sink, budget)?;
    sink.finish()
}

fn write(value: &Value, sink: &mut Sink<'_>, budget: &Budget<'_>) -> Result<()> {
    budget.checkpoint()?;
    let object = value
        .as_object()
        .ok_or_else(|| error("arguments", "XML element must be an object"))?;
    if object.len() != 3
        || !object.contains_key("name")
        || !object.contains_key("attributes")
        || !object.contains_key("children")
    {
        return Err(error(
            "arguments",
            "XML elements require exactly name, attributes and children",
        ));
    }
    let name = object["name"]
        .as_str()
        .ok_or_else(|| error("arguments", "XML name must be string"))?;
    valid_name(name)?;
    let attributes = object["attributes"]
        .as_object()
        .ok_or_else(|| error("arguments", "XML attributes must be an object of strings"))?;
    let children = object["children"]
        .as_array()
        .ok_or_else(|| error("arguments", "XML children must be an ordered array"))?;
    sink.push("<")?;
    sink.push(name)?;
    for (key, value) in attributes {
        valid_name(key)?;
        let value = value
            .as_str()
            .ok_or_else(|| error("arguments", "XML attribute values must be strings"))?;
        sink.push(" ")?;
        sink.push(key)?;
        sink.push("=\"")?;
        escape(value, sink, true)?;
        sink.push("\"")?;
    }
    if children.is_empty() {
        return sink.push("/>");
    }
    sink.push(">")?;
    for child in children {
        if let Some(text) = child
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get("text"))
        {
            let text = text
                .as_str()
                .ok_or_else(|| error("arguments", "XML text must be a string"))?;
            escape(text, sink, false)?;
        } else {
            write(child, sink, budget)?;
        }
    }
    sink.push("</")?;
    sink.push(name)?;
    sink.push(">")
}

fn escape(text: &str, sink: &mut Sink<'_>, attribute: bool) -> Result<()> {
    valid_chars(text)?;
    // Character references keep CR and attribute whitespace from normalizing on parse.
    for ch in text.chars() {
        match ch {
            '&' => sink.push("&amp;")?,
            '<' => sink.push("&lt;")?,
            '>' => sink.push("&gt;")?,
            '"' if attribute => sink.push("&quot;")?,
            '\r' => sink.push("&#13;")?,
            '\n' if attribute => sink.push("&#10;")?,
            '\t' if attribute => sink.push("&#9;")?,
            ch => {
                sink.push(ch.encode_utf8(&mut [0; 4]))?;
            }
        }
    }
    Ok(())
}
