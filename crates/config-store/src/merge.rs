//! Format-preserving rewrite of the configuration file.
//!
//! [`render_update`] takes the text currently on disk and the configuration
//! that should be stored, and produces new text in which only the values that
//! differ are touched. Every untouched key, comment, blank line, key order,
//! quoting style and number format stays exactly as the user wrote it.
//!
//! # How
//!
//! The text is parsed twice: into a [`toml_edit::DocumentMut`], which keeps
//! all formatting, and into a [`Config`], which is serialised to a plain value
//! tree (`old`). The target configuration is serialised the same way (`new`).
//! The two trees are compared structurally and the differences — and nothing
//! else — are applied to the document:
//!
//! * a changed scalar keeps the comments around its key;
//! * a new key is appended to its table. Missing sections are created as
//!   `[section]` headers and lists of tables as `[[section.name]]`; small maps
//!   inside list elements (`headers`, `set`, `thinking`) are written inline;
//! * a removed key or table disappears together with the comment on its line
//!   and the comment lines directly above it;
//! * lists of tables are matched **by identity** (provider name, client key,
//!   model id + alias, credential key, alias name, price pattern), then by
//!   equal content, then by resemblance, so adding, deleting or reordering
//!   one element leaves the text of the others byte-identical. Elements that
//!   have no identity (payload rules) fall back to their index;
//! * an inline table written over several lines is edited line by line: a
//!   new key gets a line of its own, a removed key takes its line and the
//!   comment on it, and the neighbours' lines are not touched;
//! * a table or list the user wrote inline stays inline;
//! * plain arrays are edited element-wise when values were replaced where
//!   they stand or elements were appended or removed at the end, and rewritten
//!   in their original layout (multi-line stays multi-line) otherwise.
//!
//! Because the old tree is derived from the *parsed* configuration, keys the
//! user left out (defaults) are not written unless their value changes.
//!
//! # Comments
//!
//! A comment on a key's or header's own line, and comment lines directly
//! above it, are *about* that item: they move with it and are removed with
//! it. Comment paragraphs set apart by a blank line — a section banner, the
//! text that opens the file, a commented-out example — belong to the *place*:
//! they stay where they are when the table below them is removed, moved, or
//! gets a new neighbour in front of it. In a file that has no table yet, all
//! of the text is such a place: the first table goes below it.
//!
//! When more than one line goes at once, what counts as "the item" follows
//! from what was removed:
//!
//! * An element of a list (`[[providers]]`, `[[auth.keys]]`) is removed as a
//!   block: its header, its keys and the sub-tables that follow it, with the
//!   comments between them. The paragraph that stands apart *above* the
//!   element stays.
//! * A section that merely groups tables — `payload`, whether or not it has
//!   a `[payload]` header — disappears when the last of them goes. Each of
//!   the headers below it is then treated on its own, so the banner above
//!   `[[payload.override]]` survives the last override rule.
//! * A map written with dotted keys (`headers.X-Title = "…"`) is so many
//!   lines of the table it is written in, and each of those is removed like
//!   any other key: a paragraph that stood apart above it moves in front of
//!   the next line of that table, or ends the table's body.
//!
//! # Line endings
//!
//! A file that uses Windows line endings throughout keeps them, and a leading
//! byte-order mark is kept too.
//!
//! The result is re-parsed and compared with the target. Should it differ —
//! which would be a bug in the merge — or should the existing text not be a
//! readable configuration at all, the file is serialised from scratch
//! instead, under a header comment saying so. A wrong file is never written.

use crate::validate::strip_bom;
use serde_json::{Map, Value as Json};
use std::collections::HashMap;
use switchyard_core::Config;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, RawString, Table, Value};

/// First lines of a file that had to be serialised from scratch.
pub const REWRITE_HEADER: &str = "# Switchyard configuration.\n\
# This file was rewritten from scratch by Switchyard because the previous\n\
# version could not be edited in place; its comments and layout were not kept.\n\n";

/// How [`render_update`] produced its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// The existing text already describes the configuration; it is returned
    /// byte for byte.
    Unchanged,
    /// The differences were merged into the existing text.
    Merged,
    /// The file was serialised from scratch (formatting lost).
    Rewritten,
}

/// Output of [`render_update`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The text to write to the configuration file.
    pub text: String,
    pub strategy: Strategy,
}

/// Produces the file text that stores `new`, changing as little of `text` as
/// possible. See the [module documentation](self).
///
/// Fails only when `new` cannot be represented in TOML at all (a JSON `null`
/// or an integer beyond 64 bits inside a payload rule, for example). The
/// error message names the problem and contains no configuration values.
pub fn render_update(text: &str, new: &Config) -> Result<Rendered, String> {
    let new_tree = tree_of(new)?;
    match merge(text, &new_tree) {
        Ok(None) => {
            return Ok(Rendered {
                text: text.to_string(),
                strategy: Strategy::Unchanged,
            });
        }
        Ok(Some(merged)) => {
            if tree_of_text(&merged).as_ref() == Some(&new_tree) {
                return Ok(Rendered {
                    text: merged,
                    strategy: Strategy::Merged,
                });
            }
            tracing::warn!(
                "merging the configuration change into the existing file would not have \
                 produced the intended configuration; rewriting the file from scratch"
            );
        }
        Err(MergeError::Unreadable) => {
            // Reported by the caller, which knows whether the text was expected to be readable.
        }
        Err(MergeError::Mismatch) => {
            tracing::warn!(
                "the configuration file's structure does not match its parsed content; \
                 rewriting the file from scratch"
            );
        }
        Err(MergeError::Unrepresentable(what)) => return Err(what),
    }

    let body = new
        .to_toml()
        .map_err(|e| format!("the configuration cannot be written as TOML: {e}"))?;
    let rewritten = format!("{REWRITE_HEADER}{body}");
    if tree_of_text(&rewritten).as_ref() != Some(&new_tree) {
        return Err(
            "the configuration cannot be written as TOML without changing its meaning".to_string(),
        );
    }
    Ok(Rendered {
        text: rewritten,
        strategy: Strategy::Rewritten,
    })
}

/// The value tree of a configuration: what its TOML serialisation contains,
/// in field order.
fn tree_of(config: &Config) -> Result<Map<String, Json>, String> {
    match serde_json::to_value(config) {
        Ok(Json::Object(map)) => Ok(map),
        Ok(_) => Err("the configuration did not serialise to a table".to_string()),
        Err(e) => Err(format!("the configuration cannot be serialised: {e}")),
    }
}

/// The value tree of configuration text, or `None` when the text is not a
/// readable configuration (syntax or schema error).
fn tree_of_text(text: &str) -> Option<Map<String, Json>> {
    let config: Config = toml::from_str(strip_bom(text)).ok()?;
    tree_of(&config).ok()
}

/// Whether text can serve as the base of a merge: it is TOML and matches the
/// schema (it need not pass semantic validation).
pub(crate) fn is_readable(text: &str) -> bool {
    tree_of_text(text).is_some()
}

#[derive(Debug)]
enum MergeError {
    /// The existing text is not a readable configuration.
    Unreadable,
    /// The document and the value tree parsed from the same text disagree
    /// about structure. Cannot happen unless the two parsers differ.
    Mismatch,
    /// A value of the new configuration has no TOML representation.
    Unrepresentable(String),
}

/// Merges `new` into `text`. `Ok(None)` means the text already says `new`.
fn merge(text: &str, new: &Map<String, Json>) -> Result<Option<String>, MergeError> {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let old = tree_of_text(body).ok_or(MergeError::Unreadable)?;
    if old == *new {
        return Ok(None);
    }
    let mut doc: DocumentMut = body.parse().map_err(|_| MergeError::Unreadable)?;

    let mut merger = Merger::default();
    let mut path = Vec::new();
    merger.merge_table(doc.as_table_mut(), &old, new, &mut path, false)?;
    layout_tables(&mut doc, merger.orphans);

    let mut out = doc.to_string();
    // toml_edit drops carriage returns when it prints; put them back for a
    // file that used Windows line endings throughout.
    if uses_crlf(body) {
        out = out.replace('\n', "\r\n");
    }
    if bom {
        out.insert(0, '\u{feff}');
    }
    Ok(Some(out))
}

fn uses_crlf(text: &str) -> bool {
    let newlines = text.matches('\n').count();
    newlines > 0 && text.matches("\r\n").count() == newlines
}

// ---------------------------------------------------------------------------
// Structural merge
// ---------------------------------------------------------------------------

