//! YAML documents: parsed from `saphyr-parser`'s events, which say where
//! each value is, into a tree for each document of the stream, which
//! Flareite's schemas check. Scalars resolve by YAML 1.2's core schema
//! (`yes` is a string, `0o17` an integer), aliases are expanded, and `<<`
//! merge keys are merged, as tools that read YAML do.

use std::collections::HashMap;

use flareite_extension_schemas::document::{self, Node, NodeKind, Problem, Scalar, Severity, Span};
use flareite_extension_schemas::widen;
use saphyr_parser::{Event, Parser, ScalarStyle, Tag};
use serde_json::{Map, Number, Value as Json};

/// A syntax error.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxError {
  pub span: Span,
  pub message: String,
}

/// Parses a YAML stream for Flareite's schemas: a part for each document,
/// covering the text up to the next.
pub fn parse(text: &str) -> document::Document {
  let stream = parse_stream(text);
  let ranges = document_ranges(text);
  let parts = stream
    .documents
    .into_iter()
    .map(|part| {
      let span = ranges
        .iter()
        .find(|range| range.contains(&part.span.start) || range.end == part.span.start)
        .cloned()
        .unwrap_or(part.span);
      document::Part {
        tree: part.tree,
        span,
      }
    })
    .collect();
  let mut problems: Vec<Problem> = stream
    .errors
    .into_iter()
    .map(|error| Problem {
      span: error.span,
      severity: Severity::Error,
      message: error.message,
    })
    .collect();
  problems.extend(stream.duplicates.into_iter().map(|span| Problem {
    span,
    severity: Severity::Error,
    message: "Duplicate key: a mapping's keys must be different.".to_owned(),
  }));
  problems.sort_by_key(|problem| problem.span.start);
  document::Document {
    parts,
    named_schema: named_schema(text),
    problems,
  }
}

/// A parsed YAML stream.
pub struct YamlDocument {
  /// Its documents, in order. A stream of only comments has none.
  pub documents: Vec<Part>,
  pub errors: Vec<SyntaxError>,
  /// Keys given twice in a mapping, which YAML doesn't allow.
  pub duplicates: Vec<Span>,
}

/// A document in a stream.
pub struct Part {
  /// From its `---`, or its first value, to the end of its last value.
  pub span: Span,
  pub tree: Node,
  /// `None` when the stream has syntax errors.
  pub value: Option<Json>,
}

/// Most values aliases may expand to, in all: aliases of aliases grow
/// exponentially ("billion laughs").
const MAX_EXPANDED: usize = 100_000;

/// Most nested mappings and sequences.
const MAX_DEPTH: usize = 512;

/// Parses a YAML stream. After a syntax error, the documents hold what
/// was read before it.
pub fn parse_stream(text: &str) -> YamlDocument {
  let mut builder = Builder {
    text,
    offsets: Offsets::new(text),
    frames: Vec::new(),
    anchors: HashMap::new(),
    expanded: 0,
    duplicates: Vec::new(),
    errors: Vec::new(),
    documents: Vec::new(),
    document_start: 0,
    root: None,
  };
  for event in Parser::new_from_str(text) {
    let (event, span) = match event {
      Ok(event) => event,
      Err(error) => {
        let at = builder.offsets.at(error.marker().index());
        builder.error(at..at, error.info());
        break;
      }
    };
    let start = builder.offsets.at(span.start.index());
    let end = builder.offsets.at(span.end.index()).max(start);
    if !builder.event(event, start..end) {
      break;
    }
  }
  if !builder.errors.is_empty() {
    builder.close();
  }
  let failed = !builder.errors.is_empty();
  let mut documents = builder.documents;
  if failed {
    for part in &mut documents {
      part.value = None;
    }
  }
  YamlDocument {
    documents,
    errors: builder.errors,
    duplicates: builder.duplicates,
  }
}

/// Byte offsets of `saphyr-parser`'s character indexes.
struct Offsets {
  /// Each character's offset, unless the text is ASCII.
  bytes: Option<Vec<usize>>,
  len: usize,
}

