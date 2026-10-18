//! Recorded fixtures must stay parseable and in the documented format
//! (`docs/drivers.md`, "Fixture format").

use std::fs;
use std::path::Path;

use serde_json::Value;

fn check_dir(dir: &Path) -> usize {
    let mut files = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            files += 1;
            let text = fs::read_to_string(&path).unwrap();
            assert!(!text.trim().is_empty(), "{} is empty", path.display());
            for (n, line) in text.lines().enumerate() {
                let rec: Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), n + 1));
                let dir = rec["dir"].as_str().unwrap_or_default();
                assert!(
                    matches!(dir, "in" | "out" | "exit"),
                    "{}:{}: bad dir {dir:?}",
                    path.display(),
                    n + 1
                );
                assert!(
                    rec.get("msg").is_some(),
                    "{}:{}: no msg",
                    path.display(),
                    n + 1
                );
            }
        }
    }
    files
}

#[test]
fn fixtures_are_wellformed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    assert!(check_dir(&root.join("claude")) > 0);
    assert!(check_dir(&root.join("codex")) > 0);
}