/// A comment that lost the item it preceded but is not about that item (it
/// was separated from it by a blank line), remembered so it can be kept.
struct Orphan {
    /// Document position of the table the comment belonged to, or whose body
    /// it ended.
    position: isize,
    text: String,
}

#[derive(Default)]
struct Merger {
    orphans: Vec<Orphan>,
}

type Path = Vec<String>;

impl Merger {
    /// Applies the difference between `old` and `new` to a `[table]`.
    ///
    /// `in_element` is true inside an element of a list of tables, where new
    /// maps are written inline rather than as `[a.b.c]` headers.
    ///
    /// Returns the comments that outlived the keys they stood above and have
    /// no key left to stand above in this table. Only a table of dotted keys
    /// (`a.b = 1`) returns any: its keys are lines of the table they are
    /// written in, which is where such a comment has to stay. A table with a
    /// header keeps them itself, as the text that ends its body.
    fn merge_table(
        &mut self,
        table: &mut Table,
        old: &Map<String, Json>,
        new: &Map<String, Json>,
        path: &mut Path,
        in_element: bool,
    ) -> Result<Option<String>, MergeError> {
        // Tables that stand for dotted keys (`a.b = 1`) cannot hold headers.
        let headers_allowed = !table.is_dotted();
        let mut left_over: Option<String> = None;

        for key in old.keys() {
            if !new.contains_key(key) {
                let kept = self.remove_key(table, key);
                keep_comment(&mut left_over, kept);
            }
        }

        for (key, new_value) in new {
            let old_value = old.get(key);
            if old_value == Some(new_value) {
                continue;
            }
            path.push(key.clone());
            let result = match table.get_mut(key) {
                Some(item) => self
                    .merge_item(
                        item,
                        old_value,
                        new_value,
                        path,
                        in_element,
                        headers_allowed,
                    )
                    .map(|from_dotted_keys| {
                        // What a group of dotted keys could not keep goes in
                        // front of the line that follows the group.
                        let kept = from_dotted_keys.and_then(|text| place_after(table, key, text));
                        keep_comment(&mut left_over, kept);
                    }),
                None => self
                    .absent_item(old_value, new_value, path, in_element, headers_allowed)
                    .map(|item| {
                        table.insert(key, item);
                    }),
            };
            path.pop();
            result?;
        }

        if table.is_dotted() {
            return Ok(left_over);
        }
        if let Some(text) = left_over {
            // The comment now ends the table's body, i.e. it precedes the
            // next header. The root table's body precedes every header.
            self.orphans.push(Orphan {
                position: table.position().unwrap_or(0),
                text,
            });
        }
        Ok(None)
    }

    /// Builds the item for a key the document does not have.
    fn absent_item(
        &mut self,
        old: Option<&Json>,
        new: &Json,
        path: &mut Path,
        in_element: bool,
        headers_allowed: bool,
    ) -> Result<Item, MergeError> {
        // The key is missing from the file yet present in the old tree: the
        // file relies on defaults for this whole table. Write only what
        // differs from them, not the entire table.
        if let (Some(Json::Object(old)), Json::Object(new)) = (old, new) {
            if headers_allowed && !in_element {
                let mut table = new_section();
                // A table made here holds no comments to keep.
                self.merge_table(&mut table, old, new, path, false)?;
                return Ok(Item::Table(table));
            }
            let mut table = InlineTable::new();
            self.merge_inline(&mut table, old, new, path)?;
            return Ok(Item::Value(Value::InlineTable(table)));
        }
        new_item(new, path, in_element, headers_allowed)
    }

    /// Like [`merge_table`](Self::merge_table), returns the comments a group
    /// of dotted keys could not keep.
    fn merge_item(
        &mut self,
        item: &mut Item,
        old: Option<&Json>,
        new: &Json,
        path: &mut Path,
        in_element: bool,
        headers_allowed: bool,
    ) -> Result<Option<String>, MergeError> {
        match (&mut *item, new) {
            (Item::Table(table), Json::Object(new)) => {
                let empty = Map::new();
                let old = old.and_then(Json::as_object).unwrap_or(&empty);
                self.merge_table(table, old, new, path, in_element)
            }
            (Item::ArrayOfTables(array), Json::Array(new)) if is_table_list(new) => {
                let old = old.and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                self.merge_array_of_tables(array, old, new, path)?;
                Ok(None)
            }
            (Item::Value(value), _) => {
                self.merge_value(value, old, new, path)?;
                Ok(None)
            }
            _ => {
                // The kind of item changed (cannot happen with the schema as
                // it is, where every key has one type): replace it wholesale.
                let kept = self.note_removed_item(item);
                *item = new_item(new, path, in_element, headers_allowed)?;
                Ok(kept)
            }
        }
    }

    /// `[[list]]` elements are matched by identity, so every element that
    /// survives keeps its text, wherever it ends up.
    fn merge_array_of_tables(
        &mut self,
        array: &mut ArrayOfTables,
        old: &[Json],
        new: &[Json],
        path: &mut Path,
    ) -> Result<(), MergeError> {
        if array.len() != old.len() || !old.iter().all(Json::is_object) {
            return Err(MergeError::Mismatch);
        }
        let matches = match_elements(path, old, new);
        let mut tables: Vec<Option<Table>> = std::mem::take(array).into_iter().map(Some).collect();
        let mut merged = ArrayOfTables::new();
        for (new_element, matched) in new.iter().zip(&matches) {
            let Json::Object(new_map) = new_element else {
                return Err(MergeError::Mismatch);
            };
            let table = match matched {
                Some(i) => {
                    let mut table = tables
                        .get_mut(*i)
                        .and_then(Option::take)
                        .ok_or(MergeError::Mismatch)?;
                    let old_map = old[*i].as_object().ok_or(MergeError::Mismatch)?;
                    // An element has a header, so it keeps its own comments.
                    self.merge_table(&mut table, old_map, new_map, path, true)?;
                    table
                }
                None => new_table(new_map, path, true)?,
            };
            merged.push(table);
        }
        for removed in tables.iter().flatten() {
            self.note_removed_table(removed, true, None);
        }
        *array = merged;
        Ok(())
    }

    fn merge_value(
        &mut self,
        value: &mut Value,
        old: Option<&Json>,
        new: &Json,
        path: &mut Path,
    ) -> Result<(), MergeError> {
        match (&mut *value, new) {
            (Value::InlineTable(table), Json::Object(new)) => {
                let empty = Map::new();
                let old = old.and_then(Json::as_object).unwrap_or(&empty);
                self.merge_inline(table, old, new, path)
            }
            (Value::Array(array), Json::Array(new)) => {
                let old = old.and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                self.merge_array(array, old, new, path)
            }
            _ => {
                let replacement = new_value(new, path)?;
                replace_value(value, replacement);
                Ok(())
            }
        }
    }

    fn merge_inline(
        &mut self,
        table: &mut InlineTable,
        old: &Map<String, Json>,
        new: &Map<String, Json>,
        path: &mut Path,
    ) -> Result<(), MergeError> {
        self.merge_inline_at(table, &mut Vec::new(), old, new, path)
    }

    /// Merges the keys found at `at` inside the inline table `root`: `root`
    /// itself when `at` is empty, the keys written `a.x = …` when it is
    /// `["a"]`. Dotted keys share the lines, commas and comments of the table
    /// they are written in, so they are edited through it.
    fn merge_inline_at(
        &mut self,
        root: &mut InlineTable,
        at: &mut Vec<String>,
        old: &Map<String, Json>,
        new: &Map<String, Json>,
        path: &mut Path,
    ) -> Result<(), MergeError> {
        // Additions come before removals: a table whose keys are all replaced
        // then never passes through `{}`, which would lose its layout.
        for (key, new_value_json) in new {
            let old_value = old.get(key);
            if old_value == Some(new_value_json) {
                continue;
            }
            path.push(key.clone());
            at.push(key.clone());
            let existing = inline_value(root, at);
            let present = existing.is_some();
            let dotted = matches!(
                existing,
                Some(Value::InlineTable(inner)) if inner.is_dotted()
            );
            let result = if dotted && let Json::Object(new) = new_value_json {
                let empty = Map::new();
                let old = old_value.and_then(Json::as_object).unwrap_or(&empty);
                self.merge_inline_at(root, at, old, new, path)
            } else if present {
                match inline_value_mut(root, at) {
                    Some(value) => self.merge_value(value, old_value, new_value_json, path),
                    None => Err(MergeError::Mismatch),
                }
            } else {
                let built = match (old_value, new_value_json) {
                    // Relying on defaults so far: write only the difference.
                    (Some(Json::Object(old)), Json::Object(new)) => {
                        let mut inner = InlineTable::new();
                        self.merge_inline(&mut inner, old, new, path)
                            .map(|()| Value::InlineTable(inner))
                    }
                    _ => new_value(new_value_json, path),
                };
                built.map(|value| insert_inline(root, at, value))
            };
            at.pop();
            path.pop();
            result?;
        }
        for key in old.keys() {
            if !new.contains_key(key) {
                at.push(key.clone());
                remove_inline(root, at);
                at.pop();
            }
        }
        Ok(())
    }