impl Offsets {
  fn new(text: &str) -> Self {
    let bytes = (!text.is_ascii()).then(|| {
      let mut bytes: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
      bytes.push(text.len());
      bytes
    });
    Self {
      bytes,
      len: text.len(),
    }
  }

  fn at(&self, index: usize) -> usize {
    match &self.bytes {
      Some(bytes) => bytes.get(index).copied().unwrap_or(self.len),
      None => index.min(self.len),
    }
  }
}

/// A key read in a mapping, waiting for its value.
struct Key {
  name: String,
  span: Span,
  /// `<<`, whose value's entries are merged into the mapping.
  merge: bool,
}

/// A mapping or sequence being read.
enum Frame {
  Mapping {
    start: usize,
    anchor: usize,
    entries: Vec<(String, Node)>,
    map: Map<String, Json>,
    key: Option<Key>,
    /// The values of `<<` keys, merged when the mapping ends.
    merges: Vec<(Span, Node, Json)>,
    /// The end of the last key or value.
    end: usize,
    /// Values in it, for aliases' limit.
    size: usize,
  },
  Sequence {
    start: usize,
    anchor: usize,
    items: Vec<Node>,
    values: Vec<Json>,
    end: usize,
    size: usize,
  },
}

struct Builder<'a> {
  text: &'a str,
  offsets: Offsets,
  frames: Vec<Frame>,
  /// Anchored values: their trees, values, and sizes.
  anchors: HashMap<usize, (Node, Json, usize)>,
  expanded: usize,
  duplicates: Vec<Span>,
  errors: Vec<SyntaxError>,
  documents: Vec<Part>,
  document_start: usize,
  root: Option<(Node, Json)>,
}

