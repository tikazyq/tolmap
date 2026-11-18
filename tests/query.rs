use std::collections::BTreeSet;
use std::path::Path;

use serde_json::json;
use tolmap::query::Snapshot;
use tolmap::query_contract::{Direction, Query, QueryResult};

fn fixture(name: &str) -> Snapshot {
    Snapshot::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("data/{name}.json")))
        .unwrap()
}

#[test]
fn pinned_file_symbol_and_ambiguity() {
    let s = fixture("tolmap");
    let file = s.where_is("src/main.rs", 8).unwrap();
    assert_eq!(file.schema, "tolmap.query/v3");
    assert_eq!(file.total, 1);
    assert_eq!(file.map.sha256.len(), 64);
    assert!(file.map.commit.is_none());
    assert!(file.coverage.unresolved.is_none());
    assert!(serde_json::to_vec(&file).unwrap().len() < 2000);
    let symbol = s.where_is("main", 1).unwrap();
    assert!(matches!(
        symbol.query,
        Query::Where {
            ambiguous: true,
            ..
        }
    ));
    assert!(symbol.total > 1);
    assert!(symbol.truncated);
    let exact = s.where_is("src/bin/generate_types.rs::main", 8).unwrap();
    assert_eq!(exact.total, 1);
    assert!(
        matches!(&exact.results[0], QueryResult::File { symbol: Some(symbol), .. } if symbol.name == "main")
    );
    assert_eq!(s.where_is("no-such-symbol", 8).unwrap().total, 0);
}

#[test]
fn pinned_deps_match_all_edges_and_limit_each_direction() {
    let s = fixture("tolmap");
    for (path, in_total, out_total) in [("src/progress.rs", 14, 2), ("src/geometry.rs", 2, 10)] {
        let file = s.map.files.iter().position(|p| p == path).unwrap();
        let importers: BTreeSet<_> = s
            .map
            .edges
            .iter()
            .filter(|&&[_, b]| b == file)
            .map(|&[a, _]| s.map.files[a].as_str())
            .collect();
        let imports: BTreeSet<_> = s
            .map
            .edges
            .iter()
            .filter(|&&[a, _]| a == file)
            .map(|&[_, b]| s.map.files[b].as_str())
            .collect();
        assert_eq!((importers.len(), imports.len()), (in_total, out_total));
        for direction in [Direction::In, Direction::Out, Direction::Both] {
            for limit in [0, 1, 2, in_total.max(out_total)] {
                let report = s.deps(path, direction, limit);
                let incoming = (direction != Direction::Out).then_some(in_total);
                let outgoing = (direction != Direction::In).then_some(out_total);
                let counts = |total: usize| json!({"total": total, "returned": total.min(limit), "omitted": total.saturating_sub(limit)});
                let wire = serde_json::to_value(&report).unwrap();
                assert_eq!(
                    wire["directions"],
                    json!({"in": incoming.map(counts), "out": outgoing.map(counts)})
                );
                let total = incoming.unwrap_or(0) + outgoing.unwrap_or(0);
                let full_limit = incoming.max(outgoing).unwrap();
                let mut expected = Vec::new();
                if incoming.is_some() {
                    expected.extend(importers.iter().take(limit).map(|&p| (p, "in")));
                }
                if outgoing.is_some() {
                    expected.extend(imports.iter().take(limit).map(|&p| (p, "out")));
                }
                expected.sort_unstable();
                let actual: Vec<_> = wire["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| {
                        (
                            row["path"].as_str().unwrap(),
                            row["direction"].as_str().unwrap(),
                        )
                    })
                    .collect();
                assert_eq!(actual, expected);
                assert_eq!(wire["total"], total);
                assert_eq!(wire["returned"], expected.len());
                assert_eq!(wire["omitted"], total - expected.len());
                assert_eq!(wire["truncated"], full_limit > limit);
                assert_eq!(
                    wire["next_limit"],
                    json!((full_limit > limit).then_some(full_limit))
                );
            }
        }
    }
    let report = s.deps("src/main.rs", Direction::Both, 8);
    assert!(serde_json::to_vec(&report).unwrap().len() < 2000);
    let missing = s.deps("missing", Direction::Both, 0);
    assert_eq!(missing.total, 0);
    assert!(!missing.truncated);
    assert_eq!(missing.next_limit, None);
    assert_eq!(
        serde_json::to_value(missing).unwrap()["directions"],
        json!({
            "in": {"total": 0, "returned": 0, "omitted": 0},
            "out": {"total": 0, "returned": 0, "omitted": 0}
        })
    );
}

