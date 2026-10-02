//! The function names a Gemini client declared, used to hand function calls
//! back under exactly those names.
//!
//! Gemini function names may contain dots and colons (`mcp.files:read`).
//! OpenAI and Anthropic upstreams accept neither, so their encoders put a
//! `_` in place of such a character, and the model then calls the function
//! by that spelling. A client only recognises the name it declared: a
//! returned name that is not one of the declared ones is matched loosely
//! (ignoring case, punctuation and leading underscores) and replaced when
//! exactly one declared function fits.

use serde_json::Value;

/// The key two spellings of one function name share.
fn loose(name: &str) -> String {
    let key: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    key.trim_start_matches('_').to_string()
}

/// See the module documentation.
#[derive(Debug, Default)]
pub(crate) struct ClientNames {
    declared: Vec<String>,
}

impl ClientNames {
    /// Reads `tools[].functionDeclarations[].name` (either spelling) from
    /// the client's original request; empty when that is unavailable.
    pub(crate) fn from_request(request: &Value) -> Self {
        let mut declared: Vec<String> = Vec::new();
        let tools = match request.get("tools") {
            Some(Value::Array(tools)) => tools.as_slice(),
            Some(tool @ Value::Object(_)) => std::slice::from_ref(tool),
            _ => &[],
        };
        for tool in tools {
            let declarations = tool
                .get("functionDeclarations")
                .or_else(|| tool.get("function_declarations"))
                .and_then(Value::as_array);
            for declaration in declarations.into_iter().flatten() {
                if let Some(name) = declaration.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                    && !declared.iter().any(|d| d == name)
                {
                    declared.push(name.to_string());
                }
            }
        }
        Self { declared }
    }

    /// The client's spelling of `name`, or `name` itself when it is already
    /// one of the declared names or matches none (or several) of them.
    pub(crate) fn restore<'a>(&'a self, name: &'a str) -> &'a str {
        if self.declared.is_empty() || self.declared.iter().any(|d| d == name) {
            return name;
        }
        let key = loose(name);
        let mut found: Option<&'a str> = None;
        for declared in self.declared.iter().filter(|d| loose(d) == key) {
            if found.is_some() {
                return name;
            }
            found = Some(declared);
        }
        found.unwrap_or(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declared_names_are_restored_from_sanitised_spellings() {
        let request = json!({"tools": [
            {"functionDeclarations": [{"name": "mcp.files:read-file"}, {"name": "getWeather"}]},
            {"function_declarations": [{"name": "a.b"}, {"name": "a:b"}]},
            {"googleSearch": {}}
        ]});
        let names = ClientNames::from_request(&request);
        assert_eq!(names.restore("mcp_files_read-file"), "mcp.files:read-file");
        assert_eq!(names.restore("mcp.files:read-file"), "mcp.files:read-file");
        assert_eq!(names.restore("getweather"), "getWeather");
        // Ambiguous and unknown names are left alone.
        assert_eq!(names.restore("a_b"), "a_b");
        assert_eq!(names.restore("other"), "other");
        assert_eq!(
            ClientNames::from_request(&Value::Null).restore("x_y"),
            "x_y"
        );
    }
}
