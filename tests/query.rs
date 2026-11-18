use std::path::Path;

use tolmap::query::Snapshot;
use tolmap::query_contract::{Direction, Query, QueryResult, QUERY_SCHEMA};

fn fixture(name: &str) -> Snapshot {
    Snapshot::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("data/{name}.json")))
        .unwrap()
}

#[test]
fn pinned_file_symbol_and_ambiguity() {
    let s = fixture("tolmap");
    let file = s.where_is("src/main.rs", 8).unwrap();
    assert_eq!(QUERY_SCHEMA, "tolmap.query/v3");
    assert_eq!(file.schema, QUERY_SCHEMA);
    assert_eq!(file.returned, file.results.len());
    assert!(file.directions.is_none());
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
fn pinned_deps_match_all_edges_and_keep_total() {
    for name in ["tolmap", "flask"] {
        let s = fixture(name);
        let path = &s.map.files[1];
        for direction in [Direction::In, Direction::Out, Direction::Both] {
            let report = s.deps(path, direction, 1);
            let expected: std::collections::BTreeSet<_> = s
                .map
                .edges
                .iter()
                .flat_map(|&[a, b]| {
                    let mut rows = Vec::new();
                    if a == 1 && direction != Direction::In {
                        rows.push((b, 1));
                    }
                    if b == 1 && direction != Direction::Out {
                        rows.push((a, 0));
                    }
                    rows
                })
                .collect();
            let in_total = expected.iter().filter(|(_, d)| *d == 0).count();
            let out_total = expected.len() - in_total;
            let returned = in_total.min(1) + out_total.min(1);
            let largest = in_total.max(out_total);
            assert_eq!(report.total, expected.len());
            assert_eq!(report.returned, returned);
            assert_eq!(report.truncated, expected.len() > returned);
            assert_eq!(report.omitted, expected.len() - returned);
            assert_eq!(report.next_limit, (largest > 1).then_some(largest));
            assert_eq!(report.results.len(), returned);
            assert_eq!(
                serde_json::to_vec(&report).unwrap(),
                serde_json::to_vec(&s.deps(path, direction, 1)).unwrap()
            );
        }
    }
    let s = fixture("tolmap");
    let report = s.deps("src/main.rs", Direction::Both, 8);
    assert!(serde_json::to_vec(&report).unwrap().len() < 2000);
    assert_eq!(s.deps("missing", Direction::Both, 8).total, 0);
}

#[test]
fn pinned_asymmetric_deps_limit_each_direction() {
    let s = fixture("tolmap");
    for (path, totals) in [("src/progress.rs", [14, 2]), ("src/geometry.rs", [2, 10])] {
        let file = s.map.files.iter().position(|p| p == path).unwrap();
        let mut expected = [
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        ];
        for &[a, b] in &s.map.edges {
            if b == file {
                expected[0].insert(s.map.files[a].clone());
            }
            if a == file {
                expected[1].insert(s.map.files[b].clone());
            }
        }
        assert_eq!([expected[0].len(), expected[1].len()], totals);
        for direction in [Direction::Both, Direction::In, Direction::Out] {
            for limit in [0, 1, 2, usize::MAX] {
                let report = s.deps(path, direction, limit);
                let metadata = report.directions.as_ref().unwrap();
                let mut expected_rows = Vec::new();
                let mut total = 0;
                let mut returned = 0;
                let mut largest = 0;
                let text = tolmap::query::text(&report);
                for (d, name, requested, counts) in [
                    (0, "in", direction != Direction::Out, &metadata.r#in),
                    (1, "out", direction != Direction::In, &metadata.out),
                ] {
                    if !requested {
                        assert!(counts.is_none());
                        assert!(!text.contains(&format!("\n{name}:")));
                        continue;
                    }
                    let counts = counts.as_ref().unwrap();
                    let n = totals[d].min(limit);
                    assert_eq!(counts.total, totals[d]);
                    assert_eq!(counts.returned, n);
                    assert_eq!(counts.omitted, totals[d] - n);
                    total += totals[d];
                    returned += n;
                    largest = largest.max(totals[d]);
                    expected_rows
                        .extend(expected[d].iter().take(limit).map(|p| (p.as_str(), name)));
                    assert!(text.contains(&format!("{name}: {n} of {} results", totals[d])));
                    let notice = format!(
                        "{name}: {} rows omitted; repeat with --limit {} to return all {name} rows",
                        totals[d] - n,
                        totals[d]
                    );
                    assert_eq!(text.contains(&notice), n < totals[d]);
                }
                expected_rows.sort();
                let rows: Vec<_> = report
                    .results
                    .iter()
                    .map(|row| match row {
                        QueryResult::Dependency {
                            path, direction, ..
                        } => (
                            path.as_str(),
                            if *direction == Direction::In {
                                "in"
                            } else {
                                "out"
                            },
                        ),
                        _ => panic!("expected dependency"),
                    })
                    .collect();
                assert_eq!(rows, expected_rows);
                assert_eq!(report.total, total);
                assert_eq!(report.returned, returned);
                assert_eq!(report.returned, report.results.len());
                assert_eq!(report.omitted, total - returned);
                assert_eq!(report.total, report.returned + report.omitted);
                assert_eq!(report.truncated, total > returned);
                assert_eq!(report.next_limit, (total > returned).then_some(largest));
                let json = serde_json::to_vec(&report).unwrap();
                assert_eq!(
                    json,
                    serde_json::to_vec(&s.deps(path, direction, limit)).unwrap()
                );
                let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
                assert_eq!(
                    json["directions"]["in"].is_null(),
                    direction == Direction::Out
                );
                assert_eq!(
                    json["directions"]["out"].is_null(),
                    direction == Direction::In
                );
                if let Some(next_limit) = report.next_limit {
                    let complete = s.deps(path, direction, next_limit);
                    assert_eq!(complete.returned, total);
                    assert!(!complete.truncated);
                }
            }
        }
    }
    let missing = s.deps("missing", Direction::Both, 0);
    assert_eq!(missing.total, 0);
    assert_eq!(missing.returned, 0);
    assert_eq!(missing.omitted, 0);
    assert!(!missing.truncated);
    assert_eq!(missing.next_limit, None);
    let directions = missing.directions.unwrap();
    assert_eq!(directions.r#in.unwrap().total, 0);
    assert_eq!(directions.out.unwrap().total, 0);
}

#[test]
fn deps_deduplicate_and_order_reciprocal_rows() {
    let mut s = fixture("tolmap");
    s.map.edges = vec![[0, 1], [1, 0], [0, 1], [2, 0], [0, 2]];
    let report = s.deps(&s.map.files[0], Direction::Both, 1);
    assert_eq!(report.total, 4);
    assert_eq!(report.returned, 2);
    let first = [&s.map.files[1], &s.map.files[2]]
        .into_iter()
        .min()
        .unwrap();
    assert!(
        matches!(&report.results[0], QueryResult::Dependency { path, direction: Direction::In, .. } if path == first)
    );
    assert!(
        matches!(&report.results[1], QueryResult::Dependency { path, direction: Direction::Out, .. } if path == first)
    );
    s.map.edges.reverse();
    assert_eq!(
        serde_json::to_vec(&report).unwrap(),
        serde_json::to_vec(&s.deps(&s.map.files[0], Direction::Both, 1)).unwrap()
    );
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
        assert_eq!(overview.returned, overview.total);
        assert!(overview.directions.is_none());
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
        assert_eq!(short.returned, 1);
        assert!(short.directions.is_none());
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
        assert_eq!(report["schema"], QUERY_SCHEMA);
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
    let deps = || {
        std::process::Command::new(binary)
            .args([
                "deps",
                "src/progress.rs",
                "--direction",
                "both",
                "--limit",
                "2",
                "--map",
            ])
            .arg(&map)
            .output()
            .unwrap()
    };
    let output = deps();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert_eq!(output.stdout, deps().stdout);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["returned"], 4);
    assert_eq!(report["directions"]["out"]["returned"], 2);
    let output = std::process::Command::new(binary)
        .args([
            "deps",
            "src/progress.rs",
            "--limit",
            "2",
            "--format",
            "text",
            "--map",
        ])
        .arg(&map)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("in: 2 of 14 results (truncated)"));
    assert!(text.contains("in: 12 rows omitted; repeat with --limit 14"));
    assert!(text.contains("out: 2 of 2 results\n"));
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
