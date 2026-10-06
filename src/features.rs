use crate::{
    analysis::{eq, Analysis, Catalog, Function},
    config::Effective,
    text::{self, Kind},
};
use std::{collections::HashSet, sync::OnceLock};
use tower_lsp::lsp_types::*;

pub fn builtins() -> &'static Vec<Function> {
    static F: OnceLock<Vec<Function>> = OnceLock::new();
    F.get_or_init(|| serde_json::from_str(include_str!("../assets/functions.json")).unwrap())
}
pub fn keywords() -> &'static Vec<String> {
    static K: OnceLock<Vec<String>> = OnceLock::new();
    K.get_or_init(|| serde_json::from_str(include_str!("../assets/keywords.json")).unwrap())
}
pub fn functions<'a>(cat: &'a Catalog) -> Box<dyn Iterator<Item = &'a Function> + 'a> {
    static SPECIAL: OnceLock<Vec<Function>> = OnceLock::new();
    let special = SPECIAL.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/special-functions.json")).unwrap()
    });
    if cat.functions.is_empty() {
        Box::new(builtins().iter().chain(special.iter()))
    } else {
        Box::new(cat.functions.iter().chain(special.iter()))
    }
}
pub fn prefix(text: &str, byte: usize) -> (usize, usize, String) {
    if let Some(t) = text::lex(text).iter().find(|t| {
        t.start < byte
            && byte <= t.end
            && matches!(t.kind, Kind::Word | Kind::Quoted | Kind::Number)
    }) {
        return (
            t.start,
            t.end,
            text[t.start..byte]
                .trim_start_matches('"')
                .replace("\"\"", "\""),
        );
    }
    let start = text[..byte]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(byte);
    (start, byte, text[start..byte].into())
}
pub fn complete(
    text: &str,
    byte: usize,
    a: &Analysis,
    cat: &Catalog,
    config: &Effective,
    limit: usize,
) -> CompletionList {
    let (start, end, filter) = prefix(text, byte);
    let edit_range = text::range(text, start, end);
    let mut items = vec![];
    let mut add =
        |label: String, kind: CompletionItemKind, detail: String, insert: String, rank: u8| {
            if !label.to_lowercase().starts_with(&filter.to_lowercase()) {
                return;
            }
            items.push(CompletionItem {
                label: label.clone(),
                kind: Some(kind),
                detail: Some(detail),
                filter_text: Some(label.clone()),
                sort_text: Some(format!("{rank}{label}")),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: edit_range,
                    new_text: insert,
                })),
                ..Default::default()
            });
        };
    let lexical = text::lex(text);
    let context = lexical
        .iter()
        .find(|t| t.start <= byte.saturating_sub(1) && byte.saturating_sub(1) < t.end);
    if context.is_some_and(|t| t.kind == Kind::String || t.kind == Kind::Comment) {
        return CompletionList {
            is_incomplete: false,
            items,
        };
    }
    if let Some(t) = context.filter(|t| t.kind == Kind::Template) {
        let before = &text[t.start..start];
        let member = before.strip_suffix('.').and_then(|s| {
            s.split(|c: char| !c.is_alphanumeric() && c != '_')
                .next_back()
        });
        for (name, value) in config.context() {
            if member == Some(name.as_str()) {
                if let Some(m) = value.as_object() {
                    for k in m.keys() {
                        add(
                            k.clone(),
                            CompletionItemKind::FIELD,
                            "Jinja context member".into(),
                            k.clone(),
                            0,
                        );
                    }
                }
            } else if member.is_none() {
                add(
                    name.clone(),
                    CompletionItemKind::VARIABLE,
                    "Jinja context variable".into(),
                    name,
                    0,
                );
            }
        }
        if member.is_none() {
            for name in [
                "if", "else", "elif", "endif", "for", "endfor", "set", "macro", "endmacro",
                "include", "import", "range", "loop",
            ] {
                add(
                    name.into(),
                    CompletionItemKind::KEYWORD,
                    "Jinja".into(),
                    name.into(),
                    2,
                );
            }
            for key in config
                .values
                .keys()
                .filter_map(|k| k.strip_prefix("templater.jinja.macros."))
            {
                add(
                    key.into(),
                    CompletionItemKind::FUNCTION,
                    "Configured Jinja macro".into(),
                    key.into(),
                    1,
                );
            }
        }
        return CompletionList {
            is_incomplete: false,
            items,
        };
    }
    let before: Vec<_> = a.tokens.iter().filter(|t| t.end <= start).collect();
    let mut qualifier = vec![];
    let mut j = before.len();
    while j >= 2 && before[j - 1].is(".") && before[j - 2].ident() {
        qualifier.insert(0, before[j - 2].name());
        j -= 2;
    }
    let scope = a.scope_at(byte);
    if qualifier.len() == 1 {
        if let Some(source) = scope.and_then(|i| a.source(i, &qualifier[0])) {
            for c in a.columns(source, cat) {
                add(
                    c.name.clone(),
                    CompletionItemKind::FIELD,
                    c.data_type,
                    text::quote(&c.name),
                    0,
                );
            }
            return CompletionList {
                is_incomplete: false,
                items,
            };
        }
    }
    if !qualifier.is_empty() {
        let mut schemas = HashSet::new();
        for t in &cat.tables {
            if qualifier.len() == 1 && eq(&qualifier[0], &t.catalog) {
                if schemas.insert(t.schema.clone()) {
                    add(
                        t.schema.clone(),
                        CompletionItemKind::MODULE,
                        "Schema".into(),
                        text::quote(&t.schema),
                        1,
                    );
                }
            }
            if (qualifier.len() == 1
                && (eq(&qualifier[0], &t.schema) || eq(&qualifier[0], &t.catalog)))
                || (qualifier.len() == 2
                    && eq(&qualifier[0], &t.catalog)
                    && eq(&qualifier[1], &t.schema))
            {
                add(
                    t.name.clone(),
                    CompletionItemKind::CLASS,
                    format!("{}.{}.{}", t.catalog, t.schema, t.name),
                    text::quote(&t.name),
                    1,
                );
            }
        }
        return CompletionList {
            is_incomplete: false,
            items,
        };
    }
    let relation = before
        .last()
        .is_some_and(|t| t.is("FROM") || t.is("JOIN") || t.is("UPDATE") || t.is("INTO"));
    // Snapshot values are metadata, never a query on the completion request path.
    let tail = text[..start].trim_end();
    if tail.ends_with("=>") && tail.to_uppercase().contains("AT (VERSION") {
        let catalog = scope
            .and_then(|i| a.scopes[i].sources.last())
            .and_then(|s| cat.resolve(&s.parts))
            .map(|t| t.catalog.as_str());
        if let Some(rows) = catalog.and_then(|c| cat.snapshots.get(c)) {
            for row in rows {
                if let Some(id) = row.get("snapshot_id") {
                    let id = id.to_string();
                    add(
                        id.clone(),
                        CompletionItemKind::VALUE,
                        "DuckLake snapshot".into(),
                        id,
                        0,
                    );
                }
            }
        }
        return CompletionList {
            is_incomplete: false,
            items,
        };
    }
    if let Some(mut i) = scope {
        let mut seen = HashSet::new();
        loop {
            for s in &a.scopes[i].sources {
                if seen.insert(s.name.to_lowercase()) && !relation {
                    add(
                        s.name.clone(),
                        CompletionItemKind::VARIABLE,
                        "Relation alias".into(),
                        text::quote(&s.name),
                        0,
                    );
                    for c in a.columns(s, cat) {
                        add(
                            c.name.clone(),
                            CompletionItemKind::FIELD,
                            format!("{} · {}", s.name, c.data_type),
                            text::quote(&c.name),
                            0,
                        );
                    }
                }
            }
            for s in &a.symbols {
                if s.scope == i && s.kind == "cte" {
                    add(
                        s.name.clone(),
                        CompletionItemKind::CLASS,
                        "Common table expression".into(),
                        text::quote(&s.name),
                        0,
                    );
                }
            }
            match a.scopes[i].parent {
                Some(p) => i = p,
                None => break,
            }
        }
    }
    let mut cats = HashSet::new();
    let mut schemas = HashSet::new();
    for d in &a.declarations {
        if d.end < byte {
            if let Some(name) = d.parts.last() {
                add(
                    name.clone(),
                    CompletionItemKind::CLASS,
                    "Table declared in this document".into(),
                    d.parts
                        .iter()
                        .map(|s| text::quote(s))
                        .collect::<Vec<_>>()
                        .join("."),
                    1,
                );
            }
        }
    }
    for t in &cat.tables {
        add(
            t.name.clone(),
            CompletionItemKind::CLASS,
            format!(
                "{}.{}.{}{}",
                t.catalog,
                t.schema,
                t.name,
                if t.ducklake { " · DuckLake" } else { "" }
            ),
            if eq(&cat.default_catalog, &t.catalog) && eq(&cat.default_schema, &t.schema) {
                text::quote(&t.name)
            } else {
                format!(
                    "{}.{}.{}",
                    text::quote(&t.catalog),
                    text::quote(&t.schema),
                    text::quote(&t.name)
                )
            },
            1,
        );
        if cats.insert(t.catalog.clone()) {
            add(
                t.catalog.clone(),
                CompletionItemKind::MODULE,
                "Catalog".into(),
                text::quote(&t.catalog),
                2,
            );
        }
        if schemas.insert(t.schema.clone()) {
            add(
                t.schema.clone(),
                CompletionItemKind::MODULE,
                "Schema".into(),
                text::quote(&t.schema),
                2,
            );
        }
    }
    for f in functions(cat) {
        if !relation || f.kind == "table" || f.kind == "table_macro" {
            add(
                f.name.clone(),
                CompletionItemKind::FUNCTION,
                f.signatures.first().cloned().unwrap_or_default(),
                config.style(&f.name, "functions"),
                3,
            );
        }
    }
    for k in keywords() {
        add(
            k.clone(),
            CompletionItemKind::KEYWORD,
            "DuckDB keyword".into(),
            config.style(k, "keywords"),
            4,
        );
    }
    if !relation {
        for typ in [
            "BOOLEAN",
            "TINYINT",
            "SMALLINT",
            "INTEGER",
            "BIGINT",
            "HUGEINT",
            "UBIGINT",
            "DECIMAL",
            "FLOAT",
            "DOUBLE",
            "VARCHAR",
            "BLOB",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "INTERVAL",
            "UUID",
            "STRUCT",
            "MAP",
            "UNION",
            "JSON",
        ] {
            add(
                typ.into(),
                CompletionItemKind::TYPE_PARAMETER,
                "DuckDB type".into(),
                config.style(typ, "types"),
                3,
            );
        }
    }
    drop(add);
    let mut seen = HashSet::new();
    items.retain(|x| seen.insert((x.label.clone(), x.detail.clone())));
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    let incomplete = items.len() > limit;
    items.truncate(limit);
    CompletionList {
        is_incomplete: incomplete,
        items,
    }
}
pub fn hover(text: &str, byte: usize, a: &Analysis, cat: &Catalog) -> Option<Hover> {
    let token = a.token_at(byte)?;
    let name = token.name();
    let mut content = None;
    if let Some(id) = a.symbol_at(byte) {
        let s = &a.symbols[id];
        content = Some(format!("**{}** `{}`", s.kind, s.name));
    }
    if let Some(f) = functions(cat).find(|f| eq(&f.name, &name)) {
        content = Some(format!(
            "```sql\n{}\n```\n{}",
            f.signatures.join("\n"),
            f.description
        ));
    }
    if let Some(mut scope) = a.scope_at(byte) {
        let index = a.tokens.partition_point(|t| t.start < token.start);
        let qualifier = if index >= 2 && a.tokens[index - 1].is(".") {
            Some(a.tokens[index - 2].name())
        } else {
            None
        };
        let mut columns = vec![];
        let mut seen = HashSet::new();
        loop {
            for s in &a.scopes[scope].sources {
                if s.start <= byte && byte < s.end {
                    if let Some(t) = cat.resolve(&s.parts) {
                        content = Some(format!("```sql\n{}\n```", table_document(t)));
                    }
                }
                if !seen.insert(s.name.to_lowercase())
                    || qualifier.as_ref().is_some_and(|q| !eq(q, &s.name))
                {
                    continue;
                }
                if let Some(c) = a.columns(s, cat).iter().find(|c| eq(&c.name, &name)) {
                    columns.push(format!(
                        "`{}.{}` **{}**\n\n{}",
                        s.name, c.name, c.data_type, c.comment
                    ));
                }
            }
            match a.scopes[scope].parent {
                Some(p) => scope = p,
                None => break,
            }
        }
        if !columns.is_empty() {
            content = Some(if columns.len() > 1 {
                format!(
                    "Ambiguous column; qualify a relation:\n\n{}",
                    columns.join("\n\n")
                )
            } else {
                columns.remove(0)
            });
        }
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: content?,
        }),
        range: Some(text::range(text, token.start, token.end)),
    })
}
pub fn snippets(sql: &str, byte: usize, config: &Effective) -> Vec<CompletionItem> {
    let (start, end, filter) = prefix(sql, byte);
    let (statement, _) = text::statement_at(sql, byte);
    if !text::lex(&sql[statement..start])
        .iter()
        .all(|t| t.kind == Kind::Comment)
    {
        return vec![];
    }
    let width = config
        .values
        .get("indentation.tab_space_size")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|v| v.parse().ok()))
        })
        .unwrap_or(4)
        .clamp(1, 16) as usize;
    let indent = if config.string("indentation.indent_unit") == Some("tab") {
        String::from("\t")
    } else {
        " ".repeat(width)
    };
    let select = config.style("SELECT", "keywords");
    let from = config.style("FROM", "keywords");
    let with = config.style("WITH", "keywords");
    let as_ = config.style("AS", "keywords");
    [("select",format!("{select}\n{indent}${{1:*}}\n{from} ${{2:table}};$0")),("with",format!("{with} ${{1:cte}} {as_} (\n{indent}{select} ${{2:*}} {from} ${{3:table}}\n)\n{select} * {from} $1;$0"))].into_iter().filter(|(label,_)|label.starts_with(&filter.to_lowercase())).map(|(label,new_text)|CompletionItem{label:format!("{label} statement"),kind:Some(CompletionItemKind::SNIPPET),insert_text_format:Some(InsertTextFormat::SNIPPET),sort_text:Some(format!("2{label}")),text_edit:Some(CompletionTextEdit::Edit(TextEdit{range:text::range(sql,start,end),new_text})),..Default::default()}).collect()
}
pub fn table_document(t: &crate::analysis::Table) -> String {
    format!(
        "-- {}.{}.{}{}\nCREATE TABLE {}.{}.{} (\n{}\n);\n",
        t.catalog,
        t.schema,
        t.name,
        if t.ducklake { " (DuckLake)" } else { "" },
        text::quote(&t.catalog),
        text::quote(&t.schema),
        text::quote(&t.name),
        t.columns
            .iter()
            .map(|c| format!("    {} {}", text::quote(&c.name), c.data_type))
            .collect::<Vec<_>>()
            .join(",\n")
    )
}
pub fn signature(text: &str, byte: usize, a: &Analysis, cat: &Catalog) -> Option<SignatureHelp> {
    let ts: Vec<_> = a.tokens.iter().filter(|t| t.end <= byte).collect();
    let mut depth = 0;
    let mut arg = 0;
    for i in (0..ts.len()).rev() {
        if ts[i].is(")") {
            depth += 1;
        } else if ts[i].is("(") {
            if depth > 0 {
                depth -= 1;
                continue;
            }
            if i == 0 {
                return None;
            }
            let f = functions(cat).find(|f| eq(&f.name, &ts[i - 1].name()))?;
            return Some(SignatureHelp {
                signatures: f
                    .signatures
                    .iter()
                    .map(|label| SignatureInformation {
                        label: label.clone(),
                        documentation: Some(Documentation::String(f.description.clone())),
                        parameters: Some(
                            f.parameters
                                .iter()
                                .map(|p| ParameterInformation {
                                    label: ParameterLabel::Simple(p.clone()),
                                    documentation: None,
                                })
                                .collect(),
                        ),
                        active_parameter: Some(
                            arg.min(f.parameters.len().saturating_sub(1) as u32),
                        ),
                    })
                    .collect(),
                active_signature: Some(0),
                active_parameter: Some(arg.min(f.parameters.len().saturating_sub(1) as u32)),
            });
        } else if ts[i].is(",") && depth == 0 {
            arg += 1;
        }
    }
    let _ = text;
    None
}
pub fn syntax_diagnostics(text: &str) -> Vec<Diagnostic> {
    use sqlparser::{dialect::DuckDbDialect, parser::Parser};
    let mut out = vec![];
    for (start, end) in text::statements(text) {
        let sql = &text[start..end];
        if sql.trim().is_empty() || text::lex(sql).iter().any(|t| t.kind == Kind::Template) {
            continue;
        }
        if let Err(e) = Parser::parse_sql(&DuckDbDialect {}, sql) {
            let msg = e.to_string();
            let re = regex::Regex::new(r"Line: (\d+), Column: (\d+)").unwrap();
            let at = re
                .captures(&msg)
                .map(|c| {
                    text::offset(
                        sql,
                        Position::new(
                            c[1].parse::<u32>().unwrap_or(1) - 1,
                            c[2].parse::<u32>().unwrap_or(1) - 1,
                        ),
                    )
                })
                .unwrap_or(0);
            out.push(Diagnostic {
                range: text::range(text, start + at, (start + at + 1).min(end)),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("duckdb-lsp/parser".into()),
                message: msg,
                ..Default::default()
            });
        }
    }
    out
}