    fn merge_array(
        &mut self,
        array: &mut Array,
        old: &[Json],
        new: &[Json],
        path: &mut Path,
    ) -> Result<(), MergeError> {
        if array.len() != old.len() {
            return Err(MergeError::Mismatch);
        }
        let inline_tables = array.iter().all(Value::is_inline_table);
        if is_table_list(new) && inline_tables && (old.is_empty() || is_table_list(old)) {
            return self.merge_inline_table_array(array, old, new, path);
        }

        // Values swapped for new ones at the same places (a rotated key, a
        // corrected pattern): replace them where they stand, comments and
        // quoting included. Elements that merely moved do not count — a
        // comment must not end up beside a different element.
        let substitution = old.len() == new.len()
            && old
                .iter()
                .zip(new)
                .all(|(o, n)| o == n || (!new.contains(o) && !old.contains(n)));
        if substitution {
            for (i, (o, n)) in old.iter().zip(new).enumerate() {
                if o != n {
                    let replacement = new_value(n, path)?;
                    let target = array.get_mut(i).ok_or(MergeError::Mismatch)?;
                    replace_value(target, replacement);
                }
            }
            return Ok(());
        }

        let common = old.iter().zip(new).take_while(|(o, n)| o == n).count();
        if common == old.len() {
            for element in &new[common..] {
                let value = new_value(element, path)?;
                push_element(array, value);
            }
        } else if common == new.len() {
            for _ in new.len()..old.len() {
                pop_element(array);
            }
        } else {
            let values = new
                .iter()
                .map(|element| new_value(element, path))
                .collect::<Result<Vec<_>, _>>()?;
            relayout(array, values);
        }
        Ok(())
    }

    /// A list of tables the user wrote as an inline array
    /// (`providers = [{ … }, { … }]`) keeps that form.
    fn merge_inline_table_array(
        &mut self,
        array: &mut Array,
        old: &[Json],
        new: &[Json],
        path: &mut Path,
    ) -> Result<(), MergeError> {
        let matches = match_elements(path, old, new);
        let kept = old.len().min(new.len());
        let aligned = matches
            .iter()
            .enumerate()
            .all(|(j, m)| if j < kept { *m == Some(j) } else { m.is_none() });
        if aligned {
            // Nothing moves — elements are edited where they are, added at
            // the end or dropped from the end — so every comment and line
            // break can stay.
            for j in 0..kept {
                if old[j] != new[j] {
                    let value = array.get_mut(j).ok_or(MergeError::Mismatch)?;
                    self.merge_value(value, Some(&old[j]), &new[j], path)?;
                }
            }
            for element in &new[kept..] {
                let value = new_value(element, path)?;
                push_element(array, value);
            }
            for _ in new.len()..old.len() {
                pop_element(array);
            }
            return Ok(());
        }

        let mut values: Vec<Option<Value>> = array.iter().cloned().map(Some).collect();
        let mut merged = Vec::with_capacity(new.len());
        for (new_element, matched) in new.iter().zip(&matches) {
            let value = match matched {
                Some(i) => {
                    let mut value = values
                        .get_mut(*i)
                        .and_then(Option::take)
                        .ok_or(MergeError::Mismatch)?;
                    self.merge_value(&mut value, Some(&old[*i]), new_element, path)?;
                    value
                }
                None => new_value(new_element, path)?,
            };
            merged.push(value);
        }
        relayout(array, merged);
        Ok(())
    }

    /// Removes a key from a `[table]`, keeping comments that are not about it.
    ///
    /// A kept comment goes in front of the key that is printed next. When no
    /// key follows in `table` it is returned, for the caller to place (see
    /// [`merge_table`](Self::merge_table)).
    fn remove_key(&mut self, table: &mut Table, key: &str) -> Option<String> {
        let Some((key_ref, item)) = table.get_key_value(key) else {
            // Also drops a placeholder left by toml_edit, if any.
            table.remove(key);
            return None;
        };
        let kept = match item {
            Item::Value(_) => raw(key_ref.leaf_decor().prefix()).and_then(detached_comment),
            other => self.note_removed_item(other),
        };
        let left_over = kept.and_then(|text| place_after(table, key, text));
        table.remove(key);
        left_over
    }

    /// Remembers the comments that outlive a removed table, a removed list
    /// of tables or a removed group of dotted keys.
    ///
    /// Comments that go by the document's header order are filed as
    /// [`Orphan`]s. Comments among the *keys* of the enclosing table — which
    /// is where a group of dotted keys is written — are returned instead.
    fn note_removed_item(&mut self, item: &Item) -> Option<String> {
        match item {
            // `a.b = 1`, `a.c = 2`: so many key lines of the enclosing
            // table, each removed like any other key. A comment paragraph that
            // stands apart above one of them stays.
            Item::Table(table) if table.is_dotted() => {
                let mut kept: Option<String> = None;
                self.note_removed_dotted_keys(table, &mut kept);
                kept
            }
            Item::Table(table) => {
                self.note_removed_table(table, false, None);
                None
            }
            Item::ArrayOfTables(array) => {
                for table in array.iter() {
                    self.note_removed_table(table, true, None);
                }
                None
            }
            Item::Value(_) | Item::None => None,
        }
    }

    /// Walks the keys of a removed group of dotted keys in the order they are
    /// printed. `kept` collects the comments that stay.
    fn note_removed_dotted_keys(&mut self, table: &Table, kept: &mut Option<String>) {
        for (key, item) in table.iter() {
            match item {
                Item::Value(_) => {
                    let own = table
                        .key(key)
                        .and_then(|k| raw(k.leaf_decor().prefix()))
                        .and_then(detached_comment);
                    keep_comment(kept, own);
                }
                Item::Table(inner) if inner.is_dotted() => {
                    self.note_removed_dotted_keys(inner, kept);
                }
                // The parser does not put headers below dotted keys.
                other => {
                    let own = self.note_removed_item(other);
                    keep_comment(kept, own);
                }
            }
        }
    }

    /// A removed `[header]` takes the comment lines directly above it along;
    /// earlier paragraphs (a section banner, the file's introduction, a
    /// commented-out example) stay.
    ///
    /// What else goes depends on what was removed:
    ///
    /// * An **element of a list** (`[[providers]]`, `[[auth.keys]]`) is one
    ///   thing, deleted as one: its block — the header, the keys, and the
    ///   sub-tables that follow it without another table in between — goes,
    ///   with every comment in it. `block` is that range of positions while
    ///   the tables below a removed element are visited.
    /// * A **section** (`payload`, with or without a `[payload]` header of
    ///   its own) merely groups what is below it, and disappears because that
    ///   became empty. Every header below it is treated on its own, so the
    ///   banner above `[[payload.override]]` stays when the last rule goes.
    ///
    /// What stands between the *keys* of a removed table is part of that
    /// table either way.
    fn note_removed_table(&mut self, table: &Table, element: bool, block: Option<(isize, isize)>) {
        let mut block = block;
        // A table without a position has no header (a parent that only groups
        // sub-tables) or no parsed text at all.
        if let Some(position) = table.position() {
            let inside = block.is_some_and(|(start, end)| start < position && position <= end);
            if !inside {
                if let Some(text) = raw(table.decor().prefix()).and_then(detached_comment) {
                    self.orphans.push(Orphan { position, text });
                }
                if element {
                    block = Some((position, block_end(table, position)));
                }
            }
        }
        for (_, child) in table.iter() {
            match child {
                Item::Table(inner) => self.note_removed_table(inner, false, block),
                Item::ArrayOfTables(array) => {
                    for inner in array.iter() {
                        self.note_removed_table(inner, true, block);
                    }
                }
                Item::Value(_) | Item::None => {}
            }
        }
    }
}

