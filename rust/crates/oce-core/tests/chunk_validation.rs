//! 真实语料的切块不变式抽查。
//!
//! 默认 `#[ignore]`：需要 `OCE_VALIDATE_DIR` 指向一个真实仓库（如 flask / cc-switch
//! 的检出），逐个源文件跑 `chunk::validate::validate_chunks`，要求零违规。
//!
//! 这是 REQ-6 的真语料证据：单元测试只能证明不变式本身，这个测试证明**现有切块器
//! 在真实代码上不丢行**——"某个函数体没进候选池"这类问题在这一层就能暴露。
//!
//! ```text
//! OCE_VALIDATE_DIR=../oce-benchmark/repos/flask \
//!   cargo test -p oce-core --test chunk_validation -- --ignored --nocapture
//! ```

use std::path::Path;

use oce_core::chunk::validate::inspect_chunks;
use oce_core::chunk::Chunker;

const SKIP_DIRS: [&str; 6] = [".git", "node_modules", "target", "dist", "build", "coverage"];
const MAX_BYTES: u64 = 1_000_000;

fn collect_files(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect_files(&path, out);
            }
            continue;
        }
        if path.metadata().map(|m| m.len()).unwrap_or(u64::MAX) <= MAX_BYTES {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "需要 OCE_VALIDATE_DIR 指向真实仓库检出"]
fn real_corpus_chunks_satisfy_invariants() {
    let Ok(dir) = std::env::var("OCE_VALIDATE_DIR") else {
        panic!("set OCE_VALIDATE_DIR to a repository checkout");
    };
    let root = Path::new(&dir);
    assert!(root.is_dir(), "not a directory: {dir}");

    let chunker = oce_core::chunk::build_chunker().expect("build chunker");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    collect_files(root, &mut files);

    let mut text_files = 0usize;
    let mut chunks_total = 0usize;
    let mut violations: Vec<String> = Vec::new();
    let mut boundary_overlaps = 0usize;
    let mut uncovered_lines = 0usize;
    let mut files_with_gaps: Vec<String> = Vec::new();
    for path in &files {
        let Ok(raw) = std::fs::read(path) else {
            continue;
        };
        if raw.contains(&0) {
            continue;
        }
        let Ok(content) = String::from_utf8(raw) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let chunks = chunker.chunk(&content, &relative);
        // OCE_DUMP_SPANS=1 时把每个文件的 chunk 跨度打成 TSV（供外部对比 unit 粒度）
        if std::env::var("OCE_DUMP_SPANS").is_ok() {
            for chunk in &chunks {
                println!(
                    "SPAN\t{relative}\t{}\t{}\t{}",
                    chunk.start_line,
                    chunk.end_line,
                    chunk.chunk_type.clone().unwrap_or_default()
                );
            }
        }
        text_files += 1;
        chunks_total += chunks.len();
        let inspection = inspect_chunks(&content, &chunks);
        boundary_overlaps += inspection.boundary_overlaps.len();
        if !inspection.uncovered.is_empty() {
            if std::env::var("OCE_VALIDATE_VERBOSE").is_ok() {
                for chunk in &chunks {
                    println!(
                        "  SPAN {} [{}-{}] {}",
                        relative,
                        chunk.start_line,
                        chunk.end_line,
                        chunk.chunk_type.clone().unwrap_or_default()
                    );
                }
            }
            uncovered_lines += inspection.uncovered.len();
            files_with_gaps.push(format!(
                "{relative}: {} line(s), first {:?}",
                inspection.uncovered.len(),
                inspection.uncovered.iter().take(5).collect::<Vec<_>>()
            ));
        }
        for reason in &inspection.hard {
            violations.push(format!("{relative}: {reason}"));
        }
    }

    println!(
        "validated {text_files} text files / {chunks_total} chunks under {dir}: \
         {} hard violation(s), {boundary_overlaps} boundary overlap(s), \
         {uncovered_lines} uncovered line(s) in {} file(s)",
        violations.len(),
        files_with_gaps.len()
    );
    for line in violations.iter().take(20) {
        println!("  HARD {line}");
    }
    for line in files_with_gaps.iter().take(20) {
        println!("  WARN {line}");
    }
    assert!(
        violations.is_empty(),
        "{} file(s) violate chunk invariants",
        violations.len()
    );
}
