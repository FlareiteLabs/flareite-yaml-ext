//! Where the cursor is in a YAML document being typed. YAML's structure is
//! its indentation, so the lines of the cursor's document are read up to
//! it, keeping the mappings and sequences open at each column; a flow
//! collection (`[a, b]`, `{a: 1}`) is read as JSON is. What's unfinished
//! is tolerated: a document rarely parses while it's typed.

use flareite_extension_schemas::document::Segment;
use flareite_extension_schemas::{Header, Place};

use crate::parse::{decode_key, is_marker};

/// Longest document read for context.
const MAX_SCAN: usize = 8 * 1024 * 1024;

/// What's being typed at `offset` in YAML `text`. A key typed as a
/// sequence's item (`- na`) is a key of the item, which may be a value too.
pub fn place(text: &str, offset: usize) -> Place {
  let offset = offset.min(text.len());
  if offset > MAX_SCAN {
    return Place::Nothing;
  }
  let mut lexer = Lexer {
    text,
    cursor: offset,
    levels: Vec::new(),
    pending: None,
    block: None,
    skip_to: 0,
  };
  let mut line_start = 0;
  loop {
    let line_end = text[line_start..]
      .find('\n')
      .map_or(text.len(), |end| line_start + end);
    let on_cursor = offset <= line_end;
    if line_start >= lexer.skip_to {
      let place = lexer.line(line_start, line_end, on_cursor);
      if let Some(place) = place {
        return place;
      }
    }
    if on_cursor || line_end >= text.len() {
      return Place::Nothing;
    }
    line_start = line_end + 1;
  }
}

/// A mapping or sequence open at a column.
struct Level {
  indent: usize,
  path: Vec<Segment>,
  /// A sequence's next item's index; `None` for a mapping.
  seq: Option<usize>,
}

/// A key or item whose value is on the lines after it.
struct Pending {
  path: Vec<Segment>,
  column: usize,
  /// A key's value may be a sequence at the key's own column.
  compact: bool,
}

struct Lexer<'a> {
  text: &'a str,
  cursor: usize,
  levels: Vec<Level>,
  pending: Option<Pending>,
  /// In a block scalar: lines indented more than this column are its text.
  block: Option<usize>,
  /// Lines before this are in a flow collection or quoted string already
  /// read.
  skip_to: usize,
}

fn is_space(b: u8) -> bool {
  matches!(b, b' ' | b'\t')
}

/// Whether `rest` starts with a sequence's `-`.
fn is_dash(rest: &str) -> bool {
  rest
    .strip_prefix('-')
    .is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t']))
}