#[test]
fn deps_cli_is_deterministic_and_reports_directional_truncation() {
    let map = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/tolmap.json");
    let run = |direction: &str, limit: &str, format: &str| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tolmap"))
            .args([
                "deps",
                "src/progress.rs",
                "--direction",
                direction,
                "--limit",
                limit,
                "--format",
                format,
                "--map",
            ])
            .arg(&map)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        output.stdout
    };
    assert_eq!(run("both", "2", "json"), run("both", "2", "json"));
    let text = String::from_utf8(run("both", "1", "text")).unwrap();
    assert!(text.contains("in (importers): 1 of 14 results\n"));
    assert!(text.contains("out (imports): 1 of 2 results\n"));
    assert!(text.contains("in (importers): 13 rows omitted; repeat with --limit 14"));
    assert!(text.contains("out (imports): 1 rows omitted; repeat with --limit 2"));
    assert!(text.contains("\nin src/blobs.rs ["));
    assert!(text.contains("\nout src/extract.rs ["));
    let text = String::from_utf8(run("both", "2", "text")).unwrap();
    assert!(text.contains("out (imports): 2 of 2 results\n"));
    assert!(!text
        .lines()
        .any(|line| line.starts_with("out (imports):") && line.contains("omitted")));
    let text = String::from_utf8(run("out", "0", "text")).unwrap();
    assert!(!text.contains("in (importers):"));
    assert!(text.contains("out (imports): 0 of 2 results\n"));
    assert!(text.contains("out (imports): 2 rows omitted; repeat with --limit 2"));
    assert!(!text.contains("\nout src/"));
}

#[test]
fn directory_layout_and_invalid_indices() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("../data/flask.json");
    std::fs::write(temp.path().join("flask.json"), bytes).unwrap();
    std::fs::write(temp.path().join("flask.names.json"), b"{}").unwrap();
    // Known sidecars are never parsed during map-only directory discovery.
    std::fs::write(temp.path().join("flask.symbols.json"), b"not JSON").unwrap();
    assert_eq!(Snapshot::load(temp.path()).unwrap().map.repo, "flask");
    std::fs::write(temp.path().join("other.json"), bytes).unwrap();
    assert!(Snapshot::load(temp.path()).is_err());
    let mut map: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    map["E"] = serde_json::json!([[999999, 0]]);
    std::fs::write(
        temp.path().join("bad.json"),
        serde_json::to_vec(&map).unwrap(),
    )
    .unwrap();
    assert!(Snapshot::load(&temp.path().join("bad.json")).is_err());
}

#[test]
fn overview_and_routes_on_pinned_maps() {
    for name in ["tolmap", "flask"] {
        let s = fixture(name);
        let overview = s.overview(1000);
        assert_eq!(overview.total, s.map.districts.len());
        let files: usize = overview
            .results
            .iter()
            .map(|r| match r {
                QueryResult::District { files, .. } => *files,
                _ => panic!("expected district"),
            })
            .sum();
        assert_eq!(files, s.map.files.len());
        let [a, b] = s.map.edges[0];
        let route = s.route(&s.map.files[a], &s.map.files[b], 8);
        assert_eq!(route.total, 2);
        assert!(matches!(
            route.query,
            Query::Route {
                edge_kind: Some(tolmap::lens::RouteKind::Imports),
                ..
            }
        ));
        assert!(
            matches!(&route.results[0], QueryResult::Step { path, .. } if path == &s.map.files[a])
        );
        let short = s.route(&s.map.files[a], &s.map.files[b], 1);
        assert_eq!(short.total, 2);
        assert!(short.truncated);
        assert_eq!(short.omitted, 1);
        assert_eq!(short.next_limit, Some(2));
        let same = s.route(&s.map.files[a], &s.map.files[a], 8);
        assert_eq!(same.total, 1);
        assert!(matches!(
            same.query,
            Query::Route {
                edge_kind: Some(tolmap::lens::RouteKind::SameFile),
                ..
            }
        ));
        assert_eq!(s.route("missing", &s.map.files[a], 8).total, 0);
        assert!(serde_json::to_vec(&s.overview(8)).unwrap().len() < 2000);
    }
}

