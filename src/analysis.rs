//! Recovery-oriented scope index. It survives incomplete statements while the
//! native parser performs authoritative validation in the background.
use crate::text::{lex, statements, Kind, Token};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type", default)]
    pub data_type: String,
    #[serde(default)]
    pub comment: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Table {
    pub catalog: String,
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub ducklake: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub signatures: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub tables: Vec<Table>,
    #[serde(default)]
    pub functions: Vec<Function>,
    #[serde(default)]
    pub snapshots: BTreeMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub default_catalog: String,
    #[serde(default)]
    pub default_schema: String,
    #[serde(default)]
    pub engine_version: String,
}
impl Catalog {
    pub fn resolve(&self, parts: &[String]) -> Option<&Table> {
        let matches: Vec<_> = self
            .tables
            .iter()
            .filter(|t| match parts {
                [name] => eq(name, &t.name),
                [schema, name] => {
                    eq(name, &t.name) && (eq(schema, &t.schema) || eq(schema, &t.catalog))
                }
                [cat, schema, name] => {
                    eq(cat, &t.catalog) && eq(schema, &t.schema) && eq(name, &t.name)
                }
                _ => false,
            })
            .collect();
        matches
            .iter()
            .find(|t| {
                eq(&t.catalog, &self.default_catalog)
                    && (self.default_schema.is_empty() || eq(&t.schema, &self.default_schema))
            })
            .copied()
            .or_else(|| {
                if matches.len() == 1 {
                    Some(matches[0])
                } else {
                    None
                }
            })
    }
}
pub fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub scope: usize,
    pub kind: &'static str,
    pub columns: Vec<Column>,
}
#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub parts: Vec<String>,
    pub symbol: Option<usize>,
    pub table_symbol: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub columns: Vec<Column>,
}
#[derive(Clone, Debug)]
pub struct Scope {
    pub start: usize,
    pub end: usize,
    pub parent: Option<usize>,
    pub sources: Vec<Source>,
    pub projections: Vec<Column>,
}
#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub tokens: Vec<Token>,
    pub scopes: Vec<Scope>,
    pub symbols: Vec<Symbol>,
    pub bindings: HashMap<usize, usize>,
    pub declarations: Vec<Declaration>,
    root_count: usize,
    children: HashMap<usize, Vec<usize>>,
}
#[derive(Clone, Debug)]
pub struct Declaration {
    pub parts: Vec<String>,
    pub start: usize,
    pub end: usize,
    pub columns: Vec<Column>,
}