impl Lexer<'_> {
  fn skip_spaces(&self, mut at: usize, end: usize) -> usize {
    let bytes = self.text.as_bytes();
    while at < end && is_space(bytes[at]) {
      at += 1;
    }
    at
  }

  /// Reads a line. Returns the place if the cursor is on it, or whatever
  /// ends the search.
  fn line(&mut self, start: usize, end: usize, on_cursor: bool) -> Option<Place> {
    let line = self.text[start..end].trim_end_matches('\r');
    let end = start + line.len();
    let indent = line.len() - line.trim_start_matches(' ').len();
    let content = start + indent;
    let rest = line[indent..].trim_start_matches('\t');
    let blank = rest.trim().is_empty();
    let column = self.cursor.saturating_sub(start);
    if let Some(block) = self.block {
      // A blank line where the cursor is at or before the block's column
      // may start what follows it.
      let inside = if blank {
        !(on_cursor && column <= block)
      } else {
        indent > block
      };
      if inside {
        return on_cursor.then_some(Place::Nothing);
      }
      self.block = None;
    }
    if indent == 0 && (is_marker(line, "---") || is_marker(line, "...")) {
      self.levels.clear();
      self.pending = None;
      return on_cursor.then_some(Place::Nothing);
    }
    if indent == 0 && line.starts_with('%') {
      return on_cursor.then_some(Place::Nothing);
    }
    if blank || rest.starts_with('#') {
      if !on_cursor {
        return None;
      }
      if !blank && self.cursor > content {
        return Some(Place::Nothing);
      }
      return Some(self.empty(column));
    }
    if on_cursor && self.cursor < content {
      return Some(Place::Nothing);
    }
    if !self.enter(indent, is_dash(rest)) {
      // The rest of a multi-line scalar, or a mistake.
      return on_cursor.then_some(Place::Nothing);
    }
    let place = self.content(start, content, end, on_cursor);
    place.or(on_cursor.then_some(Place::Nothing))
  }

  /// The place on an empty line, at `column`: a key of the mapping open at
  /// that column, or of the value of the key before.
  fn empty(&self, column: usize) -> Place {
    let key = |table: &[Segment]| Place::Key {
      table: table.to_vec(),
      start: self.cursor,
      end: self.cursor,
      header: Header::None,
    };
    if let Some(pending) = &self.pending
      && column > pending.column
    {
      return key(&pending.path);
    }
    if self.levels.is_empty() {
      return key(&[]);
    }
    for level in self.levels.iter().rev() {
      if level.indent > column {
        continue;
      }
      if level.indent < column {
        break;
      }
      // At a sequence's column, the mapping whose key it's the value of,
      // if it's at the same column; else an item would start here.
      if level.seq.is_none() {
        return key(&level.path);
      }
    }
    Place::Nothing
  }

  /// Finds the collection a line's content at `column` is in, opening the
  /// one a key's value starts. Returns whether it's in one.
  fn enter(&mut self, column: usize, dash: bool) -> bool {
    if let Some(pending) = self.pending.take()
      && (column > pending.column || (column == pending.column && dash && pending.compact))
    {
      self.levels.push(Level {
        indent: column,
        path: pending.path,
        seq: dash.then_some(0),
      });
      return true;
    }
    while self
      .levels
      .last()
      .is_some_and(|level| level.indent > column)
    {
      self.levels.pop();
    }
    // A sequence at its key's column ends at the next key.
    if !dash
      && self
        .levels
        .last()
        .is_some_and(|level| level.indent == column && level.seq.is_some())
    {
      self.levels.pop();
    }
    match self.levels.last() {
      Some(level) if level.indent == column => level.seq.is_some() == dash,
      Some(_) => false,
      None => {
        self.levels.push(Level {
          indent: column,
          path: Vec::new(),
          seq: dash.then_some(0),
        });
        true
      }
    }
  }

  /// Reads a line's content from `at`: its sequences' dashes, then a key
  /// and its value, or a value.
  fn content(
    &mut self,
    line_start: usize,
    mut at: usize,
    end: usize,
    on_cursor: bool,
  ) -> Option<Place> {
    let text = self.text;
    let cursor = if on_cursor { self.cursor } else { usize::MAX };
    let mut path = self
      .levels
      .last()
      .map(|level| level.path.clone())
      .unwrap_or_default();
    let mut item = false;
    loop {
      let column = at - line_start;
      if is_dash(&text[at..end]) {
        if cursor == at {
          return Some(Place::Nothing);
        }
        let index = match self.levels.last_mut() {
          Some(Level {
            indent,
            seq: Some(next),
            ..
          }) if *indent == column => {
            *next += 1;
            *next - 1
          }
          _ => {
            self.levels.push(Level {
              indent: column,
              path: path.clone(),
              seq: Some(1),
            });
            0
          }
        };
        path = self
          .levels
          .last()
          .map(|level| level.path.clone())
          .unwrap_or_default();
        path.push(Segment::Index(index));
        let after = at + 1;
        at = self.skip_spaces(after, end);
        item = true;
        let rest = &text[at..end];
        if rest.is_empty() || rest.starts_with('#') {
          // The item is on the lines after it.
          self.pending = Some(Pending {
            path: path.clone(),
            column,
            compact: false,
          });
          if cursor > after && cursor <= end && !(rest.starts_with('#') && cursor > at) {
            return Some(Place::Key {
              table: path,
              start: cursor,
              end: cursor,
              header: Header::None,
            });
          }
          return None;
        }
        if cursor < at {
          return Some(Place::Nothing);
        }
        continue;
      }
      if let Some((key_end, after_colon)) = self.key_at(at, end) {
        let in_mapping = matches!(
          self.levels.last(),
          Some(Level { indent, seq: None, .. }) if *indent == column
        );
        if !in_mapping {
          self.levels.push(Level {
            indent: column,
            path: path.clone(),
            seq: None,
          });
        }
        if cursor >= at && cursor <= key_end {
          return Some(Place::Key {
            table: path,
            start: at,
            end: key_end,
            header: Header::None,
          });
        }
        if cursor > key_end && cursor < after_colon {
          return Some(Place::Nothing);
        }
        path.push(Segment::Key(decode_key(&text[at..key_end])));
        return self.value(after_colon, end, path, column, cursor, true);
      }
      // A value on its own: a key being typed (an item's may be its
      // value), or an item's value.
      let first = text.as_bytes()[at];
      let key_end = match first {
        b'"' | b'\'' if !item => Some(quoted_end(text, at).min(end)),
        b'[' | b'{' | b'"' | b'\'' | b'|' | b'>' | b'&' | b'!' => None,
        _ => Some(self.plain_end(at, end)),
      };
      if let Some(key_end) = key_end {
        return (cursor >= at && cursor <= key_end).then_some(Place::Key {
          table: path,
          start: at,
          end: key_end,
          header: Header::None,
        });
      }
      return self.value(at, end, path, column, cursor, false);
    }
  }

  /// A key at `at`: where it ends, and where its value may start (after
  /// the `:`).
  fn key_at(&self, at: usize, end: usize) -> Option<(usize, usize)> {
    let bytes = self.text.as_bytes();
    let colon = |i: usize| bytes[i] == b':' && (i + 1 >= end || is_space(bytes[i + 1]));
    match bytes[at] {
      b'"' | b'\'' => {
        let quote_end = quoted_end(self.text, at);
        if quote_end > end {
          return None;
        }
        let i = self.skip_spaces(quote_end, end);
        (i < end && colon(i)).then_some((quote_end, i + 1))
      }
      b'[' | b'{' | b'|' | b'>' | b'&' | b'!' | b'*' | b'#' | b'%' | b'@' | b'`' | b'?' => None,
      _ => {
        let mut i = at;
        while i < end {
          if colon(i) {
            let key = self.text[at..i].trim_end_matches([' ', '\t']);
            return Some((at + key.len(), i + 1));
          }
          if bytes[i] == b'#' && i > at && is_space(bytes[i - 1]) {
            return None;
          }
          i += 1;
        }
        None
      }
    }
  }

  /// Where a plain scalar from `at` ends on its line: before a comment, and
  /// trailing spaces.
  fn plain_end(&self, at: usize, end: usize) -> usize {
    let bytes = self.text.as_bytes();
    let mut stop = end;
    for i in at..end {
      if bytes[i] == b'#' && i > at && is_space(bytes[i - 1]) {
        stop = i;
        break;
      }
    }
    at + self.text[at..stop].trim_end_matches([' ', '\t']).len()
  }

  /// Reads the value at `path` from `from` (after a `:` or `-` at
  /// `column`).
  fn value(
    &mut self,
    from: usize,
    end: usize,
    path: Vec<Segment>,
    column: usize,
    cursor: usize,
    key: bool,
  ) -> Option<Place> {
    let text = self.text;
    let bytes = text.as_bytes();
    let mut at = self.skip_spaces(from, end);
    // Anchors and tags before it.
    while at < end && matches!(bytes[at], b'&' | b'!') {
      let token_end = (at..end).find(|&i| is_space(bytes[i])).unwrap_or(end);
      if cursor > at && cursor <= token_end {
        return Some(Place::Nothing);
      }
      at = self.skip_spaces(token_end, end);
    }
    let rest = &text[at..end];
    if rest.is_empty() || rest.starts_with('#') {
      self.pending = Some(Pending {
        path: path.clone(),
        column,
        compact: key,
      });
      let in_comment = rest.starts_with('#') && cursor > at;
      // Right after the colon, a space must come first.
      return (cursor > from && cursor <= end && !in_comment).then_some(Place::Value {
        path,
        start: cursor,
        end: cursor,
      });
    }
    if cursor >= from && cursor < at {
      return Some(Place::Nothing);
    }
    match bytes[at] {
      b'|' | b'>' => {
        self.block = Some(column);
        None
      }
      b'[' | b'{' => match flow(text, at, path, self.cursor) {
        Err(place) => Some(place),
        Ok(after) => {
          self.skip_to = after;
          None
        }
      },
      b'"' | b'\'' => {
        let quote_end = quoted_end(text, at);
        if cursor > at && cursor <= quote_end.min(end) {
          return Some(Place::Value {
            path,
            start: at,
            end: quote_end.min(end),
          });
        }
        self.skip_to = quote_end;
        None
      }
      _ => {
        let plain_end = self.plain_end(at, end);
        (cursor >= at && cursor <= plain_end).then_some(Place::Value {
          path,
          start: at,
          end: plain_end,
        })
      }
    }
  }
}

