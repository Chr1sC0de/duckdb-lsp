use duckdb_lsp::{
    analysis::{Analysis, Catalog, Column, Table},
    config::Effective,
    features,
};
use std::time::Instant;

fn main() {
    let mut catalog = Catalog {
        default_catalog: "db".into(),
        default_schema: "main".into(),
        ..Default::default()
    };
    for t in 0..1000 {
        catalog.tables.push(Table {
            catalog: "db".into(),
            schema: "main".into(),
            name: format!("table_{t}"),
            columns: (0..20)
                .map(|c| Column {
                    name: format!("column_{c}"),
                    data_type: "INTEGER".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
    }
    let sql = "SELECT 1 AS value;\n".repeat(1999) + "SELECT p. FROM table_0 p";
    let start = Instant::now();
    let analysis = Analysis::new(&sql);
    let analysis_ms = start.elapsed().as_secs_f64() * 1000.;
    let byte = sql.rfind("p.").unwrap() + 2;
    let config = Effective::default();
    let mut durations = vec![];
    for i in 0..220 {
        let start = Instant::now();
        let results = features::complete(&sql, byte, &analysis, &catalog, &config, 200);
        assert_eq!(results.items.len(), 20);
        if i >= 20 {
            durations.push(start.elapsed().as_secs_f64() * 1000.);
        }
    }
    durations.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"documentLines":2000,"tables":1000,"columnsPerTable":20,"samples":200,"analysisMs":analysis_ms,"completionMedianMs":durations[100],"completionP95Ms":durations[190]})
    );
}
