//! `switchyard import-cliproxy`: converts the API-key parts of a CLIProxyAPI
//! `config.yaml` into a Switchyard configuration.
//!
//! Both layouts of the source file are understood: the flat one (`port`,
//! `api-keys` as a list, `gemini-api-key`, …) and the nested "v8" one
//! (`server.port`, `access.api-keys`, `api-keys.gemini[].keys[]`, …). They
//! may be mixed; where a setting is present in both spellings the nested one
//! wins, as in the program the file belongs to.
//!
//! Nothing is dropped silently. Every setting the importer reads is marked;
//! afterwards the document is walked and whatever was not marked (and is not
//! an empty or `false` value) is listed in the "not imported" report by its
//! key path. Values never appear in the report, the summary or an error.

mod general;
mod payload;
mod providers;
mod render;
mod report;
#[cfg(test)]
mod tests;
mod values;

use crate::check::{self, EnvLookup};
use crate::cli::ImportArgs;
use crate::error::{CliError, format_issues};
use crate::starter::{absolute, write_new_file};
use crate::yaml;
use render::{clean, header, render_toml};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;
use switchyard_config_store::validate_text;
use switchyard_core::Config;
use switchyard_core::config::is_secret_reference;
use values::{boolean, integer, lookup, pick_in, text};

/// Largest source file read. A real configuration is a few kilobytes.
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Where the payload rules are, in the nested and in the flat layout.
const PAYLOAD_NESTED: &str = "requests.payload";
const PAYLOAD_FLAT: &str = "payload";

/// The result of converting one source document.
#[derive(Clone, Debug)]
pub struct Imported {
    /// The configuration that was built. Valid.
    pub config: Config,
    /// The file to write: a comment header (with the report) and the
    /// settings that differ from the defaults.
    pub text: String,
    /// What could not be carried over, one sentence per item.
    pub not_imported: Vec<String>,
    /// Adjustments made on the way that are worth knowing.
    pub notes: Vec<String>,
}

/// What `import-cliproxy` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportOutput {
    /// The summary of what was imported.
    pub stdout: String,
    /// The "not imported" report and the notes; empty when there are none.
    pub stderr: String,
}

/// Runs the command: reads `args.input`, writes `args.output`.
pub fn run(args: &ImportArgs, env: EnvLookup<'_>) -> Result<ImportOutput, CliError> {
    let input = &args.input;
    let output = &args.output;
    if absolute(input) == absolute(output) {
        return Err(CliError::usage(
            "the output file is the input file; choose another with -o",
        ));
    }
    if !args.force && output.exists() {
        return Err(CliError::usage(format!(
            "{} already exists; pass --force to replace it, or choose another file with -o",
            output.display()
        )));
    }
    let text = read_input(input)?;
    let source_name = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.yaml".to_string());
    let imported = convert(&text, &source_name).map_err(|error| match error {
        ConvertError::Yaml(error) => CliError::usage(format!(
            "{} is not a configuration this command can read: {error}",
            input.display()
        )),
        ConvertError::Invalid(message) => CliError::failure(message),
    })?;
    write_new_file(output, &imported.text, args.force)?;

    let shown = absolute(output);
    let report = check::inspect(&imported.config, env, None);
    let mut stdout = format!(
        "imported {} into {}\n",
        clean(&input.display().to_string()),
        shown.display()
    );
    for line in &report.summary {
        stdout.push_str(&format!("  {line}\n"));
    }
    let payload_rules = imported.config.payload.default.len()
        + imported.config.payload.overrides.len()
        + imported.config.payload.filter.len();
    stdout.push_str(&format!("  payload rules {payload_rules}\n"));
    if !report.warnings.is_empty() {
        stdout.push_str("to look at:\n");
        for warning in &report.warnings {
            stdout.push_str(&format!("  - {warning}\n"));
        }
    }
    stdout.push_str(&format!(
        "next: review the file, then run `switchyard check --config \"{}\"`\n",
        shown.display()
    ));

    let mut stderr = String::new();
    if !imported.not_imported.is_empty() {
        stderr.push_str("not imported:\n");
        for line in &imported.not_imported {
            stderr.push_str(&format!("  - {line}\n"));
        }
    }
    if !imported.notes.is_empty() {
        stderr.push_str("notes:\n");
        for line in &imported.notes {
            stderr.push_str(&format!("  - {line}\n"));
        }
    }
    Ok(ImportOutput { stdout, stderr })
}