/// The position of the last header in the block of the list element at
/// `position`: its sub-tables count for as long as they follow it without a
/// header of another table in between. (TOML also allows
/// `[[providers.models]]` further down the file, after other sections; such
/// a header is not part of the block.)
fn block_end(element: &Table, position: isize) -> isize {
    fn positions(table: &Table, out: &mut Vec<isize>) {
        for (_, child) in table.iter() {
            match child {
                Item::Table(inner) => {
                    out.extend(inner.position());
                    positions(inner, out);
                }
                Item::ArrayOfTables(array) => {
                    for inner in array.iter() {
                        out.extend(inner.position());
                        positions(inner, out);
                    }
                }
                Item::Value(_) | Item::None => {}
            }
        }
    }
    let mut below = Vec::new();
    positions(element, &mut below);
    below.sort_unstable();
    let mut end = position;
    for p in below {
        if p == end + 1 {
            end = p;
        }
    }
    end
}

/// Adds a kept comment to those kept so far.
fn keep_comment(kept: &mut Option<String>, more: Option<String>) {
    if let Some(more) = more {
        *kept = Some(match kept.take() {
            Some(earlier) => join_comments(&earlier, &more),
            None => more,
        });
    }
}

/// The keys leading from `item` to the first value printed for it: none for
/// a value, the path to the first one for a group of dotted keys. `None` for
/// anything that is printed under a header of its own.
fn first_entry(item: &Item) -> Option<Vec<String>> {
    match item {
        Item::Value(_) => Some(Vec::new()),
        Item::Table(table) if table.is_dotted() => table.iter().find_map(|(key, child)| {
            let mut path = first_entry(child)?;
            path.insert(0, key.to_string());
            Some(path)
        }),
        _ => None,
    }
}

/// Puts a kept comment in front of the first key printed after `key` in
/// `table`. Hands the comment back when no key follows.
fn place_after(table: &mut Table, key: &str, text: String) -> Option<String> {
    let next = table
        .iter()
        .skip_while(|(k, _)| *k != key)
        .skip(1)
        .find_map(|(k, item)| {
            let mut path = first_entry(item)?;
            path.insert(0, k.to_string());
            Some(path)
        });
    let Some((leaf, parents)) = next.as_deref().and_then(<[String]>::split_last) else {
        return Some(text);
    };
    let mut holder = table;
    for parent in parents {
        match holder.get_mut(parent).and_then(Item::as_table_mut) {
            Some(inner) => holder = inner,
            None => return Some(text),
        }
    }
    let Some(mut next_key) = holder.key_mut(leaf) else {
        return Some(text);
    };
    let existing = raw(next_key.leaf_decor().prefix())
        .unwrap_or_default()
        .to_string();
    next_key
        .leaf_decor_mut()
        .set_prefix(join_comments(&text, &existing));
    None
}

// ---------------------------------------------------------------------------
// Matching list elements
// ---------------------------------------------------------------------------

/// The value that identifies an element of a known list of tables.
fn identity(path: &[String], element: &Json) -> Option<String> {
    let field = |name: &str| {
        element
            .get(name)
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
    };
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["providers"] | ["aliases"] => field("name").map(str::to_string),
        ["auth", "keys"] => field("key").map(str::to_string),
        ["providers", "models"] => {
            field("id").map(|id| format!("{id}\u{1f}{}", field("alias").unwrap_or("")))
        }
        ["providers", "credentials"] => field("api_key")
            .map(|v| format!("key\u{1f}{v}"))
            .or_else(|| field("service_account_file").map(|v| format!("file\u{1f}{v}")))
            .or_else(|| field("label").map(|v| format!("label\u{1f}{v}"))),
        ["pricing"] => field("model").map(str::to_string),
        _ => None,
    }
}

/// For every element of `new`, the index of the element of `old` it
/// continues, if any.
///
/// 1. Elements with the same identity (when that identity is unique on both
///    sides) are the same element.
/// 2. Elements that are exactly equal are the same element. This covers lists
///    without an identity (payload rules) and duplicates.
/// 3. What remains is paired by resemblance: an old element sharing at least
///    half of its fields with the new one — an edit in place, such as a
///    renamed provider. Ties go to the nearest index.
/// 4. An element that is still unpaired continues the element at the same
///    index when either of the two has no unique identity.
///
/// Unmatched new elements are insertions, unmatched old ones deletions.
fn match_elements(path: &[String], old: &[Json], new: &[Json]) -> Vec<Option<usize>> {
    let mut result: Vec<Option<usize>> = vec![None; new.len()];
    let mut used = vec![false; old.len()];

    let old_ids: Vec<Option<String>> = old.iter().map(|e| identity(path, e)).collect();
    let new_ids: Vec<Option<String>> = new.iter().map(|e| identity(path, e)).collect();
    let count = |ids: &[Option<String>]| {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for id in ids.iter().flatten() {
            *counts.entry(id.clone()).or_default() += 1;
        }
        counts
    };
    let (old_counts, new_counts) = (count(&old_ids), count(&new_ids));
    for (j, id) in new_ids.iter().enumerate() {
        let Some(id) = id else { continue };
        if new_counts.get(id) != Some(&1) || old_counts.get(id) != Some(&1) {
            continue;
        }
        if let Some(i) = old_ids.iter().position(|o| o.as_ref() == Some(id)) {
            result[j] = Some(i);
            used[i] = true;
        }
    }

    for (j, element) in new.iter().enumerate() {
        if result[j].is_some() {
            continue;
        }
        if let Some(i) = (0..old.len()).find(|&i| !used[i] && old[i] == *element) {
            result[j] = Some(i);
            used[i] = true;
        }
    }

    for (j, element) in new.iter().enumerate() {
        if result[j].is_some() {
            continue;
        }
        let best = (0..old.len())
            .filter(|&i| !used[i])
            .filter_map(|i| {
                let (shared, total) = resemblance(&old[i], element);
                // At least half of the fields agree.
                (total > 0 && shared * 2 >= total).then_some((i, shared, total))
            })
            .max_by(|a, b| {
                // Higher share first; then the closer index.
                (a.1 * b.2)
                    .cmp(&(b.1 * a.2))
                    .then_with(|| b.0.abs_diff(j).cmp(&a.0.abs_diff(j)))
            });
        if let Some((i, _, _)) = best {
            result[j] = Some(i);
            used[i] = true;
        }
    }

    // Elements with no identity of their own — payload rules, a credential
    // that is nothing but a weight — fall back to their index: a rule edited
    // beyond recognition is still "the first rule", and keeps its comments.
    // The same goes for an element that only now gains an identity (a keyless
    // credential is given a label) or loses it. Where both sides have one and
    // they differ, nothing is paired: a different provider at the same index
    // is a different provider, and the comments of the old one do not
    // describe it.
    let anonymous = |ids: &[Option<String>], counts: &HashMap<String, usize>, index: usize| {
        ids[index]
            .as_ref()
            .is_none_or(|id| counts.get(id) != Some(&1))
    };
    for j in 0..new.len().min(old.len()) {
        if result[j].is_none()
            && !used[j]
            && (anonymous(&new_ids, &new_counts, j) || anonymous(&old_ids, &old_counts, j))
        {
            result[j] = Some(j);
            used[j] = true;
        }
    }
    result
}

/// Number of fields two tables agree on, and the number of fields either has.
fn resemblance(a: &Json, b: &Json) -> (usize, usize) {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return (0, 0);
    };
    let shared = a.iter().filter(|(k, v)| b.get(*k) == Some(v)).count();
    let only_b = b.keys().filter(|k| !a.contains_key(*k)).count();
    (shared, a.len() + only_b)
}

fn is_table_list(list: &[Json]) -> bool {
    !list.is_empty() && list.iter().all(Json::is_object)
}

// ---------------------------------------------------------------------------
// Building new items
// ---------------------------------------------------------------------------

/// A `[section]` created by the merge. It is marked implicit so that its
/// header is printed only if it ends up holding values of its own.
fn new_section() -> Table {
    let mut table = Table::new();
    table.set_implicit(true);
    table
}

fn new_item(
    value: &Json,
    path: &mut Path,
    in_element: bool,
    headers_allowed: bool,
) -> Result<Item, MergeError> {
    match value {
        Json::Object(map) if headers_allowed && !in_element => {
            let mut table = new_section();
            fill_table(&mut table, map, path, false)?;
            Ok(Item::Table(table))
        }
        Json::Array(list) if headers_allowed && is_table_list(list) => {
            let mut array = ArrayOfTables::new();
            for element in list {
                let Json::Object(map) = element else {
                    return Err(MergeError::Mismatch);
                };
                array.push(new_table(map, path, true)?);
            }
            Ok(Item::ArrayOfTables(array))
        }
        other => Ok(Item::Value(new_value(other, path)?)),
    }
}

