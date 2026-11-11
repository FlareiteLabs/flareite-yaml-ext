//! YAML for Flareite: YAML streams parsed for Flareite's schemas, which
//! check each of their documents, and their keys and values completed and
//! described from what the schemas say, written as YAML writes them.

mod parse;
mod place;

use std::sync::Mutex;

use flareite_extension_api::types::{
  Activation, CommandContext, DocumentChange, DocumentId, DocumentInfo,
};
use flareite_extension_api::{Guest, export_extension};
use flareite_extension_schemas::document::{Document, Segment};
use flareite_extension_schemas::{Documents, Header, Language, Place, Property, Shape};
use serde_json::Value as Json;

/// YAML, as Flareite's schemas see it.
pub struct Yaml;

/// The start of the line `at` is on.
fn line_start(text: &str, at: usize) -> usize {
  text[..at].rfind('\n').map_or(0, |at| at + 1)
}

impl Language for Yaml {
  fn languages(&self) -> &[&str] {
    &["yaml"]
  }

  fn parse(&self, text: &str) -> Document {
    parse::parse(text)
  }

  fn place(&self, text: &str, offset: usize) -> Place {
    place::place(text, offset)
  }

  fn decode_key(&self, raw: &str) -> String {
    parse::decode_key(raw)
  }

  fn key(&self, name: &str) -> String {
    parse::string(name, false)
  }

  /// `key: ` for a value; a mapping or sequence goes on the lines after its
  /// key, indented two spaces past it. A value of several kinds
  /// (`string | object`) is written as the user chooses.
  fn insert_property(
    &self,
    key: &str,
    property: &Property,
    _header: Header,
    text: &str,
    start: usize,
  ) -> String {
    let column = text[line_start(text, start)..start].chars().count();
    let indent = " ".repeat(column + 2);
    let several = property
      .kind
      .as_ref()
      .is_some_and(|kind| kind.contains(" | "));
    match property.shape {
      Shape::Value => format!("{key}: "),
      _ if several => format!("{key}: "),
      Shape::Table => format!("{key}:\n{indent}"),
      Shape::Array | Shape::ArrayOfTables => format!("{key}:\n{indent}- "),
    }
  }

  /// Plain when a string reads back the same; in the quotes begun at
  /// `start`, if there are any.
  fn insert_value(&self, value: &Json, text: &str, start: usize) -> String {
    let quote = text[start..]
      .chars()
      .next()
      .filter(|c| matches!(c, '"' | '\''));
    match (quote, value) {
      (Some('"'), Json::String(s)) => serde_json::to_string(s).unwrap_or_default(),
      (Some(_), Json::String(s)) => format!("'{}'", s.replace('\'', "''")),
      _ => parse::render(value),
    }
  }

  fn render(&self, value: &Json) -> String {
    parse::render(value)
  }

  /// After a sequence's `-`, what's typed may be the item itself.
  fn values_at_key(&self, text: &str, table: &[Segment], start: usize) -> bool {
    let before = &text[line_start(text, start)..start];
    matches!(table.last(), Some(Segment::Index(_)))
      && before.contains('-')
      && before.trim_start_matches([' ', '-']).is_empty()
  }
}

static DOCUMENTS: Mutex<Option<Documents<Yaml>>> = Mutex::new(None);

fn with_documents<T>(f: impl FnOnce(&mut Documents<Yaml>) -> T) -> T {
  let mut documents = DOCUMENTS
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  f(documents.get_or_insert_with(|| Documents::new(Yaml)))
}

struct Extension;

impl Guest for Extension {
  fn activate(_context: Activation) -> Result<(), String> {
    Ok(())
  }

  fn deactivate() {}

  fn run_command(_command: String, _context: CommandContext) -> Result<(), String> {
    with_documents(Documents::run_command).map(|_| ())
  }

  fn document_opened(document: DocumentInfo) {
    with_documents(|documents| documents.opened(document));
  }

  fn document_changed(change: DocumentChange) {
    with_documents(|documents| documents.changed(change));
  }

  fn document_closed(document: DocumentId) {
    with_documents(|documents| documents.closed(document));
  }
}

export_extension!(Extension);

#[cfg(test)]
mod tests {
  use flareite_extension_schemas::testing::{self, Schema};
  use flareite_extension_schemas::{Completion, testing::Severity};
  use serde_json::json;

  use super::*;