fn read_input(path: &Path) -> Result<String, CliError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| CliError::usage(format!("cannot read {}: {error}", path.display())))?;
    if metadata.is_dir() {
        return Err(CliError::usage(format!(
            "{} is a directory; name the config.yaml inside it",
            path.display()
        )));
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(CliError::usage(format!(
            "{} is larger than {} MiB; that is not a configuration file",
            path.display(),
            MAX_INPUT_BYTES / (1024 * 1024)
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| CliError::failure(format!("cannot read {}: {error}", path.display())))?;
    String::from_utf8(bytes)
        .map_err(|_| CliError::usage(format!("{} is not valid UTF-8 text", path.display())))
}

/// Why a document could not be converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvertError {
    /// The text is not YAML, or not a configuration mapping.
    Yaml(yaml::YamlError),
    /// The result did not validate. A bug in the importer: the message lists
    /// the issues.
    Invalid(String),
}

/// Converts the text of a CLIProxyAPI configuration. `source_name` is the
/// file name mentioned in the header of the generated file.
pub fn convert(yaml_text: &str, source_name: &str) -> Result<Imported, ConvertError> {
    // Read verbatim: the settings are text, booleans and integers, and a
    // key, a password or a name must come out as the file has it. Typing
    // `012345` or `1e5` as numbers first would rewrite them.
    let document = yaml::parse_verbatim(yaml_text).map_err(ConvertError::Yaml)?;
    let empty = Map::new();
    let root = match &document {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => {
            return Err(ConvertError::Yaml(yaml::YamlError {
                line: None,
                message: "the configuration must be a mapping".to_string(),
            }));
        }
    };
    // The values that payload rules set are the one place where a scalar
    // may be of any type, so those are taken from the typed reading of the
    // same text (the two trees have the same shape).
    let typed = match pick_in(root, PAYLOAD_NESTED, PAYLOAD_FLAT) {
        Some(_) => Some(yaml::parse(yaml_text).map_err(ConvertError::Yaml)?),
        None => None,
    };
    let typed_payload = typed
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| pick_in(root, PAYLOAD_NESTED, PAYLOAD_FLAT));

    let mut importer = Importer {
        root,
        typed_payload,
        consumed: BTreeSet::new(),
        not_imported: Vec::new(),
        notes: Vec::new(),
    };
    let config = importer.build();
    let issues = config.validate();
    if !issues.is_empty() {
        return Err(ConvertError::Invalid(format!(
            "the imported configuration does not validate (please report this):\n{}",
            format_issues(&issues)
        )));
    }

    // The one case in which a secret that is imported as written still
    // means something else here.
    let references = environment_lookalikes(&config);
    if references > 0 {
        importer.notes.push(format!(
            "{references} of the imported keys and secrets are written like a reference to an \
             environment variable (env:NAME or ${{NAME}}). CLIProxyAPI uses such text as the \
             secret itself; Switchyard reads the variable instead"
        ));
    }

    let mut not_imported = std::mem::take(&mut importer.not_imported);
    not_imported.extend(importer.leftover_report());
    let not_imported: Vec<String> = not_imported.iter().map(|line| clean(line)).collect();
    let notes: Vec<String> = importer.notes.iter().map(|line| clean(line)).collect();

    let body = render_toml(&config).map_err(ConvertError::Invalid)?;
    let mut text = header(source_name, &not_imported, &notes);
    text.push_str(&body);
    // The header is comments only, but make sure of the whole file.
    match validate_text(&text) {
        Ok(parsed) if parsed == config => {}
        _ => {
            return Err(ConvertError::Invalid(
                "the generated file does not read back as the imported configuration \
                 (please report this)"
                    .to_string(),
            ));
        }
    }
    Ok(Imported {
        config,
        text,
        not_imported,
        notes,
    })
}