/// A new element of a list of tables.
fn new_table(
    map: &Map<String, Json>,
    path: &mut Path,
    in_element: bool,
) -> Result<Table, MergeError> {
    let mut table = Table::new();
    fill_table(&mut table, map, path, in_element)?;
    Ok(table)
}

fn fill_table(
    table: &mut Table,
    map: &Map<String, Json>,
    path: &mut Path,
    in_element: bool,
) -> Result<(), MergeError> {
    for (key, value) in map {
        path.push(key.clone());
        let item = new_item(value, path, in_element, true);
        path.pop();
        table.insert(key, item?);
    }
    Ok(())
}

fn new_value(value: &Json, path: &mut Path) -> Result<Value, MergeError> {
    let unrepresentable = |what: &str, path: &Path| {
        MergeError::Unrepresentable(format!(
            "`{}` holds {what}, which cannot be written in TOML",
            path.join(".")
        ))
    };
    Ok(match value {
        Json::Null => return Err(unrepresentable("a null value", path)),
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if n.is_u64() {
                return Err(unrepresentable(
                    "an integer beyond 64-bit signed range",
                    path,
                ));
            } else if let Some(f) = n.as_f64() {
                Value::from(f)
            } else {
                return Err(unrepresentable("an unsupported number", path));
            }
        }
        Json::String(s) => string_value(s),
        Json::Array(list) => {
            let mut array = Array::new();
            for element in list {
                array.push_formatted(new_value(element, path)?);
            }
            Value::Array(array)
        }
        Json::Object(map) => {
            let mut table = InlineTable::new();
            for (key, element) in map {
                path.push(key.clone());
                let built = new_value(element, path);
                path.pop();
                table.insert(key, built?);
            }
            Value::InlineTable(table)
        }
    })
}

/// A string value. Text with line breaks is written on one line with escapes
/// rather than as a `"""` block: a configuration value reads better that way,
/// and a block's own line breaks would be at the mercy of the file's line
/// endings.
fn string_value(text: &str) -> Value {
    if text.contains(['\n', '\r']) {
        let mut repr = String::with_capacity(text.len() + 8);
        repr.push('"');
        for c in text.chars() {
            match c {
                '"' => repr.push_str("\\\""),
                '\\' => repr.push_str("\\\\"),
                '\n' => repr.push_str("\\n"),
                '\r' => repr.push_str("\\r"),
                '\t' => repr.push_str("\\t"),
                c if c.is_control() => repr.push_str(&format!("\\u{:04X}", u32::from(c))),
                c => repr.push(c),
            }
        }
        repr.push('"');
        if let Ok(value) = repr.parse::<Value>()
            && value.as_str() == Some(text)
        {
            return value;
        }
    }
    Value::from(text)
}

/// Replaces a value, keeping the whitespace and comments around it and,
/// where possible, the quoting style of a string.
fn replace_value(target: &mut Value, mut replacement: Value) {
    if let (Value::String(old), Value::String(new)) = (&*target, &replacement) {
        let was_literal = old
            .as_repr()
            .and_then(|r| r.as_raw().as_str())
            .is_some_and(|raw| raw.starts_with('\'') && !raw.starts_with("'''"));
        let text = new.value();
        // A literal string cannot contain a quote or control characters.
        if was_literal
            && !text.contains('\'')
            && !text.chars().any(char::is_control)
            && let Ok(literal) = format!("'{text}'").parse::<Value>()
            && literal.as_str() == Some(text.as_str())
        {
            replacement = literal;
        }
    }
    let decor = target.decor().clone();
    *replacement.decor_mut() = decor;
    *target = replacement;
}

// ---------------------------------------------------------------------------
// Inline tables and arrays
// ---------------------------------------------------------------------------

fn raw(value: Option<&RawString>) -> Option<&str> {
    value.and_then(RawString::as_str)
}

// How toml_edit stores the text of an inline table written over several
// lines (TOML 1.1):
//
// ```toml
// headers = {            # after the brace
//   # about a
//   a = 1,               # on a's line
//   b = 2,               # on b's line
// }
// ```
//
// Everything between two entries — the rest of the previous entry's line,
// the comment lines above the next key and its indentation — is the *lead*
// of the next key. What follows the last entry is the table's `trailing`
// text when the entry ends with a comma, and the suffix of the last value
// otherwise. So the comment on an entry's own line is stored with its
// successor, and the first line of an entry's lead belongs to its
// predecessor. The functions below keep each comment with the line it is on.
//
// Keys written with dots (`a.b = 1`) are stored as a nested table `a` marked
// "dotted", but printed as entries of the table they are written in. An
// *entry* is therefore identified by its path (`["a", "b"]`), and the entries
// of a table are listed in the order they are printed.

/// The paths of the entries of an inline table, in the order they are
/// printed.
fn inline_entries(table: &InlineTable) -> Vec<Vec<String>> {
    let mut entries = Vec::new();
    for (key, value) in table.iter() {
        match value {
            Value::InlineTable(inner) if inner.is_dotted() => {
                for mut entry in inline_entries(inner) {
                    entry.insert(0, key.to_string());
                    entries.push(entry);
                }
            }
            _ => entries.push(vec![key.to_string()]),
        }
    }
    entries
}

/// The table that holds the key at `path`: `root`, or a dotted table in it.
fn inline_parent<'a>(root: &'a InlineTable, path: &[String]) -> Option<&'a InlineTable> {
    let (_, parents) = path.split_last()?;
    let mut table = root;
    for key in parents {
        table = table.get(key)?.as_inline_table()?;
    }
    Some(table)
}

fn inline_parent_mut<'a>(
    root: &'a mut InlineTable,
    path: &[String],
) -> Option<&'a mut InlineTable> {
    let (_, parents) = path.split_last()?;
    let mut table = root;
    for key in parents {
        table = table.get_mut(key)?.as_inline_table_mut()?;
    }
    Some(table)
}

fn inline_value<'a>(root: &'a InlineTable, path: &[String]) -> Option<&'a Value> {
    inline_parent(root, path)?.get(path.last()?)
}

fn inline_value_mut<'a>(root: &'a mut InlineTable, path: &[String]) -> Option<&'a mut Value> {
    inline_parent_mut(root, path)?.get_mut(path.last()?)
}

/// The text in front of an entry.
fn lead_of(root: &InlineTable, entry: &[String]) -> String {
    entry
        .last()
        .and_then(|leaf| inline_parent(root, entry)?.key(leaf))
        .and_then(|key| raw(key.leaf_decor().prefix()))
        // What toml_edit prints for a key without explicit decoration.
        .unwrap_or(" ")
        .to_string()
}

fn set_lead(root: &mut InlineTable, entry: &[String], lead: String) {
    if let Some(leaf) = entry.last()
        && let Some(mut key) = inline_parent_mut(root, entry).and_then(|t| t.key_mut(leaf))
    {
        key.leaf_decor_mut().set_prefix(lead);
    }
}

/// The text between an entry's value and the comma or brace that follows.
fn suffix_of(root: &InlineTable, entry: &[String]) -> String {
    inline_value(root, entry)
        .and_then(|value| raw(value.decor().suffix()))
        .unwrap_or_default()
        .to_string()
}

fn set_suffix(root: &mut InlineTable, entry: &[String], suffix: String) {
    if let Some(value) = inline_value_mut(root, entry) {
        value.decor_mut().set_suffix(suffix);
    }
}

/// The indentation of the entries of an inline table written over several
/// lines; `None` when no entry starts a line.
fn inline_indent(root: &InlineTable, entries: &[Vec<String>]) -> Option<String> {
    entries.iter().find_map(|entry| {
        let lead = lead_of(root, entry);
        lead.rsplit_once('\n').map(|(_, indent)| indent.to_string())
    })
}

/// What is worth keeping of the rest of a line: a comment, with the spaces
/// in front of it. Mere spaces before a line break are dropped.
fn rest_of_line(line: &str) -> &str {
    if line.contains('#') {
        line.trim_end_matches(['\r', ' ', '\t'])
    } else {
        ""
    }
}

