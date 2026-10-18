//! Fixture helpers shared by driver tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;
use wardex_core::EventKind;
use wardex_drivers::runner::{Output, Protocol};

pub struct Record {
    pub dir: String,
    pub msg: Value,
}

pub fn fixture_path(driver: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(driver)
        .join(format!("{name}.jsonl"))
}

pub fn load(driver: &str, name: &str) -> Vec<Record> {
    let text = std::fs::read_to_string(fixture_path(driver, name)).unwrap();
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Record {
                dir: v["dir"].as_str().unwrap().to_string(),
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

/// Feed every `out` record to the protocol; collect events and writes.
pub fn feed_all<P: Protocol>(p: &mut P, records: &[Record]) -> (Vec<EventKind>, Vec<Value>) {
    let mut events = Vec::new();
    let mut writes = Vec::new();
    for r in records.iter().filter(|r| r.dir == "out") {
        let out = p.on_line(&r.msg.to_string());
        collect(out, &mut events, &mut writes);
    }
    (events, writes)
}

pub fn collect(out: Output, events: &mut Vec<EventKind>, writes: &mut Vec<Value>) {
    events.extend(out.events);
    writes.extend(out.writes.iter().map(|w| serde_json::from_str(w).unwrap()));
}

pub fn names(events: &[EventKind]) -> Vec<&'static str> {
    events
        .iter()
        .filter(|e| !e.is_delta())
        .map(|e| e.name())
        .collect()
}

/// A shell script that behaves like the recorded CLI: prints each `out`
/// record and waits for one stdin line per `in` record.
pub fn replay_script(records: &[Record], dir: &Path) -> PathBuf {
    let mut script = String::from("#!/bin/sh\n");
    for r in records {
        match r.dir.as_str() {
            "out" => {
                let line = r.msg.to_string().replace('\'', "'\\''");
                script.push_str(&format!("printf '%s\\n' '{line}'\n"));
            }
            "in" => script.push_str("IFS= read -r _ || exit 0\n"),
            "exit" => {
                let code = r.msg.as_i64().unwrap_or(0);
                script.push_str(&format!("exit {code}\n"));
            }
            _ => {}
        }
    }
    let path = dir.join("replay.sh");
    std::fs::write(&path, script).unwrap();
    path
}
