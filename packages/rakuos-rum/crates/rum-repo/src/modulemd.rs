//! Parser for `modules.yaml` (modulemd v2), the repomd `type="modules"`
//! data entry that ships module-stream metadata — enough of it for `rum
//! module list/info/enable/disable/reset` and filtering which modular RPMs
//! a plain `install <name>` is allowed to see. A modules.yaml document is a
//! multi-document YAML stream (`---`-separated), mixing `document:
//! modulemd` entries (one per module:stream:context:arch build) and
//! `document: modulemd-defaults` entries (one per module, naming its
//! default stream) — both are parsed here, everything else (`document:
//! modulemd-obsoletes`, translations, ...) is ignored.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;

/// A single modulemd document: one module at one stream/context/arch
/// build. `artifacts` is the exact list of RPM NEVRAs (`name-epoch:version-
/// release.arch`) this build provides — the source of truth for which repo
/// packages are "modular" and which module:stream they belong to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module {
    pub name: String,
    pub stream: String,
    pub version: String,
    pub context: String,
    pub arch: String,
    pub summary: String,
    pub description: String,
    /// `(profile name, rpm names)` — dnf's `module install name:stream/
    /// profile` installs a profile's named packages; `common`/`default` are
    /// conventional profile names, not special-cased here.
    pub profiles: Vec<(String, Vec<String>)>,
    /// Every RPM NEVRA this module:stream build provides, in modulemd's own
    /// `name-epoch:version-release.arch` form (always has an explicit
    /// epoch, unlike the free-form NVRA dnf/rpm print elsewhere).
    pub artifacts: Vec<String>,
}

/// Every module and module-default parsed from one repo's `modules.yaml`.
#[derive(Debug, Clone, Default)]
pub struct ModuleData {
    pub modules: Vec<Module>,
    /// module name -> default stream, from `modulemd-defaults` documents.
    pub defaults: HashMap<String, String>,
}

pub fn parse_modules_yaml(yaml: &str) -> ModuleData {
    let mut modules = Vec::new();
    let mut defaults = HashMap::new();

    for doc in serde_yaml::Deserializer::from_str(yaml) {
        let Ok(value) = Value::deserialize(doc) else { continue };
        let document = value.get("document").and_then(Value::as_str).unwrap_or("");
        let Some(data) = value.get("data") else { continue };
        match document {
            "modulemd" => {
                if let Some(m) = parse_module(data) {
                    modules.push(m);
                }
            }
            "modulemd-defaults" => {
                let module = data.get("module").and_then(scalar_to_string);
                let stream = data.get("stream").and_then(scalar_to_string);
                if let (Some(module), Some(stream)) = (module, stream) {
                    defaults.insert(module, stream);
                }
            }
            _ => {}
        }
    }

    ModuleData { modules, defaults }
}

fn parse_module(data: &Value) -> Option<Module> {
    let name = data.get("name").and_then(scalar_to_string)?;
    let stream = data.get("stream").and_then(scalar_to_string).unwrap_or_default();
    let version = data.get("version").and_then(scalar_to_string).unwrap_or_default();
    let context = data.get("context").and_then(scalar_to_string).unwrap_or_default();
    let arch = data.get("arch").and_then(scalar_to_string).unwrap_or_default();
    let summary = data.get("summary").and_then(scalar_to_string).unwrap_or_default();
    let description = data.get("description").and_then(scalar_to_string).unwrap_or_default();

    let profiles = data
        .get("profiles")
        .and_then(Value::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = scalar_to_string(k)?;
                    let rpms = v.get("rpms").and_then(Value::as_sequence).map(|seq| seq.iter().filter_map(scalar_to_string).collect()).unwrap_or_default();
                    Some((name, rpms))
                })
                .collect()
        })
        .unwrap_or_default();

    let artifacts = data
        .get("artifacts")
        .and_then(|a| a.get("rpms"))
        .and_then(Value::as_sequence)
        .map(|seq| seq.iter().filter_map(scalar_to_string).collect())
        .unwrap_or_default();

    Some(Module { name, stream, version, context, arch, summary, description, profiles, artifacts })
}

fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modulemd_and_defaults() {
        let yaml = r#"---
document: modulemd
version: 2
data:
  name: nodejs
  stream: "18"
  version: 9020020231031142629
  context: c9
  arch: x86_64
  summary: Javascript runtime
  description: >-
    Node.js JavaScript runtime
  profiles:
    common:
      rpms:
      - nodejs
      - npm
    minimal:
      rpms:
      - nodejs
  artifacts:
    rpms:
    - nodejs-1:18.18.2-1.module_f39+19733+abcdef12.x86_64
    - npm-0:9.8.1-1.18.18.2.1.module_f39+19733+abcdef12.x86_64
...
---
document: modulemd-defaults
version: 1
data:
  module: nodejs
  stream: "18"
  profiles:
    "18": [common]
...
"#;
        let data = parse_modules_yaml(yaml);
        assert_eq!(data.modules.len(), 1);
        let m = &data.modules[0];
        assert_eq!(m.name, "nodejs");
        assert_eq!(m.stream, "18");
        assert_eq!(m.context, "c9");
        assert_eq!(m.arch, "x86_64");
        assert_eq!(m.artifacts.len(), 2);
        assert!(m.artifacts.contains(&"nodejs-1:18.18.2-1.module_f39+19733+abcdef12.x86_64".to_string()));
        let common = m.profiles.iter().find(|(n, _)| n == "common").unwrap();
        assert_eq!(common.1, vec!["nodejs".to_string(), "npm".to_string()]);
        assert_eq!(data.defaults.get("nodejs"), Some(&"18".to_string()));
    }

    #[test]
    fn ignores_unknown_documents() {
        let yaml = r#"---
document: modulemd-translations
version: 1
data:
  module: nodejs
...
"#;
        let data = parse_modules_yaml(yaml);
        assert!(data.modules.is_empty());
        assert!(data.defaults.is_empty());
    }
}