/// Joins what remains on a line with what follows some other line. `line` is
/// the rest of an entry's line (spaces and possibly a comment) and `after`
/// the text whose first line is being dropped.
fn line_then(line: &str, after: &str) -> String {
    let (_, rest) = split_first_line(after);
    let kept = rest_of_line(line);
    if !rest.is_empty() {
        format!("{kept}{rest}")
    } else if !kept.is_empty() {
        // A comment runs to the end of its line, so the line must end here.
        format!("{kept}\n")
    } else {
        after.to_string()
    }
}

/// Adds the entry at `path` to an inline table, after the other entries of
/// its parent, in the table's own layout.
fn insert_inline(root: &mut InlineTable, path: &[String], mut value: Value) {
    let Some((key, parent)) = path.split_last() else {
        return;
    };
    let entries = inline_entries(root);
    let attach = |root: &mut InlineTable, value: Value, lead: Option<String>| {
        if let Some(table) = inline_parent_mut(root, path) {
            table.insert(key, value);
        }
        if let Some(lead) = lead {
            set_lead(root, path, lead);
        }
    };
    // The entry the new one is printed after.
    let Some(after) = entries.iter().rposition(|entry| entry.starts_with(parent)) else {
        if entries.is_empty() {
            // `{ }`, `{\n}`: the padding of an empty table would end up on
            // one side of the new entry only.
            root.set_trailing("");
            root.set_trailing_comma(false);
        }
        attach(root, value, None);
        return;
    };
    let indent = inline_indent(root, &entries);
    value.decor_mut().set_prefix(" ");

    if let Some(next) = entries.get(after + 1) {
        // In the middle (a dotted key joining its siblings).
        value.decor_mut().set_suffix("");
        let next_lead = lead_of(root, next);
        let lead = match (&indent, next_lead.contains('\n')) {
            (Some(indent), true) => {
                // The first line of the next entry's lead is the rest of the
                // line the new entry goes below.
                let (line, rest) = split_first_line(&next_lead);
                let lead = format!("{}\n{indent}", rest_of_line(line));
                set_lead(root, next, rest.to_string());
                Some(lead)
            }
            _ => None,
        };
        attach(root, value, lead);
        return;
    }

    let last = &entries[after];
    if root.trailing_comma() {
        value.decor_mut().set_suffix("");
        let lead = indent.map(|indent| {
            // The new entry gets a line of its own, below the last one and
            // the comment that line carries.
            let trailing = raw(Some(root.trailing())).unwrap_or_default().to_string();
            let (line, rest) = split_first_line(&trailing);
            if !rest.is_empty() {
                root.set_trailing(rest);
            }
            format!("{}\n{indent}", rest_of_line(line))
        });
        attach(root, value, lead);
    } else {
        // The text before the closing brace belongs to the last value, so it
        // moves to the new one.
        let suffix = suffix_of(root, last);
        set_suffix(root, last, String::new());
        let lead = match indent {
            Some(indent) => {
                let (line, rest) = split_first_line(&suffix);
                value.decor_mut().set_suffix(if rest.is_empty() {
                    suffix.as_str()
                } else {
                    rest
                });
                Some(format!("{}\n{indent}", rest_of_line(line)))
            }
            None => {
                value.decor_mut().set_suffix(suffix.as_str());
                None
            }
        };
        attach(root, value, lead);
    }
}

/// Removes the entry at `path` — or, for a dotted table, all the entries
/// below it — together with the comment on its line and the comment lines
/// above it, leaving the neighbours' lines as they are.
fn remove_inline(root: &mut InlineTable, path: &[String]) {
    let Some(key) = path.last() else {
        return;
    };
    let entries = inline_entries(root);
    let covered: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].starts_with(path))
        .collect();
    let detach = |root: &mut InlineTable| {
        if let Some(table) = inline_parent_mut(root, path) {
            table.remove(key);
        }
    };
    let (Some(&first), Some(&last)) = (covered.first(), covered.last()) else {
        // Nothing is printed for it (an empty dotted table).
        detach(root);
        return;
    };
    let own_lead = lead_of(root, &entries[first]);
    let own_suffix = suffix_of(root, &entries[last]);
    // What is left of the previous entry's line (or of the opening brace's).
    let (before, _) = split_first_line(&own_lead);
    detach(root);

    // `a.b = 1` was the only key written under `a`, and `a` itself stays
    // (as an empty table): write it as `a = {}` where the entry was.
    if let Some((_, parent)) = path.split_last()
        && !parent.is_empty()
        && let Some(Value::InlineTable(table)) = inline_value_mut(root, parent)
        && table.is_dotted()
        && table.is_empty()
    {
        table.set_dotted(false);
        table.decor_mut().set_prefix(" ");
        table.decor_mut().set_suffix("");
        set_lead(root, parent, own_lead);
        // The comment on that line was about the removed key.
        if let Some(next) = entries.get(last + 1) {
            let next_lead = lead_of(root, next);
            if next_lead.contains('\n') {
                set_lead(root, next, line_then("", &next_lead));
            }
        } else if root.trailing_comma() {
            let trailing = raw(Some(root.trailing())).unwrap_or_default().to_string();
            root.set_trailing(line_then("", &trailing));
        } else {
            set_suffix(root, parent, line_then("", &own_suffix));
        }
        return;
    }

    if let Some(next) = entries.get(last + 1) {
        let next_lead = lead_of(root, next);
        let lead = if next_lead.contains('\n') {
            // The removed entry's line ends inside the next key's lead: its
            // first line (the removed entry's comment) goes, the rest stays.
            line_then(before, &next_lead)
        } else {
            // The next entry shared the removed entry's line and takes its
            // place on it.
            match own_lead.rfind('\n') {
                Some(line_start) => format!("{}{}", rest_of_line(before), &own_lead[line_start..]),
                None => own_lead.clone(),
            }
        };
        set_lead(root, next, lead);
        return;
    }
    let Some(previous) = first.checked_sub(1).map(|i| &entries[i]) else {
        // The table is empty now; a comment after the opening brace stays.
        root.set_trailing_comma(false);
        let kept = rest_of_line(before);
        root.set_trailing(if kept.is_empty() {
            String::new()
        } else {
            format!("{kept}\n")
        });
        return;
    };
    if root.trailing_comma() {
        let trailing = raw(Some(root.trailing())).unwrap_or_default().to_string();
        root.set_trailing(line_then(before, &trailing));
    } else {
        let kept = suffix_of(root, previous);
        let closing = line_then(before, &own_suffix);
        set_suffix(root, previous, format!("{kept}{closing}"));
    }
}

/// Splits decoration at its first line break: what shares the line with the
/// preceding token, and the rest (starting with the line break).
fn split_first_line(text: &str) -> (&str, &str) {
    match text.find('\n') {
        Some(i) => text.split_at(i),
        None => (text, ""),
    }
}

/// The indentation of a multi-line array's elements, or `None` for an array
/// written on one line.
fn element_indent(array: &Array) -> Option<String> {
    array.iter().find_map(|value| {
        let prefix = raw(value.decor().prefix())?;
        let (_, indent) = prefix.rsplit_once('\n')?;
        Some(indent.to_string())
    })
}

/// The text between the last element and `]` (minus a trailing comma).
fn closing_of(array: &Array) -> String {
    let tail = if array.trailing_comma() || array.is_empty() {
        raw(Some(array.trailing())).unwrap_or_default()
    } else {
        array
            .iter()
            .last()
            .and_then(|v| raw(v.decor().suffix()))
            .unwrap_or_default()
    };
    match tail.rsplit_once('\n') {
        Some((_, indent)) => format!("\n{indent}"),
        None => String::new(),
    }
}

/// Appends an element in the array's own style.
fn push_element(array: &mut Array, mut value: Value) {
    let indent = element_indent(array);
    let Some(last_index) = array.len().checked_sub(1) else {
        // `[ ]`: the padding of an empty array would end up on one side only.
        if !raw(Some(array.trailing()))
            .unwrap_or_default()
            .contains('\n')
        {
            array.set_trailing("");
        }
        array.push_formatted(value);
        return;
    };

    if array.trailing_comma() {
        // `…, <trailing>]`: a comment on the last element's line sits in the
        // array's trailing text and must stay on that line.
        if let Some(indent) = indent {
            let trailing = raw(Some(array.trailing())).unwrap_or_default().to_string();
            let (same_line, rest) = split_first_line(&trailing);
            value
                .decor_mut()
                .set_prefix(format!("{same_line}\n{indent}"));
            value.decor_mut().set_suffix("");
            let rest = if rest.is_empty() { "\n" } else { rest };
            array.set_trailing(rest);
        }
    } else if let Some(last) = array.get_mut(last_index) {
        // `… <suffix>]`: the last element's suffix holds its line's comment
        // and the layout of the closing bracket.
        let suffix = raw(last.decor().suffix()).unwrap_or_default().to_string();
        let (same_line, rest) = split_first_line(&suffix);
        last.decor_mut().set_suffix("");
        match indent {
            Some(indent) => {
                value
                    .decor_mut()
                    .set_prefix(format!("{same_line}\n{indent}"));
                value
                    .decor_mut()
                    .set_suffix(if rest.is_empty() { "\n" } else { rest });
            }
            None => {
                value.decor_mut().set_prefix(" ");
                value.decor_mut().set_suffix(suffix.as_str());
            }
        }
    }
    array.push_formatted(value);
}