/// How many of the imported secrets Switchyard would not use as they are
/// written, but look up in the environment.
fn environment_lookalikes(config: &Config) -> usize {
    let upstream = config.providers.iter().flat_map(|provider| {
        provider.api_keys.iter().chain(
            provider
                .credentials
                .iter()
                .map(|credential| &credential.api_key),
        )
    });
    std::iter::once(&config.admin.secret)
        .chain(config.auth.keys.iter().map(|key| &key.key))
        .chain(upstream)
        .filter(|secret| is_secret_reference(secret))
        .count()
}

/// One mapping of the source document together with its normalised path
/// (list positions written as `[]`), under which the fields read from it
/// are marked as consumed.
#[derive(Clone)]
struct Layer<'v> {
    map: &'v Map<String, Value>,
    path: String,
}

/// The state of one conversion.
struct Importer<'a> {
    /// The document, read verbatim.
    root: &'a Map<String, Value>,
    /// The payload rules of the same document with typed scalars, when it
    /// has any.
    typed_payload: Option<&'a Value>,
    /// Normalised paths of everything that was read.
    consumed: BTreeSet<String>,
    not_imported: Vec<String>,
    notes: Vec<String>,
}

impl<'a> Importer<'a> {
    fn mark(&mut self, path: &str) {
        self.consumed.insert(path.to_string());
    }

    /// The value at a path that has one spelling, when present and not
    /// null.
    fn get(&mut self, path: &str) -> Option<&'a Value> {
        self.mark(path);
        lookup(self.root, path).filter(|value| !value.is_null())
    }

    /// The value of a setting with a nested and a flat spelling. Presence of
    /// the nested one decides, whatever its value.
    fn pick(&mut self, nested: &str, flat: &str) -> Option<&'a Value> {
        self.mark(nested);
        self.mark(flat);
        pick_in(self.root, nested, flat)
    }

    fn wrong_type(&mut self, path: &str) {
        self.not_imported
            .push(format!("{path}: the value has an unexpected type"));
    }

    fn pick_bool(&mut self, nested: &str, flat: &str) -> Option<bool> {
        let value = self.pick(nested, flat)?;
        let parsed = boolean(value);
        if parsed.is_none() {
            self.wrong_type(flat);
        }
        parsed
    }

    fn pick_int(&mut self, nested: &str, flat: &str) -> Option<i64> {
        let value = self.pick(nested, flat)?;
        let parsed = integer(value);
        if parsed.is_none() {
            self.wrong_type(flat);
        }
        parsed
    }

    fn pick_text(&mut self, nested: &str, flat: &str) -> Option<String> {
        let value = self.pick(nested, flat)?;
        let parsed = text(value);
        if parsed.is_none() {
            self.wrong_type(flat);
        }
        parsed
    }

    /// A field of a credential entry: marked in every layer, taken from the
    /// first layer that has it with a value other than null (null means
    /// "inherit").
    fn field<'v>(&mut self, layers: &[Layer<'v>], key: &str) -> Option<&'v Value> {
        for layer in layers {
            self.consumed.insert(format!("{}.{key}", layer.path));
        }
        layers
            .iter()
            .find_map(|layer| layer.map.get(key).filter(|value| !value.is_null()))
    }

    /// Like [`Importer::field`] but without marking the field itself, for
    /// lists whose items are marked field by field (so that unknown fields
    /// of the items still show up in the report).
    fn unmarked_field<'v>(&self, layers: &[Layer<'v>], key: &str) -> Option<&'v Value> {
        layers
            .iter()
            .find_map(|layer| layer.map.get(key).filter(|value| !value.is_null()))
    }

    fn build(&mut self) -> Config {
        let mut config = Config::default();
        self.mark("config-version");
        self.server(&mut config);
        self.admin(&mut config);
        self.client_keys(&mut config);
        self.routing(&mut config);
        self.requests(&mut config);
        self.observability(&mut config);
        self.providers(&mut config);
        config
    }
}