#[test]
fn route_direction_and_no_route_use_lens_semantics() {
    let mut s = fixture("tolmap");
    s.map.edges = vec![[0, 1], [2, 1]];
    let route = s.route(&s.map.files[1], &s.map.files[0], 8);
    assert!(matches!(
        route.query,
        Query::Route {
            edge_kind: Some(tolmap::lens::RouteKind::ImportedBy),
            ..
        }
    ));
    let route = s.route(&s.map.files[0], &s.map.files[2], 8);
    assert!(matches!(
        route.query,
        Query::Route {
            edge_kind: Some(tolmap::lens::RouteKind::Undirected),
            ..
        }
    ));
    assert_eq!(route.total, 3);
    assert_eq!(s.route(&s.map.files[0], &s.map.files[3], 8).total, 0);
}

#[test]
fn cli_exit_codes_and_text() {
    let binary = env!("CARGO_BIN_EXE_tolmap");
    let map = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/tolmap.json");
    for args in [
        vec!["overview"],
        vec!["where", "missing"],
        vec!["deps", "src/main.rs"],
        vec!["route", "src/main.rs", "src/lib.rs"],
    ] {
        let output = std::process::Command::new(binary)
            .args(args)
            .arg("--map")
            .arg(&map)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema"], "tolmap.query/v3");
        assert_eq!(
            report.get("directions").is_some(),
            report["query"]["kind"] == "deps"
        );
        if report["query"]["kind"] == "overview" {
            assert_eq!(
                report["results"].as_array().unwrap().len(),
                report["total"].as_u64().unwrap() as usize
            );
            assert_eq!(report["truncated"], false);
        } else {
            assert_eq!(report["limit"], 25);
        }
    }
    let limited = std::process::Command::new(binary)
        .args(["overview", "--limit", "0", "--format", "text", "--map"])
        .arg(&map)
        .output()
        .unwrap();
    assert_eq!(limited.status.code(), Some(0));
    let limited = String::from_utf8(limited.stdout).unwrap();
    assert!(limited.contains("rows omitted; repeat with --limit"));
    let text = std::process::Command::new(binary)
        .args(["where", "main", "--format", "text", "--map"])
        .arg(&map)
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(0));
    assert!(String::from_utf8(text.stdout)
        .unwrap()
        .contains("ambiguous"));
    let bad = std::process::Command::new(binary)
        .args(["overview", "--map", "/nonexistent/tolmap-query-input"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty());
    #[cfg(unix)]
    {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let internal = std::process::Command::new(binary)
            .args(["overview", "--map"])
            .arg(&map)
            .stdout(full)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(internal.code(), Some(3));
    }
}

#[test]
fn hierarchical_symbols_and_unknown_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let map = temp.path().join("flask.json");
    std::fs::write(&map, include_bytes!("../data/flask.json")).unwrap();
    let symbols = serde_json::json!({
        "files": [0], "symbols": [[0,"App",0,1,20,-1,5,false], [0,"run",1,2,5,0,3,false], [0,"run",1,22,25,-1,3,false]],
        "edges": [], "module_code_lines": {},
        "coverage": {"calls_total": 7, "calls_resolved": 3, "unresolved": {"dynamic": 4}}
    });
    std::fs::write(
        map.with_extension("symbols.json"),
        serde_json::to_vec(&symbols).unwrap(),
    )
    .unwrap();
    let s = Snapshot::load(&map).unwrap();
    let report = s.where_is("run", 0).unwrap();
    assert_eq!(report.total, 2);
    assert!(report.truncated);
    assert!(matches!(
        report.query,
        Query::Where {
            ambiguous: true,
            ..
        }
    ));
    let report = s.where_is("App::run", 8).unwrap();
    assert_eq!(report.total, 1);
    assert_eq!(report.coverage.calls_total, Some(7));
    assert_eq!(report.coverage.unresolved.unwrap()["dynamic"], 4);
    assert!(s
        .where_is(&s.map.files[0], 8)
        .unwrap()
        .coverage
        .calls_total
        .is_none());
    let mut malformed = symbols;
    malformed["symbols"][0][5] = serde_json::json!(1);
    std::fs::write(
        map.with_extension("symbols.json"),
        serde_json::to_vec(&malformed).unwrap(),
    )
    .unwrap();
    assert!(s
        .where_is("run", 8)
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    assert_eq!(s.where_is(&s.map.files[0], 8).unwrap().total, 1);
}
