use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub config_provider: String,
    pub python: String,
    pub diagnostics_delay_ms: u64,
    pub worker_timeout_ms: u64,
    pub catalog_ttl_seconds: u64,
    pub lint: bool,
    pub format: bool,
    pub native_diagnostics: bool,
    pub max_completions: usize,
    pub default_connection: Option<Value>,
    pub include_user_config: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            config_provider: "auto".into(),
            python: "python3".into(),
            diagnostics_delay_ms: 250,
            worker_timeout_ms: 10000,
            catalog_ttl_seconds: 60,
            lint: false,
            format: false,
            native_diagnostics: true,
            max_completions: 200,
            default_connection: None,
            include_user_config: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Effective {
    pub provider: String,
    pub values: BTreeMap<String, Value>,
    pub sources: BTreeMap<String, String>,
    pub files: Vec<PathBuf>,
    pub issues: Vec<String>,
    pub render: bool,
    pub dialect: String,
}
impl Effective {
    pub fn string(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_str()
    }
    pub fn style(&self, word: &str, category: &str) -> String {
        let key = if category == "keywords" {
            "capitalisation_policy"
        } else {
            "extended_capitalisation_policy"
        };
        match self.string(&format!("rules.capitalisation.{category}.{key}")) {
            Some("upper") => word.to_uppercase(),
            Some("lower") => word.to_lowercase(),
            _ => word.to_string(),
        }
    }
    pub fn context(&self) -> BTreeMap<String, Value> {
        self.values
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("templater.jinja.context.")
                    .map(|k| (k.to_string(), v.clone()))
            })
            .collect()
    }
}
const SQ: &[&str] = &[".sqruff", ".sqruff.ini", "sqruff.toml", "pyproject.toml"];
const FL: &[&str] = &[
    "setup.cfg",
    "tox.ini",
    "pep8.ini",
    ".sqlfluff",
    "pyproject.toml",
];