impl Builder<'_> {
  fn error(&mut self, span: Span, message: &str) {
    let mut message = message.trim().to_owned();
    if let Some(first) = message.get(..1) {
      let upper = first.to_uppercase();
      message.replace_range(..1, &upper);
    }
    self.errors.push(SyntaxError {
      span: widen(self.text, span),
      message,
    });
  }

  /// Takes in an event. Returns whether to go on.
  fn event(&mut self, event: Event<'_>, span: Span) -> bool {
    match event {
      Event::StreamStart | Event::StreamEnd | Event::Nothing => {}
      Event::DocumentStart(_) => {
        self.document_start = span.start;
        self.root = None;
      }
      Event::DocumentEnd => self.end_document(),
      Event::Alias(anchor) => {
        let Some((node, value, size)) = self.anchors.get(&anchor).cloned() else {
          self.error(span, "Unknown anchor");
          return false;
        };
        self.expanded += size;
        if self.expanded > MAX_EXPANDED {
          self.error(span, "Aliases expand to too many values");
          return false;
        }
        let node = Node {
          span,
          key_span: None,
          ..node
        };
        self.add(node, value, None, 0, size);
      }
      Event::Scalar(raw, style, anchor, tag) => {
        // A plain scalar's span may take in the bracket after it.
        let span = if style == ScalarStyle::Plain
          && !raw.is_empty()
          && self.text[span.start..].starts_with(raw.as_ref())
        {
          span.start..span.start + raw.len()
        } else {
          span
        };
        let value = resolve(&raw, style, tag.as_deref());
        let node = Node::scalar(span, Scalar::from_json(&value));
        let merge = style == ScalarStyle::Plain && tag.is_none() && raw == "<<";
        self.add(node, value, Some((raw.into_owned(), merge)), anchor, 1);
      }
      Event::MappingStart(anchor, _) | Event::SequenceStart(anchor, _) => {
        if self.frames.len() >= MAX_DEPTH {
          self.error(span.start..span.start + 1, "Too deeply nested");
          return false;
        }
        let start = span.start;
        self
          .frames
          .push(if matches!(event, Event::MappingStart(..)) {
            Frame::Mapping {
              start,
              anchor,
              entries: Vec::new(),
              map: Map::new(),
              key: None,
              merges: Vec::new(),
              end: start,
              size: 1,
            }
          } else {
            Frame::Sequence {
              start,
              anchor,
              items: Vec::new(),
              values: Vec::new(),
              end: start,
              size: 1,
            }
          });
      }
      Event::MappingEnd | Event::SequenceEnd => {
        // A flow collection ends at its bracket; a block one, at its last
        // value (the event is at whatever follows).
        let closed = self.text[span.start..].starts_with(['}', ']']);
        self.end_collection(closed.then_some(span.start + 1));
      }
    }
    true
  }

  /// Ends the collection being read, at `end` or its last value.
  fn end_collection(&mut self, end: Option<usize>) {
    let Some(frame) = self.frames.pop() else {
      return;
    };
    match frame {
      Frame::Mapping {
        start,
        anchor,
        mut entries,
        mut map,
        key,
        merges,
        end: last,
        mut size,
      } => {
        // A key without a value, after an error.
        if let Some(key) = key
          && !key.merge
          && !map.contains_key(&key.name)
        {
          let mut node = Node::scalar(key.span.end..key.span.end, None);
          node.key_span = Some(key.span);
          map.insert(key.name.clone(), Json::Null);
          entries.push((key.name, node));
        }
        // Keys written out win over merged ones, and earlier merges over
        // later ones.
        for (key_span, node, value) in merges {
          let sources: Vec<(Node, Json)> = match (node.kind, value) {
            (NodeKind::Array(nodes), Json::Array(values)) => {
              nodes.into_iter().zip(values).collect()
            }
            (kind, value) => vec![(
              Node {
                kind,
                ..Node::scalar(node.span, None)
              },
              value,
            )],
          };
          for (source, value) in sources {
            let (NodeKind::Table(children), Json::Object(values)) = (source.kind, value) else {
              continue;
            };
            for (name, child) in children {
              let Some(value) = values.get(&name) else {
                continue;
              };
              if map.contains_key(&name) {
                continue;
              }
              size += 1;
              map.insert(name.clone(), value.clone());
              entries.push((
                name,
                Node {
                  key_span: Some(key_span.clone()),
                  ..child
                },
              ));
            }
          }
        }
        let node = Node {
          span: start..end.unwrap_or(last).max(start),
          key_span: None,
          kind: NodeKind::Table(entries),
        };
        self.add(node, Json::Object(map), None, anchor, size);
      }
      Frame::Sequence {
        start,
        anchor,
        items,
        values,
        end: last,
        size,
      } => {
        let node = Node {
          span: start..end.unwrap_or(last).max(start),
          key_span: None,
          kind: NodeKind::Array(items),
        };
        self.add(node, Json::Array(values), None, anchor, size);
      }
    }
  }

  /// Adds a finished value to the collection it's in. `scalar` is a
  /// scalar's text, and whether it's a merge key, for keys.
  fn add(
    &mut self,
    mut node: Node,
    value: Json,
    scalar: Option<(String, bool)>,
    anchor: usize,
    size: usize,
  ) {
    if anchor != 0 {
      self
        .anchors
        .insert(anchor, (node.clone(), value.clone(), size));
    }
    let end = node.span.end;
    match self.frames.last_mut() {
      None => self.root = Some((node, value)),
      Some(Frame::Sequence {
        items,
        values,
        end: last,
        size: total,
        ..
      }) => {
        items.push(node);
        values.push(value);
        *last = end;
        *total += size;
      }
      Some(Frame::Mapping {
        entries,
        map,
        key,
        merges,
        end: last,
        size: total,
        ..
      }) => {
        *last = end;
        *total += size;
        match key.take() {
          None => {
            // A key. A mapping or sequence as a key is named by its text.
            let (name, merge) = scalar.unwrap_or_else(|| {
              let text = self.text.get(node.span.clone()).unwrap_or_default();
              (text.split_whitespace().collect::<Vec<_>>().join(" "), false)
            });
            *key = Some(Key {
              name,
              span: node.span,
              merge,
            });
          }
          Some(Key {
            span: key_span,
            merge: true,
            ..
          }) if matches!(node.kind, NodeKind::Table(_) | NodeKind::Array(_)) => {
            merges.push((key_span, node, value));
          }
          Some(Key {
            name,
            span: key_span,
            ..
          }) => {
            node.key_span = Some(key_span.clone());
            if let Some(existing) = entries.iter().position(|(known, _)| *known == name) {
              self.duplicates.push(key_span);
              entries.remove(existing);
            }
            map.insert(name.clone(), value);
            entries.push((name, node));
          }
        }
      }
    }
  }

  fn end_document(&mut self) {
    let (tree, value) = self.root.take().unwrap_or_else(|| {
      (
        Node::scalar(self.document_start..self.document_start, Some(Scalar::Null)),
        Json::Null,
      )
    });
    let end = tree.span.end.max(self.document_start);
    self.documents.push(Part {
      span: self.document_start..end,
      tree,
      value: Some(value),
    });
  }

  /// Ends whatever is open, after an error, keeping what was read.
  fn close(&mut self) {
    while !self.frames.is_empty() {
      self.end_collection(None);
    }
    if self.root.is_some() {
      self.end_document();
    }
  }
}