/// Where a quoted string starting at `start` ends (after its closing
/// quote), which may be on a later line.
fn quoted_end(text: &str, start: usize) -> usize {
  let bytes = text.as_bytes();
  let quote = bytes[start];
  let mut i = start + 1;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' if quote == b'"' => i += 2,
      b'\'' if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') => i += 2,
      b if b == quote => return i + 1,
      _ => i += 1,
    }
  }
  bytes.len()
}

/// Reads a flow collection from its bracket at `from`, at `path`. Returns
/// where it ends, or the place, if the cursor is in it.
fn flow(text: &str, from: usize, path: Vec<Segment>, cursor: usize) -> Result<usize, Place> {
  #[derive(PartialEq)]
  enum Expect {
    Key,
    Colon,
    Value,
    Comma,
  }
  struct Frame {
    path: Vec<Segment>,
    mapping: bool,
    key: Option<String>,
    index: usize,
    expect: Expect,
  }
  let bytes = text.as_bytes();
  let value_path = |frames: &[Frame]| -> Option<Vec<Segment>> {
    let Some(frame) = frames.last() else {
      return Some(path.clone());
    };
    if frame.expect != Expect::Value {
      return None;
    }
    let mut path = frame.path.clone();
    path.push(if frame.mapping {
      Segment::Key(frame.key.clone()?)
    } else {
      Segment::Index(frame.index)
    });
    Some(path)
  };
  let here = |frames: &[Frame], start: usize, end: usize| -> Place {
    match frames.last() {
      Some(frame) if frame.mapping && frame.expect == Expect::Key => Place::Key {
        table: frame.path.clone(),
        start,
        end,
        header: Header::None,
      },
      _ => match value_path(frames) {
        Some(path) => Place::Value { path, start, end },
        None => Place::Nothing,
      },
    }
  };
  // Whether a `:` at `i` separates a key from its value.
  let colon = |i: usize| {
    bytes.get(i + 1).is_none_or(|&b| {
      matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | b',' | b'[' | b']' | b'{' | b'}'
      )
    })
  };
  let mut frames: Vec<Frame> = Vec::new();
  let mut at = from;
  loop {
    // Spaces, line breaks, and comments.
    loop {
      match bytes.get(at) {
        Some(b' ' | b'\t' | b'\n' | b'\r') if at < cursor => at += 1,
        Some(b'#') if at < cursor && at > 0 && bytes[at - 1].is_ascii_whitespace() => {
          while bytes.get(at).is_some_and(|&b| b != b'\n') {
            at += 1;
          }
          if at >= cursor {
            return Err(Place::Nothing);
          }
        }
        _ => break,
      }
    }
    if at >= cursor {
      return Err(here(&frames, cursor, cursor));
    }
    if at >= bytes.len() {
      return Ok(at);
    }
    let start = at;
    match bytes[at] {
      bracket @ (b'{' | b'[') => {
        let path = value_path(&frames).unwrap_or_default();
        let mapping = bracket == b'{';
        frames.push(Frame {
          path,
          mapping,
          key: None,
          index: 0,
          expect: if mapping { Expect::Key } else { Expect::Value },
        });
        at += 1;
        continue;
      }
      b'}' | b']' => {
        at += 1;
        frames.pop();
        if frames.is_empty() {
          return Ok(at);
        }
      }
      b',' => {
        at += 1;
        if let Some(frame) = frames.last_mut() {
          if frame.mapping {
            frame.key = None;
            frame.expect = Expect::Key;
          } else {
            frame.index += 1;
            frame.expect = Expect::Value;
          }
        }
        continue;
      }
      b':' if colon(at) => {
        at += 1;
        if let Some(frame) = frames.last_mut()
          && frame.mapping
        {
          frame.expect = Expect::Value;
        }
        continue;
      }
      b'"' | b'\'' => {
        let end = quoted_end(text, at);
        let closed = end > start + 1 && bytes[end - 1] == bytes[start];
        if (cursor > start && cursor < end) || (cursor == end && !closed) {
          return Err(here(&frames, start, end));
        }
        at = end;
        if let Some(frame) = frames.last_mut()
          && frame.mapping
          && frame.expect == Expect::Key
        {
          frame.key = Some(decode_key(&text[start..end]));
          frame.expect = Expect::Colon;
          continue;
        }
      }
      _ => {
        let mut end = at;
        while end < bytes.len() {
          let b = bytes[end];
          if matches!(b, b',' | b'[' | b']' | b'{' | b'}' | b'\n' | b'\r')
            || (b == b':' && colon(end))
            || (b == b'#' && end > at && is_space(bytes[end - 1]))
          {
            break;
          }
          end += 1;
        }
        let end = at + text[at..end].trim_end_matches([' ', '\t']).len();
        if cursor <= end {
          return Err(here(&frames, start, end));
        }
        at = end.max(at + 1);
        while !text.is_char_boundary(at) {
          at += 1;
        }
        if let Some(frame) = frames.last_mut()
          && frame.mapping
          && frame.expect == Expect::Key
        {
          frame.key = Some(text[start..end].to_owned());
          frame.expect = Expect::Colon;
          continue;
        }
      }
    }
    // After a value.
    if let Some(frame) = frames.last_mut() {
      frame.expect = Expect::Comma;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The place at `‸` (YAML has `|`).
  fn at(text: &str) -> Place {
    let offset = text.find('‸').expect("cursor");
    place(&text.replacen('‸', "", 1), offset)
  }

  fn keys(path: &[&str]) -> Vec<Segment> {
    path.iter().map(|key| Segment::key(*key)).collect()
  }

  fn key_of(place: &Place) -> Option<&[Segment]> {
    match place {
      Place::Key { table, .. } => Some(table),
      _ => None,
    }
  }

  fn value_of(place: &Place) -> Option<&[Segment]> {
    match place {
      Place::Value { path, .. } => Some(path),
      _ => None,
    }
  }

  #[test]
  fn keys_in_mappings() {
    assert_eq!(key_of(&at("‸")), Some(&[][..]));
    assert_eq!(key_of(&at("name: x\n‸")), Some(&[][..]));
    assert_eq!(key_of(&at("name: x\nna‸")), Some(&[][..]));
    assert!(matches!(
      at("name: x\nve‸rsion: 1"),
      Place::Key {
        start: 8,
        end: 15,
        ..
      }
    ));
    assert_eq!(
      key_of(&at("server:\n  ‸")),
      Some(&keys(&["server"])[..]),
      "the next line of a key without a value"
    );
    assert_eq!(
      key_of(&at("server:\n  port: 1\n  ho‸")),
      Some(&keys(&["server"])[..])
    );
    assert_eq!(key_of(&at("server:\n  port: 1\n‸")), Some(&[][..]));
    assert_eq!(
      key_of(&at("a:\n  b:\n    c: 1\n  ‸")),
      Some(&keys(&["a"])[..])
    );
    assert_eq!(key_of(&at("\"quoted key‸\": 1")), Some(&[][..]));
    assert_eq!(at("a: 1 # note ‸"), Place::Nothing);
    assert_eq!(at("# note ‸"), Place::Nothing);
    // More indented than anything open: not a place for a key.
    assert_eq!(at("a: 1\n    ‸"), Place::Nothing);
  }

  #[test]
  fn values() {
    assert_eq!(value_of(&at("theme: ‸")), Some(&keys(&["theme"])[..]));
    assert!(matches!(
      at("theme: da‸rk"),
      Place::Value {
        start: 7,
        end: 11,
        ..
      }
    ));
    assert!(matches!(
      at("theme: \"da‸rk\" # c"),
      Place::Value {
        start: 7,
        end: 13,
        ..
      }
    ));
    assert_eq!(at("theme:‸"), Place::Nothing);
    assert_eq!(
      value_of(&at("a:\n  b: &anchor !!str x‸")),
      Some(&keys(&["a", "b"])[..])
    );
    // After a block scalar.
    assert_eq!(
      key_of(&at("run: |\n  echo hi\n  echo there\n‸")),
      Some(&[][..])
    );
    assert_eq!(at("run: |\n  echo ‸"), Place::Nothing);
    assert_eq!(key_of(&at("run: >-\n  text\n\nne‸")), Some(&[][..]));
  }

  #[test]
  fn sequences() {
    let item = |index: usize| vec![Segment::key("steps"), Segment::Index(index)];
    assert_eq!(key_of(&at("steps:\n  - ‸")), Some(&item(0)[..]));
    assert_eq!(
      key_of(&at("steps:\n  - uses: x\n  - na‸")),
      Some(&item(1)[..])
    );
    assert_eq!(
      key_of(&at("steps:\n  - uses: x\n    ‸")),
      Some(&item(0)[..])
    );
    assert_eq!(
      key_of(&at("steps:\n- uses: x\n- run: y\n  ‸")),
      Some(&item(1)[..])
    );
    // A sequence at its key's column ends at the next key.
    assert_eq!(key_of(&at("steps:\n- uses: x\n‸")), Some(&[][..]));
    assert_eq!(
      value_of(&at("steps:\n  - uses: x\n    with:\n      a: ‸")),
      Some(
        &[
          Segment::key("steps"),
          Segment::Index(0),
          Segment::key("with"),
          Segment::key("a")
        ][..]
      )
    );
    assert_eq!(
      key_of(&at("-\n  ‸")),
      Some(&[Segment::Index(0)][..]),
      "an item on the lines after its dash"
    );
    assert_eq!(
      key_of(&at("- - a\n  - ‸")),
      Some(&[Segment::Index(0), Segment::Index(1)][..])
    );
    assert_eq!(
      value_of(&at("a:\n  - x\n  - [1, ‸")),
      Some(&[Segment::key("a"), Segment::Index(1), Segment::Index(1)][..])
    );
  }

  #[test]
  fn flow_collections() {
    assert_eq!(
      value_of(&at("on: [push, pu‸")),
      Some(&[Segment::key("on"), Segment::Index(1)][..])
    );
    assert_eq!(key_of(&at("a: { b: 1, ‸")), Some(&keys(&["a"])[..]));
    assert_eq!(value_of(&at("a: { b: ‸")), Some(&keys(&["a", "b"])[..]));
    assert_eq!(
      key_of(&at("a: [1,\n  2]\n‸")),
      Some(&[][..]),
      "a flow collection over lines"
    );
    assert_eq!(
      value_of(&at("a: {\"b c\": ‸")),
      Some(&keys(&["a", "b c"])[..])
    );
  }

  #[test]
  fn documents_of_a_stream() {
    assert_eq!(key_of(&at("a:\n  b: 1\n---\n‸")), Some(&[][..]));
    assert_eq!(at("---‸"), Place::Nothing);
    assert_eq!(value_of(&at("--- \nkind: ‸")), Some(&keys(&["kind"])[..]));
  }
}