pub fn resolve(
    path: &Path,
    root: &Path,
    language: &str,
    text: &str,
    settings: &Settings,
) -> Effective {
    let mut e = Effective {
        dialect: "duckdb".into(),
        ..Default::default()
    };
    let mut dirs = Vec::new();
    if settings.include_user_config {
        if let Some(home) = std::env::var_os("HOME") {
            let h = PathBuf::from(home);
            dirs.push(
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| h.join(".config"))
                    .join("sqlfluff"),
            );
            dirs.push(h);
        }
    }
    let mut ancestors = Vec::new();
    let mut p = path.parent();
    while let Some(d) = p {
        if !d.starts_with(root) {
            break;
        }
        ancestors.push(d.to_path_buf());
        if d == root {
            break;
        }
        p = d.parent();
    }
    ancestors.reverse();
    if ancestors.is_empty() {
        ancestors.push(root.to_path_buf());
    }
    dirs.extend(ancestors);
    dirs.dedup();
    let has = |names: &[&str]| {
        dirs.iter()
            .any(|d| names.iter().any(|f| d.join(f).is_file()))
    };
    let sq = has(&SQ[..3]);
    let fl = has(&[".sqlfluff"])
        || dirs.iter().any(|d| {
            FL[..3]
                .iter()
                .any(|f| read_config(&d.join(f), "sqlfluff").is_ok_and(|v| !v.is_empty()))
        });
    let shared = dirs.iter().any(|d| {
        std::fs::read_to_string(d.join("pyproject.toml"))
            .unwrap_or_default()
            .contains("tool.sqlfluff")
    });
    e.provider = match settings.config_provider.as_str() {
        "auto" if sq && !fl => "sqruff",
        "auto" if fl && !sq => "sqlfluff",
        "auto" if sq || fl || shared => {
            e.issues.push(
                "Ambiguous SQL configuration: set configProvider to sqruff or sqlfluff.".into(),
            );
            "none"
        }
        "auto" | "none" => "none",
        "sqruff" => "sqruff",
        "sqlfluff" => "sqlfluff",
        _ => {
            e.issues.push("Unknown configProvider.".into());
            "none"
        }
    }
    .into();
    if e.provider != "none" {
        let names = if e.provider == "sqruff" { SQ } else { FL };
        for dir in &dirs {
            for name in names {
                let f = dir.join(name);
                if !f.is_file() {
                    continue;
                }
                e.files.push(f.clone());
                match read_config(&f, &e.provider) {
                    Ok(values) => {
                        for (key, value) in values {
                            if e.provider == "sqlfluff"
                                && key == "core.templater"
                                && dir != root
                                && dir.starts_with(root)
                                && e.values.contains_key(&key)
                            {
                                continue;
                            }
                            e.sources.insert(key.clone(), f.to_string_lossy().into());
                            e.values.insert(key, value);
                        }
                    }
                    Err(_) => e
                        .issues
                        .push(format!("Cannot parse configuration: {}", f.display())),
                }
            }
        }
        for line in text.lines() {
            let prefix = format!("-- {}:", e.provider);
            if let Some(directive) = line.trim_start().strip_prefix(&prefix) {
                if let Some((k, v)) = directive.rsplit_once(':') {
                    let k = if k.contains(':') {
                        k.replace(':', ".")
                    } else {
                        format!("core.{k}")
                    };
                    e.sources.insert(k.clone(), "inline directive".into());
                    e.values.insert(k, scalar(v));
                }
            }
        }
    }
    let dialect = e.string("core.dialect").unwrap_or("duckdb").to_string();
    let templater = e.string("core.templater").map(str::to_string);
    if language == "sql" {
        e.dialect = dialect.clone();
    }
    if dialect != "duckdb" {
        e.issues.push(format!(
            "This server analyzes DuckDB; configuration selects {dialect}."
        ));
    }
    e.render = language == "jinjaduckdbsql"
        || (language == "sql" && templater.as_deref() == Some("jinja"));
    if language == "duckdbsql" && templater.as_deref().is_some_and(|x| x != "raw") {
        e.issues
            .push("duckdbsql selects raw SQL; use jinjaduckdbsql for Jinja rendering.".into());
    }
    if language == "jinjaduckdbsql" && templater.as_deref().is_some_and(|x| x != "jinja") {
        e.render = false;
        e.issues.push(
            "jinjaduckdbsql conflicts with the configured templater; rendering is paused.".into(),
        );
    }
    e
}
fn scalar(s: &str) -> Value {
    let s = s.trim();
    match s {
        "True" | "true" => json!(true),
        "False" | "false" => json!(false),
        "None" | "none" => Value::Null,
        _ => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
    }
}
fn flatten(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, Value>) {
    if prefix
        .trim_start_matches('.')
        .starts_with("templater.jinja.context.")
    {
        out.insert(
            prefix.trim_start_matches('.').into(),
            serde_json::to_value(v).unwrap_or(Value::Null),
        );
        return;
    }
    if let Some(t) = v.as_table() {
        for (k, v) in t {
            flatten(&format!("{prefix}.{k}"), v, out)
        }
    } else {
        out.insert(
            prefix.trim_start_matches('.').into(),
            serde_json::to_value(v).unwrap_or(Value::Null),
        );
    }
}
fn read_config(path: &Path, provider: &str) -> Result<BTreeMap<String, Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = BTreeMap::new();
    if path.extension().is_some_and(|x| x == "toml") {
        let v: toml::Value = text.parse::<toml::Value>().map_err(|e| e.to_string())?;
        if let Some(c) = v
            .get("tool")
            .and_then(|x| x.get(provider).or_else(|| x.get("sqlfluff")))
        {
            flatten("", c, &mut out);
        } else if path.file_name().is_some_and(|x| x == "sqruff.toml") {
            if let Some(c) = v.get("sqruff") {
                flatten("", c, &mut out);
            }
        }
        return Ok(out);
    }
    let mut section = None;
    let mut previous: Option<String> = None;
    let mut previous_indent = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            let s = &t[1..t.len() - 1];
            section = if s == provider {
                Some("core".to_string())
            } else {
                s.strip_prefix(&format!("{provider}:"))
                    .map(|s| s.replace(':', "."))
            };
            previous = None;
        } else if let Some(s) = &section {
            let indent = line.len() - line.trim_start().len();
            if indent > previous_indent && previous.is_some() {
                let k = previous.as_ref().unwrap();
                let v = out.get(k).and_then(Value::as_str).unwrap_or("");
                out.insert(k.clone(), json!(format!("{v}\n{t}")));
            } else if let Some((key, value)) = t.split_once('=') {
                let k = format!("{s}.{}", key.trim());
                out.insert(k.clone(), scalar(value));
                previous = Some(k);
                previous_indent = indent;
            } else if line.starts_with(char::is_whitespace) {
                if let Some(k) = &previous {
                    let v = out.get(k).and_then(Value::as_str).unwrap_or("");
                    out.insert(k.clone(), json!(format!("{v}\n{t}")));
                }
            } else {
                return Err("invalid INI entry".into());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_configuration_and_modes() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir(r.join("nested")).unwrap();
        std::fs::write(r.join(".sqruff"),"[sqruff]\ndialect=duckdb\n[sqruff:rules:capitalisation:keywords]\ncapitalisation_policy=upper").unwrap();
        std::fs::write(
            r.join("nested/.sqruff"),
            "[sqruff:templater:jinja:context]\ntable=people",
        )
        .unwrap();
        let e = resolve(
            &r.join("nested/a.sql"),
            r,
            "jinjaduckdbsql",
            "",
            &Settings {
                include_user_config: false,
                ..Default::default()
            },
        );
        assert_eq!(e.provider, "sqruff");
        assert!(e.render);
        assert_eq!(e.context()["table"], "people");
        assert_eq!(e.style("select", "keywords"), "SELECT");
    }
    #[test]
    fn ambiguity_and_explicit_override() {
        let d = tempfile::tempdir().unwrap();
        for n in [".sqlfluff", ".sqruff"] {
            std::fs::write(d.path().join(n), "").unwrap();
        }
        let e = resolve(
            &d.path().join("a.sql"),
            d.path(),
            "duckdbsql",
            "",
            &Settings {
                include_user_config: false,
                ..Default::default()
            },
        );
        assert_eq!(e.provider, "none");
        assert_eq!(e.issues.len(), 1);
    }
}