/// A scalar's value: by its tag if YAML's own, else by YAML 1.2's core
/// schema if it's plain (unquoted), else a string. Other tags (`!Ref`)
/// don't change what a value is.
fn resolve(raw: &str, style: ScalarStyle, tag: Option<&Tag>) -> Json {
  if let Some(tag) = tag {
    if tag.is_yaml_core_schema() {
      let resolved = match tag.suffix.as_str() {
        "null" => Some(Json::Null),
        "bool" => bool_value(raw).map(Json::Bool),
        "int" => int_value(raw),
        "float" => float_value(raw).or_else(|| int_value(raw)),
        _ => None,
      };
      return resolved.unwrap_or_else(|| Json::String(raw.to_owned()));
    }
    // The non-specific tag: a string.
    if tag.handle == "!" && tag.suffix.is_empty() {
      return Json::String(raw.to_owned());
    }
  }
  if style != ScalarStyle::Plain {
    return Json::String(raw.to_owned());
  }
  plain(raw)
}

/// A plain scalar's value, by YAML 1.2's core schema.
pub fn plain(raw: &str) -> Json {
  if matches!(raw, "" | "~" | "null" | "Null" | "NULL") {
    return Json::Null;
  }
  if let Some(value) = bool_value(raw) {
    return Json::Bool(value);
  }
  int_value(raw)
    .or_else(|| float_value(raw))
    .unwrap_or_else(|| Json::String(raw.to_owned()))
}

fn bool_value(raw: &str) -> Option<bool> {
  match raw {
    "true" | "True" | "TRUE" => Some(true),
    "false" | "False" | "FALSE" => Some(false),
    _ => None,
  }
}

/// `[-+]?[0-9]+`, `0o[0-7]+`, or `0x[0-9a-fA-F]+`.
fn int_value(raw: &str) -> Option<Json> {
  let radix = |digits: &str, radix: u32| {
    (!digits.is_empty())
      .then(|| i64::from_str_radix(digits, radix).ok())
      .flatten()
      .map(|value| Json::Number(value.into()))
  };
  if let Some(octal) = raw.strip_prefix("0o") {
    return radix(octal, 8);
  }
  if let Some(hex) = raw.strip_prefix("0x") {
    return radix(hex, 16);
  }
  let digits = raw.strip_prefix(['-', '+']).unwrap_or(raw);
  if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  let raw = raw.strip_prefix('+').unwrap_or(raw);
  if let Ok(value) = raw.parse::<i64>() {
    return Some(Json::Number(value.into()));
  }
  if let Ok(value) = raw.parse::<u64>() {
    return Some(Json::Number(value.into()));
  }
  raw
    .parse::<f64>()
    .ok()
    .and_then(Number::from_f64)
    .map(Json::Number)
}