/// Removes the last element, handing the layout of the closing bracket to
/// the element before it.
fn pop_element(array: &mut Array) {
    let Some(last_index) = array.len().checked_sub(1) else {
        return;
    };
    let removed = array.remove(last_index);
    let removed_prefix = raw(removed.decor().prefix()).unwrap_or_default();
    let removed_suffix = raw(removed.decor().suffix()).unwrap_or_default();
    // A comment after the previous element's comma was parsed as part of the
    // removed element's prefix, but it is on the previous element's line.
    let kept_line = match removed_prefix.split_once('\n') {
        Some((same_line, _)) if !same_line.trim().is_empty() => same_line,
        _ => "",
    };

    let Some(new_last_index) = array.len().checked_sub(1) else {
        array.set_trailing_comma(false);
        array.set_trailing("");
        return;
    };
    if array.trailing_comma() {
        let trailing = raw(Some(array.trailing())).unwrap_or_default().to_string();
        let rest = match trailing.find('\n') {
            Some(i) => &trailing[i..],
            None => trailing.as_str(),
        };
        array.set_trailing(format!("{kept_line}{rest}"));
    } else if let Some(new_last) = array.get_mut(new_last_index) {
        let rest = match removed_suffix.find('\n') {
            Some(i) => &removed_suffix[i..],
            None => removed_suffix,
        };
        new_last
            .decor_mut()
            .set_suffix(format!("{kept_line}{rest}"));
    }
}

/// Replaces an array's elements, keeping its shape: one element per line
/// with the original indentation when it was multi-line, a single line
/// otherwise. Comments between elements are dropped (they described elements
/// that are gone or have moved).
fn relayout(array: &mut Array, values: Vec<Value>) {
    let indent = element_indent(array);
    let closing = closing_of(array);
    let trailing_comma = array.trailing_comma();
    let decor = array.decor().clone();

    let mut rebuilt = Array::new();
    let count = values.len();
    for (i, mut value) in values.into_iter().enumerate() {
        match &indent {
            Some(indent) => {
                value.decor_mut().set_prefix(format!("\n{indent}"));
                // Without a trailing comma the last value carries the line
                // break before `]`.
                let last_without_comma = i + 1 == count && !trailing_comma;
                value.decor_mut().set_suffix(if last_without_comma {
                    closing.as_str()
                } else {
                    ""
                });
            }
            None => value.decor_mut().clear(),
        }
        rebuilt.push_formatted(value);
    }
    if indent.is_some() && count > 0 {
        rebuilt.set_trailing_comma(trailing_comma);
        if trailing_comma {
            rebuilt.set_trailing(closing);
        }
    }
    *rebuilt.decor_mut() = decor;
    *array = rebuilt;
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

fn is_blank_line(line: &str) -> bool {
    line.trim().is_empty()
}

/// The part of an item's leading decoration that is not about the item: the
/// comment paragraphs separated from it by a blank line. `None` when there is
/// no such comment.
fn detached_comment(prefix: &str) -> Option<String> {
    let (detached, _) = split_detached(prefix);
    detached.contains('#').then(|| detached.to_string())
}

/// Splits leading decoration after its last blank line: what stands apart
/// from the item (possibly just blank lines), and the comment lines directly
/// above the item.
fn split_detached(prefix: &str) -> (&str, &str) {
    let mut end = 0;
    let mut offset = 0;
    for line in prefix.split_inclusive('\n') {
        offset += line.len();
        if line.ends_with('\n') && is_blank_line(line) {
            end = offset;
        }
    }
    prefix.split_at(end)
}

fn strip_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    while let Some(i) = rest.find('\n') {
        if !is_blank_line(&rest[..i]) {
            break;
        }
        rest = &rest[i + 1..];
    }
    rest
}

/// Puts a kept comment in front of existing decoration without doubling the
/// blank line between them.
fn join_comments(comment: &str, existing: &str) -> String {
    let ends_blank = comment.ends_with("\n\n") || comment.ends_with("\r\n\r\n");
    if ends_blank {
        format!("{comment}{}", strip_leading_blank_lines(existing))
    } else {
        format!("{comment}{existing}")
    }
}

// ---------------------------------------------------------------------------
// Table order
// ---------------------------------------------------------------------------

/// One `[header]` / `[[header]]` of the document.
struct Node {
    /// Position assigned by the parser; `None` for tables created by the
    /// merge (and for parents that never had a header of their own).
    original: Option<isize>,
    parent: usize,
    /// For elements of a list of tables: the list and the index in it.
    member: Option<(usize, usize)>,
    /// Whether a header is printed for it. A parent that only groups
    /// sub-tables (`[a.b]` without `[a]`) has none.
    visible: bool,
    /// The comments and blank lines above its header, for parsed tables.
    prefix: Option<String>,
}

/// Mirrors toml_edit's rule for printing a `[header]`.
fn has_header(table: &Table) -> bool {
    !(table.is_implicit() && table.get_values().is_empty())
}

fn collect_nodes(table: &Table, me: usize, nodes: &mut Vec<Node>, lists: &mut Vec<Vec<usize>>) {
    for (_, item) in table.iter() {
        match item {
            Item::Table(child) if child.is_dotted() => collect_nodes(child, me, nodes, lists),
            Item::Table(child) => {
                let index = nodes.len();
                nodes.push(Node {
                    original: child.position(),
                    parent: me,
                    member: None,
                    visible: has_header(child),
                    prefix: raw(child.decor().prefix()).map(str::to_string),
                });
                collect_nodes(child, index, nodes, lists);
            }
            Item::ArrayOfTables(array) => {
                let list = lists.len();
                lists.push(Vec::new());
                for (n, child) in array.iter().enumerate() {
                    let index = nodes.len();
                    nodes.push(Node {
                        original: child.position(),
                        parent: me,
                        member: Some((list, n)),
                        visible: true,
                        prefix: raw(child.decor().prefix()).map(str::to_string),
                    });
                    lists[list].push(index);
                    collect_nodes(child, index, nodes, lists);
                }
            }
            _ => {}
        }
    }
}

/// Visits the tables in the same order as [`collect_nodes`].
fn for_each_table(
    table: &mut Table,
    counter: &mut usize,
    visit: &mut dyn FnMut(usize, &mut Table),
) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(child) if child.is_dotted() => for_each_table(child, counter, visit),
            Item::Table(child) => {
                *counter += 1;
                visit(*counter, child);
                for_each_table(child, counter, visit);
            }
            Item::ArrayOfTables(array) => {
                for child in array.iter_mut() {
                    *counter += 1;
                    visit(*counter, child);
                    for_each_table(child, counter, visit);
                }
            }
            _ => {}
        }
    }
}

fn is_within(nodes: &[Node], mut node: usize, ancestor: usize) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        if node == 0 {
            return false;
        }
        node = nodes[node].parent;
    }
}

/// Index in `sequence` just past the last table of `ancestor`'s subtree.
fn end_of_subtree(nodes: &[Node], sequence: &[usize], ancestor: usize) -> usize {
    if ancestor == 0 {
        return sequence.len();
    }
    sequence
        .iter()
        .rposition(|&n| is_within(nodes, n, ancestor))
        .map_or(sequence.len(), |i| i + 1)
}

