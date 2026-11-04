//! The Rust against the lab's Python as oracle, on the vectors of `scripts/prguard_dourado.py` (llm-trainer).
//!
//! - `cut_diff`: equal to `corta` on raw git diffs, byte for byte;
//! - ids and `[OPT]` positions: equal to the training collator, token for token (one token off changes the answer);
//! - probabilities: the int8 ONNX in Rust against the same int8 in Python's ONNX Runtime (same weights, tight), and
//!   against the PyTorch model the run was evaluated with (int8 error, tolerance measured on 400 test items).
//!
//! The weights (150 MB) do not go into the repository: the tests that need them look in `PR_GUARD_MODEL` (or the
//! default cache dir) and skip with a warning when it is missing. `cut_diff` needs nothing.

use std::path::PathBuf;

use pr_guard::model::{self, Encoder, Model};
use serde::Deserialize;

/// |p(Rust int8) - p(Python int8)|: same ONNX, another ONNX Runtime build (measured 5.7e-8 on the Mac).
const SAME_WEIGHTS: f64 = 1e-5;
/// |p(Rust int8) - p(PyTorch fp32)|: the max over the 400 test items in Python was 0.149 (`summary_400.max_abs_p`; mean
/// 0.007, p99 0.078), with 8-bit MatMul weights in blocks of 32. Dynamic per-tensor int8 reached 0.64 and was dropped.
const INT8_VS_FP32: f64 = 0.15;

#[derive(Deserialize)]
struct Item {
    description: String,
    diff: String,
    label: Option<u8>,
    ids: Vec<u32>,
    opt_positions: Vec<usize>,
    p_torch: f64,
    p_int8: f64,
}

#[derive(Deserialize)]
struct Golden {
    instructions: String,
    criteria: Vec<String>,
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Cut {
    raw: String,
    cut: String,
}

#[derive(Deserialize)]
struct Cuts {
    items: Vec<Cut>,
}

fn read<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn model_dir() -> Option<PathBuf> {
    // The lab-format package the vectors were made with, in a directory named by $PR_GUARD_MODEL.
    let d = PathBuf::from(std::env::var_os("PR_GUARD_MODEL")?);
    if d.join("pr_guard_model.json").exists() {
        Some(d)
    } else {
        eprintln!("WARNING: no model in {} (set PR_GUARD_MODEL): test skipped", d.display());
        None
    }
}

fn auc(p: &[f64], y: &[u8]) -> f64 {
    let (mut s, mut n) = (0.0, 0.0);
    for (a, ya) in p.iter().zip(y) {
        for (b, yb) in p.iter().zip(y) {
            if *ya == 1 && *yb == 0 {
                s += if a > b { 1.0 } else if a == b { 0.5 } else { 0.0 };
                n += 1.0;
            }
        }
    }
    s / n
}

#[test]
fn the_diff_cut_is_the_labs() {
    let c: Cuts = read("diff_cut.json");
    for (i, it) in c.items.iter().enumerate() {
        assert_eq!(model::cut_diff(&it.raw, model::DIFF_CHARS), it.cut, "item {i}: cut_diff differs from corta");
    }
    eprintln!("{} raw diffs: cut_diff equal to corta", c.items.len());
}

#[test]
fn the_question_text_is_the_trainings() {
    let g: Golden = read("description.json");
    assert_eq!(g.instructions, model::DESCRIPTION_INSTRUCTIONS);
    assert_eq!(g.criteria, model::DESCRIPTION_CRITERIA);
}

#[test]
fn ids_equal_the_python_collator() {
    let Some(dir) = model_dir() else { return };
    let g: Golden = read("description.json");
    let enc = Encoder::load(&dir).unwrap();
    for (i, it) in g.items.iter().enumerate() {
        let e = enc.description(&it.description, &it.diff).unwrap();
        assert_eq!(e.ids, it.ids, "item {i}: ids differ from Python");
        assert_eq!(e.opt_positions, it.opt_positions, "item {i}: [OPT] positions differ");
    }
    let lens: Vec<usize> = g.items.iter().map(|i| i.ids.len()).collect();
    eprintln!("{} items, {}..{} tokens: ids equal to Python", g.items.len(), lens.iter().min().unwrap(), lens.iter().max().unwrap());
}

#[test]
fn probabilities_match_python_int8_and_stay_near_pytorch() {
    let Some(dir) = model_dir() else { return };
    let g: Golden = read("description.json");
    let mut m = Model::load(&dir, 4).unwrap();
    let (mut d8, mut d32, mut flips) = (0.0f64, 0.0f64, 0);
    let (mut pr, mut pt, mut y) = (vec![], vec![], vec![]);
    for (i, it) in g.items.iter().enumerate() {
        let p = m.noul(&m.enc.description(&it.description, &it.diff).unwrap()).unwrap();
        d8 = d8.max((p - it.p_int8).abs());
        d32 = d32.max((p - it.p_torch).abs());
        flips += usize::from((p > 0.5) != (it.p_torch > 0.5));
        assert!((p - it.p_int8).abs() < SAME_WEIGHTS, "item {i}: {p} vs Python int8 {}", it.p_int8);
        assert!((p - it.p_torch).abs() < INT8_VS_FP32, "item {i}: {p} vs PyTorch {}", it.p_torch);
        if let Some(l) = it.label {
            pr.push(p);
            pt.push(it.p_torch);
            y.push(l);
        }
    }
    eprintln!(
        "{} items: max |p - Python int8| {d8:.2e}, max |p - PyTorch| {d32:.4}, {flips} flips at 0.5; AUC on {} labeled: Rust int8 {:.4}, PyTorch {:.4}",
        g.items.len(),
        y.len(),
        auc(&pr, &y),
        auc(&pt, &y)
    );
}
