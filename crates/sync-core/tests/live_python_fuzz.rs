//! Optional, non-hermetic heavy fuzz: 200k random doubles + 20k random ints
//! normalized by Rust and by a live CPython, compared byte-for-byte.
//!
//! Ignored by default (needs python3 on PATH; the checked-in fixture corpus
//! already covers parity hermetically). Run with:
//!
//! ```sh
//! cargo test -p sync-core --test live_python_fuzz -- --ignored --nocapture
//! ```

use std::io::Write as _;
use std::process::Command;

fn splitmix64(z: &mut u64) -> u64 {
    *z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = *z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[test]
#[ignore = "needs python3; fixture corpus covers this hermetically"]
fn fuzz_200k_floats_against_live_cpython() {
    let seed: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    println!("fuzz seed: {seed:#x}");
    let mut state = seed;

    let mut floats = Vec::with_capacity(200_000);
    while floats.len() < 200_000 {
        let v = f64::from_bits(splitmix64(&mut state));
        if v.is_finite() {
            floats.push(serde_json::Value::from(v));
        }
    }
    let mut ints = Vec::with_capacity(20_000);
    for _ in 0..10_000 {
        ints.push(serde_json::Value::from(splitmix64(&mut state))); // u64
        ints.push(serde_json::Value::from(splitmix64(&mut state) as i64));
    }
    let doc = serde_json::json!({ "floats": floats, "ints": ints });
    let rust_normalized = {
        let mut s = sync_core::dumps(&doc);
        s.push('\n');
        s
    };

    // Feed Rust's output to CPython; its re-dump must be byte-identical.
    let dir = tempfile::tempdir().unwrap();
    let input_path = dir.path().join("input.json");
    std::fs::File::create(&input_path)
        .unwrap()
        .write_all(rust_normalized.as_bytes())
        .unwrap();
    let py = r#"
import json, sys
raw = open(sys.argv[1], "rb").read().decode("utf-8")
out = json.dumps(json.loads(raw), sort_keys=True, indent=2, ensure_ascii=False) + "\n"
sys.stdout.buffer.write(out.encode("utf-8"))
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(py)
        .arg(&input_path)
        .output()
        .expect("python3 must be runnable for this ignored test");
    assert!(
        output.status.success(),
        "python failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python_normalized = String::from_utf8(output.stdout).unwrap();
    if rust_normalized != python_normalized {
        let pos = rust_normalized
            .bytes()
            .zip(python_normalized.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let lo = pos.saturating_sub(80);
        panic!(
            "divergence at byte {pos} (seed {seed:#x})\n  rust:   {}\n  python: {}",
            &rust_normalized[lo..(pos + 80).min(rust_normalized.len())],
            &python_normalized[lo..(pos + 80).min(python_normalized.len())],
        );
    }
    println!("220k numbers: byte-identical with CPython");
}
