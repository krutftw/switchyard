//! Writing the generated file: a comment header with the report, then the
//! settings that differ from the defaults.

use switchyard_config_store::validate_text;
use switchyard_core::Config;

/// Replaces control characters, so that text taken from the source file
/// (key names, the file name) can neither break out of a comment line in
/// the generated file nor send escape sequences to a terminal.
pub(super) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

pub(super) fn header(source_name: &str, not_imported: &[String], notes: &[String]) -> String {
    let mut out = format!(
        "# Switchyard configuration, imported from {} by `switchyard import-cliproxy`.\n\
         # Review it, then check it with `switchyard check`. Every setting and its\n\
         # default is documented in switchyard.example.toml.\n",
        clean(source_name)
    );
    if !not_imported.is_empty() {
        out.push_str("#\n# Not imported:\n");
        for line in not_imported {
            out.push_str(&format!("#   - {line}\n"));
        }
    }
    if !notes.is_empty() {
        out.push_str("#\n# Notes:\n");
        for line in notes {
            out.push_str(&format!("#   - {line}\n"));
        }
    }
    out.push('\n');
    out
}

/// Settings written even when they equal the default, because a reader
/// looks for them first.
const ALWAYS_WRITTEN: [&str; 2] = ["server.host", "server.port"];

/// The configuration as TOML with everything that equals its default left
/// out, so that what was imported stands out.
pub(super) fn render_toml(config: &Config) -> Result<String, String> {
    let broken = |what: &str| format!("the imported configuration cannot be written ({what})");
    let full_text = config.to_toml().map_err(|_| broken("serialising"))?;
    let default_text = Config::default()
        .to_toml()
        .map_err(|_| broken("serialising the defaults"))?;
    let full: toml::Table = toml::from_str(&full_text).map_err(|_| broken("reading back"))?;
    let defaults: toml::Table =
        toml::from_str(&default_text).map_err(|_| broken("reading the defaults back"))?;
    let mut document: toml_edit::DocumentMut = full_text
        .parse()
        .map_err(|_| broken("parsing for editing"))?;
    prune(document.as_table_mut(), &full, &defaults, "");

    // Removing entries leaves their blank lines behind. They are only
    // tidied when no multi-line string could be affected.
    let pruned = document.to_string();
    let text = if pruned.contains("\"\"\"") || pruned.contains("'''") {
        pruned
    } else {
        let mut text = String::new();
        let mut blank = true;
        for line in pruned.lines() {
            if line.trim().is_empty() {
                if !blank {
                    text.push('\n');
                }
                blank = true;
            } else {
                text.push_str(line);
                text.push('\n');
                blank = false;
            }
        }
        text.trim_end().to_string() + "\n"
    };

    // Should the pruned text not read back as the same configuration, the
    // complete one does.
    match validate_text(&text) {
        Ok(parsed) if parsed == *config => Ok(text),
        _ => Ok(full_text),
    }
}

fn prune(table: &mut toml_edit::Table, full: &toml::Table, defaults: &toml::Table, path: &str) {
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    for key in keys {
        let here = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let (Some(value), Some(default)) = (full.get(&key), defaults.get(&key)) else {
            continue;
        };
        match (value, default) {
            (toml::Value::Table(value), toml::Value::Table(default)) => {
                let Some(child) = table.get_mut(&key).and_then(toml_edit::Item::as_table_mut)
                else {
                    continue;
                };
                prune(child, value, default, &here);
                if child.is_empty() {
                    table.remove(&key);
                } else if !child.iter().any(|(_, item)| item.is_value()) {
                    // Only sub-tables are left: no empty `[header]`.
                    child.set_implicit(true);
                }
            }
            _ if value == default && !ALWAYS_WRITTEN.contains(&here.as_str()) => {
                table.remove(&key);
            }
            _ => {}
        }
    }
}
