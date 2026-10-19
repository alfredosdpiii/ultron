//! File-linked memory pointers through a session (compose.rs: the session's file hooks observe cells with
//! kernel_apis::memory_pointers over the agent directory's typed record index). A cell keeps a typed pitfall with
//! `memory.propose(..., type=, situation=, path_globs=)` (the Python API of the shipped runtime), a later cell writes a
//! file matching its glob, and a following cell result carries exactly one pointer line, `[memory] <id> pitfall:
//! <situation>`, never the content; `memory.get(id)` then reads the content. The memory server is the fake
//! Hindsight server of crates/hindsight/fixtures on 127.0.0.1.

mod p_session_support;
mod reopen_support;

use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use serde_json::Value;
use session_core::core::Op;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

struct Fake {
    child: Child,
    port: u16,
}

impl Fake {
    fn start() -> Fake {
        let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hindsight/fixtures/fake_hindsight.py");
        let mut child = Command::new(python)
            .arg("-I")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Fake {
            child,
            port: line.trim().parse().unwrap(),
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const CONTENT: &str = "pkill -f matches the tool shell itself; bracket the pattern";
const SITUATION: &str = "killing a background process from bash";

const PROPOSE: &str = "r = await memory.propose('pkill -f matches the tool shell itself; bracket the pattern', [{'ref': 'notes.md'}], scope='project', type='pitfall', key='pkill-self-match', situation='killing a background process from bash', path_globs=['scripts/*.sh'])\nprint('ID=' + r['memoryId'] + ';', r['state'], r['record']['indexed'])";

const WRITE: &str = "from pathlib import Path\nPath('scripts').mkdir(exist_ok=True)\nPath('scripts/run.sh').write_text('echo hi\\n')\nprint('WROTE')";

fn id_of(results: &[String]) -> Option<String> {
    let first = results.first()?;
    let start = first.find("ID=")? + 3;
    let end = first[start..].find(';')? + start;
    Some(first[start..end].to_string())
}

#[test]
fn session_core_memory_pointer_follows_a_matching_file_change() {
    let fake = Fake::start();
    let home = Home::new(None);
    let url = format!("http://127.0.0.1:{}", fake.port);
    let w = home.open(
        Box::new(|request, _| {
            let results: Vec<String> = tool_results(request).iter().map(Value::to_string).collect();
            match results.len() {
                0 => return tool("toolu_0", PROPOSE),
                1 => return tool("toolu_1", WRITE),
                _ => {}
            }
            let pointed = results.iter().any(|r| r.contains("[memory] "));
            if results.last().is_some_and(|r| r.contains("GOT=")) || results.len() > 500 {
                return text("done");
            }
            if pointed {
                let id = id_of(&results).unwrap_or_default();
                return tool(
                    &format!("toolu_{}", results.len()),
                    &format!("g = await memory.get('{id}')\nprint('GOT=' + g['content'], g['record']['type'])"),
                );
            }
            // The check runs in the background after the writing cell; later cells run until one carries the
            // pointer (a hang guard of 500 cells, never a timing expectation).
            tool(&format!("toolu_{}", results.len()), "print('NEXT')")
        }),
        &[("ULTRON_HINDSIGHT_URL", &url)],
        None,
    );
    w.call(Op::PromptAndWait {
        message: "remember and write".into(),
        images: Vec::new(),
    })
    .unwrap();
    let requests = w.requests();
    w.close();
    let results: Vec<String> = tool_results(requests.last().unwrap())
        .iter()
        .map(Value::to_string)
        .collect();
    assert!(
        results[0].contains("stored True") || results[0].contains("accepted True"),
        "{}",
        results[0]
    );
    let id = id_of(&results).expect("the propose cell printed the id");
    assert!(results[1].contains("WROTE"), "{}", results[1]);
    let pointer = format!("[memory] {id} pitfall: {SITUATION}");
    let carrying: Vec<&String> = results.iter().filter(|r| r.contains("[memory] ")).collect();
    assert_eq!(carrying.len(), 1, "one pointer, once: {results:?}");
    assert!(carrying[0].contains(&pointer), "{}", carrying[0]);
    // Only a pointer: the content reached the model only through the propose call's own text and memory.get.
    let at = results.iter().position(|r| r.contains("[memory] ")).unwrap();
    assert!(
        !results[1..=at].iter().any(|r| r.contains("bracket the pattern")),
        "{results:?}"
    );
    let last = results.last().unwrap();
    assert!(last.contains(&format!("GOT={CONTENT} pitfall")), "{last}");
    // The record index is the agent directory's shared file.
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(home.root.join("agent/memory/records.json")).unwrap()).unwrap();
    assert_eq!(index["records"][0]["id"], id.as_str());
    assert_eq!(index["records"][0]["pathGlobs"][0], "scripts/*.sh");
}
