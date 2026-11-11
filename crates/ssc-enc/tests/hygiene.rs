//! License and repository hygiene: the attribution required by the BSD-3-Clause sources this crate is ported
//! from is present in every ported file, and build output (which includes binaries compiled from the third-party
//! C sources by oracle/build_oracle.cmd) is not tracked by git.

use std::path::Path;
use std::process::Command;

const UPSTREAM: &str = "Copyright (c) 2026 William Ho (WillyBilly06)";

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = manifest_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// `(rust file, C file)` pairs from the "Ported files" list in NOTICE.
fn ported_files(notice: &str) -> Vec<(String, String)> {
    notice
        .lines()
        .filter_map(|l| {
            let (rs, c) = l.trim().split_once("<-")?;
            let c = c.split_whitespace().next()?;
            if !rs.trim().ends_with(".rs") {
                return None;
            }
            Some((rs.trim().to_owned(), c.rsplit('/').next().unwrap().to_owned()))
        })
        .collect()
}

#[test]
fn notice_covers_every_source_file() {
    let notice = read("NOTICE");
    let ported = ported_files(&notice);
    assert!(ported.len() >= 11, "NOTICE ported-file list not found: {ported:?}");
    for entry in std::fs::read_dir(manifest_dir().join("src")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let new_code: Vec<&str> =
            notice.lines().skip_while(|l| !l.starts_with("New code")).take_while(|l| !l.trim().is_empty()).collect();
        let new_code = new_code.join(" ");
        let listed = ported.iter().any(|(rs, _)| *rs == name) || new_code.contains(&name);
        assert!(listed, "src/{name} is neither a ported nor a new file in NOTICE");
    }
}

#[test]
fn ported_files_carry_upstream_copyright() {
    for (rs, c) in ported_files(&read("NOTICE")) {
        let src = read(&format!("src/{rs}"));
        let head: String = src.lines().take(12).collect::<Vec<_>>().join("\n");
        assert!(head.contains(UPSTREAM), "src/{rs}: header lacks '{UPSTREAM}'");
        assert!(head.contains("BSD-3-Clause"), "src/{rs}: header lacks the license");
        assert!(head.contains(&c), "src/{rs}: header does not name its C source {c}");
    }
}

#[test]
fn adapted_test_code_is_attributed() {
    let oracle = read("oracle/ssc_oracle.c");
    let head: String = oracle.lines().take(10).collect::<Vec<_>>().join("\n");
    assert!(head.contains("William Ho") && head.contains("ssc_conformance.c"), "oracle/ssc_oracle.c header");
    assert!(read("tests/ref_frames.rs").contains("run_tests.py"), "tests/ref_frames.rs header");
    let notice = read("NOTICE");
    for needle in ["tests/ref_frames.rs", "ssc_conformance.c", "make_synthetic", "oracle/ssc_oracle.c"] {
        assert!(notice.contains(needle), "NOTICE does not mention {needle}");
    }
    let l = read("LICENSE-THIRD-PARTY");
    assert!(l.contains("William Ho") && l.contains("Xiph.Org"), "LICENSE-THIRD-PARTY");
}

/// Nothing under the workspace `target/` directory is tracked by git (skipped when git or the repository is not
/// available, e.g. in a packaged crate).
#[test]
fn build_output_is_not_tracked() {
    let root = manifest_dir().join("../..");
    let out = match Command::new("git").arg("-C").arg(&root).args(["ls-files", "--", "target"]).output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("git or repository not available; skipped");
            return;
        }
    };
    let tracked = String::from_utf8_lossy(&out.stdout);
    let n = tracked.lines().count();
    assert_eq!(n, 0, "{n} files under target/ are tracked by git, e.g. {:?}", tracked.lines().next());
    let ignore = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    let ignored = ignore.lines().any(|l| matches!(l.trim(), "target/" | "/target/" | "target" | "/target"));
    assert!(ignored, ".gitignore must ignore target/");
}
