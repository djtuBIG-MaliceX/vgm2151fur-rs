use vgm2fur::parity::{format_f, fmt_g, fmt_float_repr, py_round_to};

#[derive(serde::Deserialize)]
struct Row {
    k: String,
    b: u64,
    #[serde(default)]
    p: usize,
    #[serde(default)]
    n: i32,
    s: String,
}

#[test]
fn matches_python_formatting() {
    let raw = std::fs::read_to_string("tests/data/py_fmt.json").expect("baseline json");
    let rows: Vec<Row> = serde_json::from_str(&raw).unwrap();
    let mut bad = 0;
    for r in &rows {
        let got = match r.k.as_str() {
            "g" => fmt_g(f64::from_bits(r.b), r.p),
            "f" => format_f(f64::from_bits(r.b), r.p),
            "r" => fmt_float_repr(py_round_to(f64::from_bits(r.b), r.n)),
            _ => unreachable!(),
        };
        if got != r.s {
            bad += 1;
            if bad < 40 {
                println!("MISMATCH k={} v={:?} p={} n={} want={:?} got={:?}", r.k, f64::from_bits(r.b), r.p, r.n, r.s, got);
            }
        }
    }
    println!("rows={} bad={}", rows.len(), bad);
    assert_eq!(bad, 0, "formatting mismatches");
}