const RESERVED:&str="SELECT FROM JOIN LEFT RIGHT FULL INNER OUTER CROSS ON USING WHERE GROUP ORDER BY HAVING QUALIFY LIMIT OFFSET UNION EXCEPT INTERSECT WINDOW AS AT TABLESAMPLE PIVOT UNPIVOT SEMI ANTI NATURAL WHEN THEN ELSE END SET RETURNING VALUES INTO AND OR WITH RECURSIVE";
pub fn reserved(s: &str) -> bool {
    RESERVED.split_whitespace().any(|x| eq(x, s))
}
impl Analysis {
    pub fn new(text: &str) -> Self {
        let tokens: Vec<_> = lex(text)
            .into_iter()
            .filter(|t| t.kind != Kind::Comment)
            .collect();
        let mut a = Self {
            tokens,
            ..Default::default()
        };
        for (start, end) in statements(text) {
            let first = a.tokens.partition_point(|t| t.start < start);
            let last = a.tokens.partition_point(|t| t.end <= end);
            let ts = &a.tokens[first..last];
            if ts.first().is_some_and(|t| t.is("CREATE")) {
                if let Ok(parsed) = sqlparser::parser::Parser::parse_sql(
                    &sqlparser::dialect::DuckDbDialect {},
                    &text[start..end],
                ) {
                    for statement in parsed {
                        if let sqlparser::ast::Statement::CreateTable(table) = statement {
                            let parts: Vec<_> = table
                                .name
                                .0
                                .iter()
                                .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
                                .collect();
                            let name_start = ts
                                .iter()
                                .position(|t| t.is("TABLE"))
                                .map(|i| i + 1)
                                .unwrap_or(0);
                            let at = ts.get(name_start).map(|t| t.start).unwrap_or(start);
                            a.declarations.push(Declaration {
                                parts,
                                start: at,
                                end,
                                columns: table
                                    .columns
                                    .iter()
                                    .map(|c| Column {
                                        name: c.name.value.clone(),
                                        data_type: c.data_type.to_string(),
                                        comment: String::new(),
                                    })
                                    .collect(),
                            });
                        }
                    }
                }
            }
            a.scopes.push(Scope {
                start,
                end,
                parent: None,
                sources: vec![],
                projections: vec![],
            });
        }
        a.root_count = a.scopes.len();
        let mut stack = Vec::new();
        let mut pairs = HashMap::new();
        for (i, t) in a.tokens.iter().enumerate() {
            if t.is("(") {
                stack.push(i);
            } else if t.is(")") {
                if let Some(j) = stack.pop() {
                    pairs.insert(j, i);
                }
            }
        }
        for j in stack {
            pairs.insert(j, a.tokens.len());
        }
        let mut nested: Vec<_> = pairs
            .iter()
            .filter_map(|(&i, &j)| {
                if a.tokens
                    .get(i + 1)
                    .is_some_and(|t| t.is("SELECT") || t.is("WITH") || t.is("FROM"))
                {
                    Some((
                        a.tokens[i].end,
                        a.tokens.get(j).map(|t| t.start).unwrap_or(text.len()),
                    ))
                } else {
                    None
                }
            })
            .collect();
        nested.sort_by_key(|(s, e)| (*s, std::cmp::Reverse(*e)));
        for (start, end) in nested {
            let parent = a.scope_at(start);
            if let Some(parent) = parent {
                a.children.entry(parent).or_default().push(a.scopes.len());
            }
            a.scopes.push(Scope {
                start,
                end,
                parent,
                sources: vec![],
                projections: vec![],
            });
        }
        // CTE declarations, including explicit projected column names.
        for i in 0..a.tokens.len() {
            if !a.tokens[i].is("WITH") {
                continue;
            }
            let owner = a.scope_at(a.tokens[i].start).unwrap_or(0);
            let mut j = i + 1;
            if a.tokens.get(j).is_some_and(|t| t.is("RECURSIVE")) {
                j += 1;
            }
            loop {
                let Some(name) = a.tokens.get(j).filter(|t| t.ident()).cloned() else {
                    break;
                };
                j += 1;
                let mut columns = vec![];
                if a.tokens.get(j).is_some_and(|t| t.is("(")) {
                    if let Some(&end) = pairs.get(&j) {
                        columns = a.tokens[j + 1..end.min(a.tokens.len())]
                            .iter()
                            .filter(|t| t.ident())
                            .map(|t| Column {
                                name: t.name(),
                                ..Default::default()
                            })
                            .collect();
                        j = end + 1;
                    }
                }
                if !a.tokens.get(j).is_some_and(|t| t.is("AS")) {
                    break;
                }
                j += 1;
                if a.tokens.get(j).is_some_and(|t| t.is("NOT")) {
                    j += 1;
                }
                if a.tokens.get(j).is_some_and(|t| t.is("MATERIALIZED")) {
                    j += 1;
                }
                if !a.tokens.get(j).is_some_and(|t| t.is("(")) {
                    break;
                }
                let end = *pairs.get(&j).unwrap_or(&a.tokens.len());
                let id = a.symbols.len();
                a.symbols.push(Symbol {
                    name: name.name(),
                    start: name.start,
                    end: name.end,
                    scope: owner,
                    kind: "cte",
                    columns,
                });
                a.bindings.insert(name.start, id);
                j = end + 1;
                if !a.tokens.get(j).is_some_and(|t| t.is(",")) {
                    break;
                }
                j += 1;
            }
        }
        // Relation references. Commas are relations only inside a FROM clause.
        let mut from_scope = HashMap::new();
        let mut depths = Vec::new();
        let mut level = 0usize;
        let mut base = HashMap::new();
        for t in &a.tokens {
            if t.is(")") {
                level = level.saturating_sub(1);
            }
            depths.push(level);
            if let Some(scope) = a.scope_at(t.start) {
                base.entry(scope).or_insert(level);
            }
            if t.is("(") {
                level += 1;
            }
        }
        for i in 0..a.tokens.len() {
            let t = &a.tokens[i];
            let Some(scope) = a.scope_at(t.start) else {
                continue;
            };
            if depths[i] != *base.get(&scope).unwrap_or(&0) {
                continue;
            }
            if [
                "WHERE", "GROUP", "ORDER", "HAVING", "QUALIFY", "LIMIT", "UNION",
            ]
            .iter()
            .any(|x| t.is(x))
            {
                from_scope.insert(scope, false);
            }
            let relation = t.is("FROM")
                || t.is("JOIN")
                || (t.is(",") && *from_scope.get(&scope).unwrap_or(&false));
            if t.is("FROM") {
                from_scope.insert(scope, true);
            }
            if !relation {
                continue;
            }
            let mut j = i + 1;
            let mut parts = vec![];
            let mut cols = vec![];
            let mut source_range = None;
            if let Some(first) = a.tokens.get(j) {
                if first.is("(") {
                    if let Some(&end) = pairs.get(&j) {
                        let inner = a.scope_at(first.end);
                        if let Some(inner) = inner {
                            cols = a.projections(inner);
                        }
                        j = end + 1;
                    }
                } else if first.ident() {
                    source_range = Some((first.start, first.end));
                    parts.push(first.name());
                    j += 1;
                    while a.tokens.get(j).is_some_and(|t| t.is("."))
                        && a.tokens.get(j + 1).is_some_and(|t| t.ident())
                    {
                        parts.push(a.tokens[j + 1].name());
                        source_range.as_mut().unwrap().1 = a.tokens[j + 1].end;
                        j += 2;
                    }
                    if a.tokens.get(j).is_some_and(|t| t.is("(")) {
                        j = pairs.get(&j).copied().unwrap_or(a.tokens.len()) + 1;
                    }
                    if a.tokens.get(j).is_some_and(|t| t.is("AT"))
                        && a.tokens.get(j + 1).is_some_and(|t| t.is("("))
                    {
                        j = pairs.get(&(j + 1)).copied().unwrap_or(a.tokens.len()) + 1;
                    }
                } else {
                    continue;
                }
            }
            let table_symbol = if parts.len() == 1 {
                a.find_symbol(scope, &parts[0], "cte")
            } else {
                None
            };
            if let (Some(id), Some((start, _))) = (table_symbol, source_range) {
                a.bindings.insert(start, id);
            }
            if a.tokens.get(j).is_some_and(|t| t.is("AS")) {
                j += 1;
            }
            let alias = a
                .tokens
                .get(j)
                .filter(|t| t.ident() && !reserved(&t.name()))
                .cloned();
            let (name, symbol) = if let Some(alias) = alias {
                let id = a.symbols.len();
                a.symbols.push(Symbol {
                    name: alias.name(),
                    start: alias.start,
                    end: alias.end,
                    scope,
                    kind: "alias",
                    columns: cols.clone(),
                });
                a.bindings.insert(alias.start, id);
                (alias.name(), Some(id))
            } else {
                (parts.last().cloned().unwrap_or_default(), table_symbol)
            };
            if name.is_empty() {
                continue;
            }
            let (start, end) = source_range.unwrap_or((a.tokens[i].end, a.tokens[i].end));
            a.scopes[scope].sources.push(Source {
                name,
                parts,
                symbol,
                table_symbol,
                start,
                end,
                columns: cols,
            });
        }
        for scope in (0..a.scopes.len()).rev() {
            a.scopes[scope].projections = a.projections(scope);
        }
        for id in 0..a.symbols.len() {
            if a.symbols[id].kind == "cte" && a.symbols[id].columns.is_empty() {
                let end = a.symbols[id].end;
                if let Some(s) = a
                    .scopes
                    .iter()
                    .filter(|s| s.start > end && s.parent == Some(a.symbols[id].scope))
                    .min_by_key(|s| s.start)
                {
                    a.symbols[id].columns = s.projections.clone();
                }
            }
        }
        // Qualified column references bind to the nearest visible relation alias.
        for i in 0..a.tokens.len() {
            let t = &a.tokens[i];
            if t.ident() && a.tokens.get(i + 1).is_some_and(|t| t.is(".")) {
                if let Some(scope) = a.scope_at(t.start) {
                    if let Some(src) = a.source(scope, &t.name()) {
                        if let Some(id) = src.symbol {
                            a.bindings.entry(t.start).or_insert(id);
                        }
                    }
                }
            }
        }
        a
    }
    pub fn scope_at(&self, byte: usize) -> Option<usize> {
        let root = self.scopes[..self.root_count]
            .partition_point(|s| s.start <= byte)
            .checked_sub(1)?;
        if byte > self.scopes[root].end {
            return None;
        }
        let mut current = root;
        while let Some(children) = self.children.get(&current) {
            let i = children.partition_point(|i| self.scopes[*i].start <= byte);
            let Some(id) = i.checked_sub(1).map(|i| children[i]) else {
                break;
            };
            if byte > self.scopes[id].end {
                break;
            }
            current = id;
        }
        Some(current)
    }
    pub fn find_symbol(&self, mut scope: usize, name: &str, kind: &str) -> Option<usize> {
        loop {
            if let Some((i, _)) = self
                .symbols
                .iter()
                .enumerate()
                .find(|(_, s)| s.scope == scope && s.kind == kind && eq(&s.name, name))
            {
                return Some(i);
            }
            scope = self.scopes.get(scope)?.parent?;
        }
    }
    pub fn source(&self, mut scope: usize, name: &str) -> Option<&Source> {
        loop {
            if let Some(s) = self.scopes[scope]
                .sources
                .iter()
                .find(|s| eq(&s.name, name))
            {
                return Some(s);
            }
            scope = self.scopes.get(scope)?.parent?;
        }
    }
    pub fn columns(&self, source: &Source, catalog: &Catalog) -> Vec<Column> {
        self.columns_inner(source, catalog, 0)
    }
    fn columns_inner(&self, source: &Source, catalog: &Catalog, depth: usize) -> Vec<Column> {
        if depth > 16 {
            return vec![];
        }
        if let Some(id) = source.table_symbol {
            let sym = &self.symbols[id];
            let mut columns = sym.columns.clone();
            if columns.iter().any(|c| c.name == "*") {
                columns.retain(|c| c.name != "*");
                if let Some(scope) = self
                    .scopes
                    .iter()
                    .filter(|s| s.start > sym.end && s.parent == Some(sym.scope))
                    .min_by_key(|s| s.start)
                {
                    for s in &scope.sources {
                        columns.extend(self.columns_inner(s, catalog, depth + 1));
                    }
                }
            }
            return columns;
        }
        if !source.columns.is_empty() {
            return source.columns.clone();
        }
        if let Some(d) = self.declarations.iter().rev().find(|d| {
            d.end < source.start
                && d.parts.len() == source.parts.len()
                && d.parts.iter().zip(&source.parts).all(|(a, b)| eq(a, b))
        }) {
            return d.columns.clone();
        }
        catalog
            .resolve(&source.parts)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
    }
    pub fn token_at(&self, byte: usize) -> Option<&Token> {
        self.tokens
            .iter()
            .find(|t| t.start <= byte && byte < t.end)
            .or_else(|| self.tokens.iter().find(|t| t.end == byte && t.ident()))
    }
    pub fn symbol_at(&self, byte: usize) -> Option<usize> {
        self.token_at(byte)
            .and_then(|t| self.bindings.get(&t.start).copied())
    }
    fn projections(&self, scope: usize) -> Vec<Column> {
        let first = self
            .tokens
            .partition_point(|t| t.start < self.scopes[scope].start);
        let last = self
            .tokens
            .partition_point(|t| t.end <= self.scopes[scope].end);
        let ts: Vec<_> = self.tokens[first..last]
            .iter()
            .filter(|t| self.scope_at(t.start) == Some(scope))
            .collect();
        let Some(start) = ts.iter().position(|t| t.is("SELECT")) else {
            return vec![];
        };
        let end = ts[start + 1..]
            .iter()
            .position(|t| t.is("FROM"))
            .map(|i| start + 1 + i)
            .unwrap_or(ts.len());
        let mut out = vec![];
        let mut begin = start + 1;
        let mut depth = 0;
        for i in start + 1..=end {
            if i < end {
                if ts[i].is("(") {
                    depth += 1;
                }
                if ts[i].is(")") {
                    depth -= 1;
                }
            }
            if i == end || (ts[i].is(",") && depth == 0) {
                let expr = &ts[begin..i];
                let name = expr
                    .iter()
                    .rposition(|t| t.is("AS"))
                    .and_then(|j| expr.get(j + 1))
                    .filter(|t| t.ident())
                    .map(|t| t.name())
                    .or_else(|| {
                        if expr.iter().all(|t| t.ident() || t.is(".") || t.is("*")) {
                            expr.last().map(|t| t.name())
                        } else {
                            None
                        }
                    });
                if let Some(name) = name {
                    out.push(Column {
                        name,
                        ..Default::default()
                    });
                }
                begin = i + 1;
            }
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cte_and_alias() {
        let s = "WITH data AS (SELECT 1 AS id) SELECT d.id FROM data AS d";
        let a = Analysis::new(s);
        let scope = a.scope_at(s.find("d.id").unwrap()).unwrap();
        let d = a.source(scope, "d").unwrap();
        assert_eq!(a.columns(d, &Catalog::default())[0].name, "id");
        assert_eq!(
            a.symbol_at(s.find("d.id").unwrap()).unwrap(),
            d.symbol.unwrap()
        );
    }
    #[test]
    fn shadowing() {
        let s = "SELECT a.id FROM outer_table a WHERE EXISTS (SELECT a.id FROM inner_table a)";
        let a = Analysis::new(s);
        let x = a.symbol_at(s.find("a.id").unwrap()).unwrap();
        let y = a.symbol_at(s.rfind("a.id").unwrap()).unwrap();
        assert_ne!(x, y);
    }
}