/// `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`, or infinity or
/// not a number, which JSON can't hold: those stay as written.
fn float_value(raw: &str) -> Option<Json> {
  let unsigned = raw.strip_prefix(['-', '+']).unwrap_or(raw);
  if matches!(unsigned, ".inf" | ".Inf" | ".INF") || matches!(raw, ".nan" | ".NaN" | ".NAN") {
    return Some(Json::String(raw.to_owned()));
  }
  let bytes = unsigned.as_bytes();
  let mut i = 0;
  let digits = |i: &mut usize| {
    let start = *i;
    while bytes.get(*i).is_some_and(u8::is_ascii_digit) {
      *i += 1;
    }
    *i > start
  };
  let whole = digits(&mut i);
  if bytes.get(i) == Some(&b'.') {
    i += 1;
    let fraction = digits(&mut i);
    if !whole && !fraction {
      return None;
    }
  } else if !whole {
    return None;
  }
  if matches!(bytes.get(i), Some(b'e' | b'E')) {
    i += 1;
    if matches!(bytes.get(i), Some(b'-' | b'+')) {
      i += 1;
    }
    if !digits(&mut i) {
      return None;
    }
  }
  if i != bytes.len() {
    return None;
  }
  let raw = raw.strip_prefix('+').unwrap_or(raw);
  raw
    .parse::<f64>()
    .ok()
    .and_then(Number::from_f64)
    .map(Json::Number)
}

/// Where each document of a stream is in its text: split at `---` and
/// `...` lines, which can't be in a document's content. A `---` line starts
/// the document it introduces.
pub fn document_ranges(text: &str) -> Vec<Span> {
  let mut ranges = Vec::new();
  let mut start = 0;
  let mut line_start = 0;
  while line_start < text.len() {
    let line_end = text[line_start..]
      .find('\n')
      .map_or(text.len(), |end| line_start + end);
    let line = text[line_start..line_end].trim_end_matches('\r');
    if is_marker(line, "---") {
      if line_start > start {
        ranges.push(start..line_start);
      }
      start = line_start;
    } else if is_marker(line, "...") {
      ranges.push(start..line_end);
      start = (line_end + 1).min(text.len());
    }
    line_start = line_end + 1;
  }
  if start < text.len() || ranges.is_empty() {
    ranges.push(start..text.len());
  }
  ranges
}

