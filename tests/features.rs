use duckdb_lsp::{
    analysis::{Analysis, Catalog, Column, Table},
    config::{self, Effective, Settings},
    features,
};

fn catalog() -> Catalog {
    Catalog {
        default_catalog: "db".into(),
        default_schema: "main".into(),
        tables: vec![Table {
            catalog: "db".into(),
            schema: "main".into(),
            name: "people".into(),
            columns: vec![Column {
                name: "id".into(),
                data_type: "INTEGER".into(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn cte_star_and_local_ddl_columns() {
    for sql in [
        "WITH d AS (SELECT * FROM people) SELECT x. FROM d x",
        "CREATE TABLE local_table (id INTEGER); SELECT x. FROM local_table x",
    ] {
        let a = Analysis::new(sql);
        let list = features::complete(
            sql,
            sql.find("x.").unwrap() + 2,
            &a,
            &catalog(),
            &Effective::default(),
            20,
        );
        assert!(list.items.iter().any(|i| i.label == "id"), "{sql}");
    }
}
#[test]
fn scalar_commas_are_not_relations_and_comments_do_not_complete() {
    let sql = "SELECT p.id FROM people p JOIN other o ON contains(o.x, p.id)";
    let a = Analysis::new(sql);
    assert_eq!(a.scopes[0].sources.len(), 2);
    for sql in ["SELECT 'sel", "SELECT 1 -- sel", "/* sel"] {
        let a = Analysis::new(sql);
        assert!(
            features::complete(sql, sql.len(), &a, &catalog(), &Effective::default(), 20)
                .items
                .is_empty()
        );
    }
}
#[test]
fn qualified_hover_and_ambiguous_columns() {
    let sql = "SELECT a.id, id FROM people a JOIN people b ON true";
    let a = Analysis::new(sql);
    let hover = features::hover(sql, 9, &a, &catalog()).unwrap();
    let json = serde_json::to_string(&hover).unwrap();
    assert!(json.contains("a.id"));
    assert!(!json.contains("b.id"));
    let json = serde_json::to_string(&features::hover(sql, 14, &a, &catalog())).unwrap();
    assert!(json.contains("Ambiguous"));
}
#[test]
fn config_toml_context_and_multiline_macros() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    std::fs::write(p.join("pyproject.toml"),"[tool.sqlfluff.core]\ndialect='duckdb'\n[tool.sqlfluff.templater.jinja.context]\nobj={field='value'}\n").unwrap();
    let settings = Settings {
        config_provider: "sqlfluff".into(),
        include_user_config: false,
        ..Default::default()
    };
    let e = config::resolve(&p.join("q.sql"), p, "jinjaduckdbsql", "", &settings);
    assert_eq!(e.context()["obj"]["field"], "value");
    std::fs::write(p.join(".sqlfluff"),"[sqlfluff:templater:jinja:macros]\nm = {% macro test() %}\n    {% set x = 1 %}\n    {{x}}{% endmacro %}\n").unwrap();
    let e = config::resolve(&p.join("q.sql"), p, "jinjaduckdbsql", "", &settings);
    assert!(e
        .string("templater.jinja.macros.m")
        .unwrap()
        .contains("set x = 1"));
}
#[test]
fn snippets_honor_indent_and_capitalisation() {
    let mut e = Effective::default();
    e.values
        .insert("indentation.tab_space_size".into(), serde_json::json!(2));
    e.values.insert(
        "rules.capitalisation.keywords.capitalisation_policy".into(),
        serde_json::json!("lower"),
    );
    let list = features::snippets("sel", 3, &e);
    let json = serde_json::to_string(&list).unwrap();
    assert!(json.contains("select\\n  "));
}