  /// A schema shaped like a CI workflow's.
  fn workflow() -> Schema {
    testing::json_schema(json!({
      "type": "object",
      "required": ["jobs"],
      "additionalProperties": false,
      "properties": {
        "name": { "type": "string", "description": "The workflow's name." },
        "on": { "type": "array", "items": { "enum": ["push", "pull_request"] } },
        "jobs": {
          "type": "object",
          "additionalProperties": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
              "runs-on": { "enum": ["ubuntu-latest", "windows-latest"] },
              "timeout": { "type": "integer", "minimum": 1 },
              "env": { "type": ["string", "object"] },
              "steps": {
                "type": "array",
                "items": {
                  "type": "object",
                  "additionalProperties": false,
                  "properties": {
                    "uses": { "type": "string" },
                    "run": { "type": "string" },
                    "with": { "type": "object" },
                    "shell": { "enum": ["bash", "pwsh"], "default": "bash" }
                  }
                }
              }
            }
          }
        }
      }
    }))
  }

  fn marked(text: &str, schema: &Schema) -> Vec<String> {
    testing::check(&Yaml, text, &[schema])
      .iter()
      .map(|d| text[d.range.clone()].to_owned())
      .collect()
  }

  #[test]
  fn documents_are_checked_where_problems_are() {
    let schema = workflow();
    let text = "name: CI\non: [push, tag]\njobs:\n  build:\n    runs-on: macos\n    timeout: 0\n    steps:\n      - uses: x\n        shel: bash\n      - 12\n";
    assert_eq!(marked(text, &schema), ["tag", "macos", "0", "shel", "12"]);
    // Each document of a stream, and problems where they are.
    assert_eq!(
      marked("jobs: {}\n---\nname: 1\n", &schema),
      ["name: 1", "1"]
    );
    // Merged keys are checked; a value is marked where it's written.
    assert_eq!(
      marked("x: &x\n  timeout: -1\njobs:\n  a:\n    <<: *x\n", &schema),
      ["x", "-1"]
    );
    // Syntax errors come alone; so do duplicate keys.
    let broken = testing::check(&Yaml, "jobs:\n  a: [\n", &[&schema]);
    assert_eq!(broken.len(), 1, "{broken:?}");
    let twice = testing::check(&Yaml, "jobs: {}\njobs: {}\n", &[&schema]);
    assert_eq!(twice.len(), 1, "{twice:?}");
    assert!(twice[0].message.starts_with("Duplicate key"));
    assert_eq!(twice[0].severity, Severity::Error);
    // An empty file has nothing to check.
    assert!(testing::check(&Yaml, "# nothing yet\n", &[&schema]).is_empty());
    // TOML Schemas check YAML too.
    let tosd = testing::toml_schema(
      "[toml-schema]\nversion = \"1.0.0\"\n[elements.port]\ntype = \"integer\"\n",
    );
    assert_eq!(marked("port: x\n", &tosd), ["x"]);
  }

  fn complete(text: &str, schema: &Schema) -> Vec<Completion> {
    let offset = text.find('‸').expect("cursor");
    let text = text.replacen('‸', "", 1);
    testing::complete(&Yaml, &text, offset, &[schema])
  }

  fn labels(items: &[Completion]) -> Vec<&str> {
    let mut labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
    labels.sort_unstable();
    labels
  }

  #[test]
  fn keys_and_values_complete() {
    let schema = workflow();
    let items = complete("name: x\n‸", &schema);
    assert_eq!(labels(&items), ["jobs", "on"]);
    let jobs = items
      .iter()
      .find(|item| item.label == "jobs")
      .expect("jobs");
    assert_eq!(jobs.insert, "jobs:\n  ");
    let on = items.iter().find(|item| item.label == "on").expect("on");
    assert_eq!(on.insert, "on:\n  - ");
    // Nested, and indented from the key's column.
    let items = complete("jobs:\n  build:\n    ‸", &schema);
    assert_eq!(labels(&items), ["env", "runs-on", "steps", "timeout"]);
    let env = items.iter().find(|item| item.label == "env").expect("env");
    assert_eq!(
      env.insert, "env: ",
      "a string or a mapping: the user's choice"
    );
    let steps = items
      .iter()
      .find(|item| item.label == "steps")
      .expect("steps");
    assert_eq!(steps.insert, "steps:\n      - ");
    let values = complete("jobs:\n  build:\n    runs-on: ‸", &schema);
    assert_eq!(labels(&values), ["ubuntu-latest", "windows-latest"]);
    let quoted = complete("jobs:\n  b:\n    runs-on: \"ub‸\"", &schema);
    assert_eq!(quoted[0].insert, "\"ubuntu-latest\"");
    assert_eq!(quoted[0].start, 24);
    // A step's keys, after its dash; then the next one's.
    let items = complete("jobs:\n  b:\n    steps:\n      - ‸", &schema);
    assert_eq!(labels(&items), ["run", "shell", "uses", "with"]);
    let items = complete(
      "jobs:\n  b:\n    steps:\n      - uses: x\n        ‸",
      &schema,
    );
    assert_eq!(labels(&items), ["run", "shell", "with"]);
    let shell = &complete("jobs:\n  b:\n    steps:\n      - shell: ‸", &schema);
    assert_eq!(labels(shell), ["bash", "pwsh"]);
    assert!(
      shell
        .iter()
        .any(|item| item.sort.starts_with('0') && item.label == "bash"),
      "the default first"
    );
    // A sequence's item may be a value.
    let items = complete("on:\n  - p‸", &schema);
    assert_eq!(labels(&items), ["pull_request", "push"]);
    let items = complete("on: [push, ‸]", &schema);
    assert_eq!(labels(&items), ["pull_request", "push"]);
  }

  #[test]
  fn hovers_describe_keys_and_values() {
    let schema = workflow();
    let text = "name: CI\njobs:\n  b:\n    timeout: 5\n";
    let (markdown, span) = testing::hover(&Yaml, text, 2, &[&schema]).expect("hover");
    assert_eq!(&text[span], "name");
    assert!(markdown.starts_with("The workflow's name."), "{markdown}");
    let offset = text.find('5').expect("timeout");
    let (markdown, span) = testing::hover(&Yaml, text, offset, &[&schema]).expect("hover");
    assert_eq!(&text[span], "5");
    assert!(markdown.contains("At least 1"), "{markdown}");
  }
}