/// Decides where every table header goes and re-attaches kept comments.
///
/// toml_edit prints tables in the order of their recorded positions, not in
/// the order of the tree. After a merge that is not enough:
///
/// * elements of a list may have been reordered, and each element must be
///   followed by its own sub-tables (`[[providers.models]]` belongs to the
///   `[[providers]]` printed before it);
/// * new tables have no position at all.
///
/// So the order is computed here. Existing tables keep their relative order.
/// Reordered list elements exchange places as whole blocks. A new list
/// element goes right after the block of the element before it; a new section
/// goes after its parent's block (at the end of the file for a top-level
/// one). Then every table is numbered accordingly.
///
/// Comments directly above a header belong to that table and travel with it.
/// Comment paragraphs set apart by a blank line (a section banner, the text
/// that opens the file) belong to the place: they stay where they are when
/// tables are reordered, removed, or inserted in front of them.
fn layout_tables(doc: &mut DocumentMut, mut orphans: Vec<Orphan>) {
    let mut nodes = vec![Node {
        original: None,
        parent: 0,
        member: None,
        visible: false,
        prefix: None,
    }];
    let mut lists: Vec<Vec<usize>> = Vec::new();
    collect_nodes(doc.as_table(), 0, &mut nodes, &mut lists);

    // Existing tables in file order.
    let mut sequence: Vec<usize> = (1..nodes.len())
        .filter(|&n| nodes[n].original.is_some())
        .collect();
    sequence.sort_by_key(|&n| nodes[n].original);
    // Whether the file has any `[header]` so far. (A parent that only groups
    // sub-tables has a position but no header of its own.)
    let had_tables = sequence.iter().any(|&n| nodes[n].visible);

    // Reordered lists: the blocks swap places. Outer lists come first in
    // `lists`, so nested lists are then rearranged within their block.
    // `takes_place_of` pairs each moved element with the one whose place it
    // takes.
    let mut takes_place_of: Vec<(usize, usize)> = Vec::new();
    for members in &lists {
        let existing: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&m| nodes[m].original.is_some())
            .collect();
        if existing.is_sorted_by_key(|&m| nodes[m].original) {
            continue;
        }
        let mut places = existing.clone();
        places.sort_by_key(|&m| nodes[m].original);
        takes_place_of.extend(
            existing
                .iter()
                .copied()
                .zip(places)
                .filter(|(element, place)| element != place),
        );
        let block_of = |n: usize| existing.iter().position(|&m| is_within(&nodes, n, m));
        let slots: Vec<usize> = (0..sequence.len())
            .filter(|&i| block_of(sequence[i]).is_some())
            .collect();
        let mut blocks: Vec<Vec<usize>> = vec![Vec::new(); existing.len()];
        for &i in &slots {
            if let Some(block) = block_of(sequence[i]) {
                blocks[block].push(sequence[i]);
            }
        }
        for (slot, node) in slots.iter().zip(blocks.into_iter().flatten()) {
            sequence[*slot] = node;
        }
    }

    // New tables, parents before children, list elements in list order.
    // `in_front_of` remembers the existing table a new one was put before.
    let mut in_front_of: Vec<(usize, usize)> = Vec::new();
    for node in 1..nodes.len() {
        if nodes[node].original.is_some() {
            continue;
        }
        let before = match nodes[node].member {
            Some((_, index)) if index > 0 => None,
            // First of its list: before the first existing element.
            Some((list, index)) => lists[list][index + 1..]
                .iter()
                .find_map(|&m| sequence.iter().position(|&n| n == m)),
            // A section whose sub-tables already exist goes before them.
            None => sequence.iter().position(|&n| is_within(&nodes, n, node)),
        };
        let at = match (before, nodes[node].member) {
            (Some(at), _) => {
                in_front_of.push((node, sequence[at]));
                at
            }
            // After the block of the element before it.
            (None, Some((list, index))) if index > 0 => {
                end_of_subtree(&nodes, &sequence, lists[list][index - 1])
            }
            // A new list or section: after its parent's block.
            (None, _) => end_of_subtree(&nodes, &sequence, nodes[node].parent),
        };
        sequence.insert(at.min(sequence.len()), node);
    }

    let mut rank = vec![0isize; nodes.len()];
    for (i, &node) in sequence.iter().enumerate() {
        rank[node] = isize::try_from(i + 1).unwrap_or(isize::MAX);
    }

    // What goes above each header, for the tables where that changes.
    let mut prefixes: HashMap<usize, String> = HashMap::new();
    let original_prefix = |n: usize| nodes[n].prefix.clone().unwrap_or_default();

    // A kept comment goes in front of the table that used to follow it.
    orphans.sort_by_key(|o| o.position);
    let mut kept: HashMap<usize, String> = HashMap::new();
    let mut at_end = String::new();
    for orphan in orphans {
        let follower = (1..nodes.len())
            .filter(|&n| nodes[n].original.is_some_and(|p| p > orphan.position))
            .min_by_key(|&n| nodes[n].original);
        let target = match follower {
            Some(n) => kept.entry(n).or_default(),
            None => &mut at_end,
        };
        *target = join_comments(target, &orphan.text);
    }
    for (node, comment) in kept {
        prefixes.insert(node, join_comments(&comment, &original_prefix(node)));
    }

    // A moved list element leaves the detached comments above it where
    // they are, for the element that takes its place.
    let parts: HashMap<usize, (String, String)> = takes_place_of
        .iter()
        .flat_map(|&(element, place)| [element, place])
        .map(|n| {
            let full = prefixes
                .get(&n)
                .cloned()
                .unwrap_or_else(|| original_prefix(n));
            let (detached, attached) = split_detached(&full);
            (n, (detached.to_string(), attached.to_string()))
        })
        .collect();
    for (element, place) in takes_place_of {
        if let (Some((_, attached)), Some((detached, _))) = (parts.get(&element), parts.get(&place))
        {
            prefixes.insert(element, format!("{detached}{attached}"));
        }
    }

    // A new table placed in front of an existing one goes between that
    // table's detached comments (a section banner, the text that opens the
    // file) and the comment lines that are about the table itself.
    for (new, existing) in in_front_of {
        if !nodes[new].visible {
            continue;
        }
        let full = prefixes
            .get(&existing)
            .cloned()
            .unwrap_or_else(|| original_prefix(existing));
        if let Some(detached) = detached_comment(&full) {
            prefixes.insert(existing, format!("\n{}", &full[detached.len()..]));
            prefixes.insert(new, detached);
        }
    }

    // The header that opens the file has no blank line above it, all others
    // usually do. When a different table comes first now, the two swap that
    // property. (The parser numbers headers from 1.)
    let headers_open_file = doc.as_table().get_values().is_empty();
    let first_now = sequence.iter().copied().find(|&n| nodes[n].visible);
    let first_before = (1..nodes.len()).find(|&n| nodes[n].original == Some(1));
    if headers_open_file && first_now != first_before {
        // A table created by the merge has no decoration; toml_edit prints
        // the first one without a blank line by itself.
        if let Some(first) = first_now
            && let Some(prefix) = prefixes
                .get(&first)
                .cloned()
                .or_else(|| nodes[first].prefix.clone())
        {
            prefixes.insert(first, strip_leading_blank_lines(&prefix).to_string());
        }
        if let Some(previous) = first_before {
            let prefix = prefixes
                .get(&previous)
                .cloned()
                .unwrap_or_else(|| original_prefix(previous));
            if !prefix.starts_with('\n') && !prefix.starts_with("\r\n") {
                prefixes.insert(previous, format!("\n{prefix}"));
            }
        }
    }

    // What ends the file: the kept comments of removed tables, then the text
    // that was there already.
    let trailing_before = raw(Some(doc.trailing())).unwrap_or_default().to_string();
    let mut trailing = if at_end.is_empty() {
        trailing_before.clone()
    } else {
        join_comments(&at_end, &trailing_before)
    };

    // A file without a single table header keeps all its comments in that
    // closing text: the notes that open the file, a commented-out template.
    // toml_edit prints tables in front of it, which would push the file's
    // opening lines to the bottom. The first table goes below them instead.
    if !had_tables
        && trailing.contains('#')
        && let Some(first) = first_now
    {
        if !trailing.ends_with('\n') {
            trailing.push('\n');
        }
        // A blank line sets the comment apart from the table: it is about
        // the file, not about whichever section happens to come first.
        if !trailing.ends_with("\n\n") && !trailing.ends_with("\n\r\n") {
            trailing.push('\n');
        }
        prefixes.insert(first, std::mem::take(&mut trailing));
    }

    let mut counter = 0;
    for_each_table(doc.as_table_mut(), &mut counter, &mut |index, table| {
        if let Some(&position) = rank.get(index) {
            table.set_position(Some(position));
        }
        if let Some(prefix) = prefixes.remove(&index) {
            table.decor_mut().set_prefix(prefix);
        }
    });
    if trailing != trailing_before {
        doc.set_trailing(trailing);
    }
}