/// Whether `line` is the document marker `marker` (`---` or `...`), alone
/// or followed by a space.
pub fn is_marker(line: &str, marker: &str) -> bool {
  line
    .strip_prefix(marker)
    .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// The schema a YAML document names: a `# yaml-language-server:
/// $schema=<address>` comment, as the YAML language server reads it, or a
/// `$schema` key at the top of the first document.
pub fn named_schema(text: &str) -> Option<String> {
  for line in text.lines() {
    let Some(comment) = line.trim_start().strip_prefix('#') else {
      continue;
    };
    let Some(settings) = comment.trim_start().strip_prefix("yaml-language-server:") else {
      continue;
    };
    if let Some((_, address)) = settings.split_once("$schema=") {
      let address = address.split_whitespace().next().unwrap_or_default();
      return (!address.is_empty()).then(|| address.to_owned());
    }
  }
  let document = parse_stream(text);
  let first = document.documents.into_iter().next()?;
  match &first.tree.kind {
    NodeKind::Table(_) => {}
    _ => return None,
  }
  match first.tree.get("$schema")?.value()? {
    Scalar::String(address) => Some(address.clone()),
    _ => None,
  }
}

/// Decodes a key as written: quoted, or plain.
pub fn decode_key(raw: &str) -> String {
  let raw = raw.trim();
  if let Some(inner) = raw.strip_prefix('\'') {
    return inner.strip_suffix('\'').unwrap_or(inner).replace("''", "'");
  }
  if raw.starts_with('"') {
    let complete = if raw.len() >= 2 && raw.ends_with('"') {
      raw.to_owned()
    } else {
      format!("{raw}\"")
    };
    let document = parse_stream(&complete);
    if let Some(Json::String(key)) = document
      .documents
      .into_iter()
      .next()
      .and_then(|part| part.value)
    {
      return key;
    }
    return raw.trim_matches('"').to_owned();
  }
  raw.to_owned()
}

/// A string as YAML: plain if it can be read back as the same string, else
/// double-quoted. In a flow collection, `,[]{}` need quotes too.
pub fn string(s: &str, flow: bool) -> String {
  let first = s.chars().next();
  let plain = first.is_some_and(|first| {
    !first.is_whitespace()
      && !"-?:,[]{}#&*!|>'\"%@`".contains(first)
      && !s.ends_with(char::is_whitespace)
      && !s.ends_with(':')
      && !s.contains(": ")
      && !s.contains(" #")
      && !s.chars().any(|c| c.is_control())
      && !(flow && s.contains([',', '[', ']', '{', '}']))
      && plain(s) == Json::String(s.to_owned())
  });
  if plain {
    s.to_owned()
  } else {
    serde_json::to_string(s).unwrap_or_default()
  }
}

/// Writes a value as YAML on one line: in flow style if it's a mapping or
/// sequence.
pub fn render(value: &Json) -> String {
  fn write(value: &Json, flow: bool) -> String {
    match value {
      Json::Null => "null".to_owned(),
      Json::Bool(b) => b.to_string(),
      Json::Number(n) => n.to_string(),
      Json::String(s) => string(s, flow),
      Json::Array(items) => format!(
        "[{}]",
        items
          .iter()
          .map(|item| write(item, true))
          .collect::<Vec<_>>()
          .join(", ")
      ),
      Json::Object(map) => {
        if map.is_empty() {
          return "{}".to_owned();
        }
        format!(
          "{{ {} }}",
          map
            .iter()
            .map(|(key, value)| format!("{}: {}", string(key, true), write(value, true)))
            .collect::<Vec<_>>()
            .join(", ")
        )
      }
    }
  }
  write(value, false)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use flareite_extension_schemas::document::Segment;

  use super::*;

  fn value(text: &str) -> Json {
    let document = parse_stream(text);
    assert!(document.errors.is_empty(), "{:?}", document.errors);
    document.documents[0].value.clone().expect("value")
  }

  #[test]
  fn scalars_resolve_by_the_core_schema() {
    assert_eq!(
      value(
        "a: 1\nb: -0x1F\nc: 0o17\nd: 1.5e3\ne: true\nf: ~\ng: yes\nh: '1'\ni: !!str 2\nj: !!int \"3\"\nk: .inf\nl:\nm: !Ref x\nn: 0x1F\no: +12\np: .5\n"
      ),
      json!({
        "a": 1, "b": "-0x1F", "c": 15, "d": 1500.0, "e": true, "f": null, "g": "yes",
        "h": "1", "i": "2", "j": 3, "k": ".inf", "l": null, "m": "x", "n": 31, "o": 12,
        "p": 0.5
      })
    );
    assert_eq!(
      value("big: 18446744073709551615\n")["big"],
      json!(18446744073709551615u64)
    );
  }

  #[test]
  fn trees_know_where_keys_and_values_are() {
    let text = "name: x\nlist:\n  - a\n  - key: \"é\"\n    other: [1, 2]\n";
    let document = parse_stream(text);
    let tree = &document.documents[0].tree;
    let (name, _) = tree.find(&[Segment::key("name")]);
    assert_eq!(&text[name.span.clone()], "x");
    assert_eq!(&text[name.key_span.clone().expect("key")], "name");
    let (key, taken) = tree.find(&[Segment::key("list"), Segment::Index(1), Segment::key("key")]);
    assert_eq!(taken, 3);
    assert_eq!(&text[key.span.clone()], "\"é\"");
    assert_eq!(key.value(), Some(&Scalar::String("é".into())));
    let (two, _) = tree.find(&[
      Segment::key("list"),
      Segment::Index(1),
      Segment::key("other"),
      Segment::Index(1),
    ]);
    assert_eq!(&text[two.span.clone()], "2");
    let (other, _) = tree.find(&[
      Segment::key("list"),
      Segment::Index(1),
      Segment::key("other"),
    ]);
    assert_eq!(&text[other.span.clone()], "[1, 2]");
    let (item, _) = tree.find(&[Segment::key("list"), Segment::Index(1)]);
    assert_eq!(&text[item.span.clone()], "key: \"é\"\n    other: [1, 2]");
  }

  #[test]
  fn anchors_aliases_and_merge_keys() {
    let text = "base: &base\n  a: 1\n  b: 2\nmore: &more\n  c: 3\nuse:\n  <<: [*base, *more]\n  b: 20\ncopy: *base\n";
    assert_eq!(
      value(text),
      json!({
        "base": { "a": 1, "b": 2 },
        "more": { "c": 3 },
        "use": { "b": 20, "a": 1, "c": 3 },
        "copy": { "a": 1, "b": 2 }
      })
    );
    let document = parse_stream(text);
    let tree = &document.documents[0].tree;
    // A merged key is marked at the `<<`.
    let (a, _) = tree.find(&[Segment::key("use"), Segment::key("a")]);
    assert_eq!(&text[a.key_span.clone().expect("key")], "<<");
    let (copy, _) = tree.find(&[Segment::key("copy")]);
    assert_eq!(&text[copy.span.clone()], "*base");
  }

  #[test]
  fn aliases_cant_expand_without_end() {
    let mut text = String::from("a: &a [1, 1, 1, 1, 1, 1, 1, 1, 1, 1]\n");
    let mut previous = 'a';
    for name in 'b'..='k' {
      text.push_str(&format!(
        "{name}: &{name} [*{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}]\n"
      ));
      previous = name;
    }
    let document = parse_stream(&text);
    assert_eq!(document.errors.len(), 1, "{:?}", document.errors);
    assert!(document.errors[0].message.contains("too many"));
  }

  #[test]
  fn streams_have_documents() {
    let text = "a: 1\n---\nb: 2\n...\n---\n- c\n";
    let document = parse_stream(text);
    assert!(document.errors.is_empty(), "{:?}", document.errors);
    let values: Vec<Json> = document
      .documents
      .iter()
      .map(|part| part.value.clone().expect("value"))
      .collect();
    assert_eq!(values, [json!({"a": 1}), json!({"b": 2}), json!(["c"])]);
    assert_eq!(
      document_ranges(text),
      [0..5, 5..17, 18..text.len()],
      "{:?}",
      document_ranges(text)
        .into_iter()
        .map(|range| &text[range])
        .collect::<Vec<_>>()
    );
    assert_eq!(document_ranges(""), vec![0..0]);
    assert_eq!(document_ranges("# only\n").len(), 1);
    assert!(parse_stream("# only a comment\n").documents.is_empty());
  }

  #[test]
  fn errors_and_duplicates_are_found() {
    let text = "a: 1\nb: [1, 2\nc: 3\n";
    let document = parse_stream(text);
    assert_eq!(document.errors.len(), 1);
    assert!(document.documents[0].value.is_none());
    // What came before the error is kept.
    assert!(document.documents[0].tree.get("a").is_some());
    let text = "a: 1\na: 2\n";
    let document = parse_stream(text);
    assert_eq!(document.duplicates, vec![5..6]);
    assert_eq!(document.documents[0].value, Some(json!({"a": 2})));
    let text = "a: 1\n  b: 2\n";
    assert!(!parse_stream(text).errors.is_empty());
    // At b's colon: b can't be a key of 1.
    assert_eq!(parse_stream(text).errors[0].span.start, 8);
  }

  #[test]
  fn documents_name_their_schemas() {
    assert_eq!(
      named_schema("# yaml-language-server: $schema=https://x/s.json\na: 1\n"),
      Some("https://x/s.json".into())
    );
    assert_eq!(
      named_schema("$schema: ./s.json\na: 1\n"),
      Some("./s.json".into())
    );
    assert_eq!(named_schema("a: 1\n"), None);
  }

  #[test]
  fn values_render_as_yaml() {
    assert_eq!(render(&json!("dark")), "dark");
    assert_eq!(render(&json!("true")), "\"true\"");
    assert_eq!(render(&json!("12")), "\"12\"");
    assert_eq!(render(&json!("a: b")), "\"a: b\"");
    assert_eq!(render(&json!("- x")), "\"- x\"");
    assert_eq!(render(&json!("")), "\"\"");
    assert_eq!(render(&json!(["a,b", 1, null])), "[\"a,b\", 1, null]");
    assert_eq!(render(&json!({"k": "v"})), "{ k: v }");
    assert_eq!(decode_key("'it''s'"), "it's");
    assert_eq!(decode_key("\"a\\tb\""), "a\tb");
    assert_eq!(decode_key("plain key"), "plain key");
  }
}
