//! Replays crates/resources/fixtures/prompts.json (recorded from the TypeScript sources by generate.mjs):
//! the Rust loaders and prompt builders must produce identical text on the same temp trees.

use resources::Diagnostic;
use resources::context_files::load_project_context_files;
use resources::prompt_files::load_prompt_files;
use resources::prompt_templates::{expand_prompt_template, load_prompt_templates, parse_command_args, substitute_args};
use resources::runtime_guide::{
    EventDelivery, ExtensionToolSummary, RuntimeGuideOptions, rlm_runtime_prompt, rlm_tool_guidelines,
};
use resources::skills::{LoadSkillsOptions, Skill, apply_always_skills, load_skills, load_skills_from_dir};
use resources::system_prompt::{
    BuildOptions, DocPaths, build_system_prompt, build_system_prompt_sections, diff_system_prompt_sections,
};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

fn fixture() -> Value {
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/prompts.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn bundled_dir() -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/coding-agent/skills");
    fs::canonicalize(dir).unwrap().to_string_lossy().into_owned()
}

/// Build a tree spec in a fresh temp dir; returns (guard, canonical root).
fn make_tree(spec: &Value) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
    for entry in spec.as_array().unwrap() {
        let path = entry[0].as_str().unwrap();
        let full = Path::new(&root).join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        if entry[1].is_null() {
            std::os::unix::fs::symlink(entry[2].as_str().unwrap(), &full).unwrap();
        } else if !path.ends_with("/.dir") {
            fs::write(&full, entry[1].as_str().unwrap()).unwrap();
        }
    }
    (dir, root)
}

fn scrub(text: &str, root: &str) -> String {
    text.replace(root, "$ROOT").replace(&bundled_dir(), "$BUNDLED")
}

fn diag_json(d: &Diagnostic, root: &str) -> Value {
    json!({
        "type": d.kind,
        "message": scrub(&d.message, root),
        "path": d.path.as_ref().map(|p| scrub(p, root)),
        "collision": d.collision.as_ref().map(|(w, l)| json!([scrub(w, root), scrub(l, root)])),
    })
}

fn skill_json(s: &Skill, root: &str) -> Value {
    json!({
        "name": s.name,
        "description": s.description,
        "filePath": scrub(&s.file_path, root),
        "baseDir": scrub(&s.base_dir, root),
        // createSyntheticSourceInfo records an explicit path's scope as "temporary".
        "source": if s.source == "path" { "temporary" } else { s.source.as_str() },
        "disableModelInvocation": s.disable_model_invocation,
        "alwaysOnContent": s.always_on_content,
    })
}

/// YAML parser messages differ between `yaml` and yaml-rust2: compare everything else.
fn without_yaml_messages(mut diagnostics: Value) -> Value {
    for d in diagnostics.as_array_mut().unwrap() {
        if d["path"]
            .as_str()
            .is_some_and(|p| p.contains("badyaml") || p.ends_with("bad.md"))
        {
            d["message"] = json!("<yaml error>");
        }
    }
    diagnostics
}

/// Equality with the first divergence shown, instead of two pages of text.
fn assert_text(actual: &str, expected: &str, what: &str) {
    if actual != expected {
        let at = actual.bytes().zip(expected.bytes()).take_while(|(a, b)| a == b).count();
        let from = at.saturating_sub(80);
        panic!(
            "{what}: differs at byte {at}\n  rust: {:?}\n  ts:   {:?}",
            actual.get(from..(at + 80).min(actual.len())),
            expected.get(from..(at + 80).min(expected.len()))
        );
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn resources_context_files_match_typescript() {
    let fx = fixture();
    for case in fx["context"].as_array().unwrap() {
        let (_guard, root) = make_tree(&fx["trees"][case["tree"].as_str().unwrap()]);
        if case["git"] == json!(true) {
            let repo = format!("{root}/repo");
            for args in [
                vec!["init", "-q"],
                vec!["add", "."],
                vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"],
                vec!["worktree", "add", "-q", "--detach", "wt/one"],
            ] {
                let status = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .status()
                    .unwrap();
                assert!(status.success());
            }
        }
        let cwd = format!("{root}/{}", case["cwd"].as_str().unwrap());
        let agent = format!("{root}/{}", case["agentDir"].as_str().unwrap());
        let files: Vec<Value> = load_project_context_files(&cwd, &agent)
            .iter()
            .map(|f| json!({"path": scrub(&f.path, &root), "content": f.content}))
            .collect();
        assert_eq!(Value::Array(files), case["files"], "cwd {}", case["cwd"]);
    }
}

#[test]
fn resources_skill_loading_matches_typescript() {
    let fx = fixture();
    for case in fx["skills"].as_array().unwrap() {
        let (_guard, root) = make_tree(&fx["trees"]["skills"]);
        let paths: Vec<String> = strings(&case["skillPaths"])
            .iter()
            .map(|p| p.replace("$ROOT", &root))
            .collect();
        let result = load_skills(&LoadSkillsOptions {
            cwd: &format!("{root}/proj"),
            agent_dir: &format!("{root}/agent"),
            skill_paths: &paths,
            include_defaults: case["includeDefaults"].as_bool().unwrap(),
        });
        let skills: Vec<Value> = result.skills.iter().map(|s| skill_json(s, &root)).collect();
        assert_eq!(Value::Array(skills), case["skills"], "case {:?}", case["skillPaths"]);
        let diagnostics: Vec<Value> = result.diagnostics.iter().map(|d| diag_json(d, &root)).collect();
        assert_eq!(
            without_yaml_messages(Value::Array(diagnostics)),
            without_yaml_messages(case["diagnostics"].clone())
        );
    }
}

/// The skills and context the TS prompt cases were built from, loaded by the Rust code.
fn prompt_base(fx: &Value, root: &str) -> (Vec<Skill>, Vec<Skill>, Vec<resources::context_files::ContextFile>) {
    let mut loaded = load_skills(&LoadSkillsOptions {
        cwd: &format!("{root}/proj"),
        agent_dir: &format!("{root}/agent"),
        skill_paths: &[],
        include_defaults: true,
    })
    .skills;
    let diagnostics = apply_always_skills(&mut loaded, &strings(&fx["always"]["names"]));
    let diagnostics: Vec<Value> = diagnostics.iter().map(|d| diag_json(d, root)).collect();
    assert_eq!(Value::Array(diagnostics), fx["always"]["diagnostics"]);
    let always: Vec<Value> = loaded.iter().map(|s| skill_json(s, root)).collect();
    assert_eq!(Value::Array(always), fx["always"]["skills"]);
    let bundled: Vec<Skill> = load_skills_from_dir(&bundled_dir(), "bundled")
        .skills
        .into_iter()
        .map(|s| Skill {
            disable_model_invocation: true,
            ..s
        })
        .collect();
    assert_eq!(bundled.len() as u64, fx["bundledCount"].as_u64().unwrap());
    let context = load_project_context_files(&format!("{root}/proj"), &format!("{root}/agent"));
    (loaded, bundled, context)
}

#[test]
fn resources_system_prompts_match_typescript() {
    let fx = fixture();
    let (_guard, root) = make_tree(&fx["trees"]["skills"]);
    let (loaded, bundled, context) = prompt_base(&fx, &root);
    let docs = &fx["docPaths"];
    let doc_paths = DocPaths {
        readme: docs["readme"].as_str().unwrap().to_string(),
        docs: docs["docs"].as_str().unwrap().to_string(),
        examples: docs["examples"].as_str().unwrap().to_string(),
    };
    let mut checked = 0;
    for case in fx["prompts"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let mut options = BuildOptions::new(&format!("{root}/proj"));
        if let Some(cwd) = input["cwd"].as_str() {
            options.cwd = cwd.to_string();
        }
        if input["selectedTools"].is_array() {
            options.selected_tools = strings(&input["selectedTools"]);
        }
        options.tool_snippets = pairs(&input["toolSnippets"]);
        options.tool_guidelines = input["toolGuidelines"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), strings(v))).collect())
            .unwrap_or_default();
        options.prompt_guidelines = strings(&input["promptGuidelines"]);
        options.custom_prompt = input["customPrompt"].as_str().map(String::from);
        options.append_system_prompt = input["appendSystemPrompt"].as_str().unwrap_or("").to_string();
        options.sections = pairs(&input["sections"]);
        options.engineering = input["engineering"] == json!(true);
        options.include_harness_docs = input["includeHarnessDocs"] != json!(false);
        options.doc_paths = doc_paths.clone();
        options.bundled_skills_dir = Some(bundled_dir());
        options.skills = match case["skillsFrom"].as_str() {
            Some("all") => loaded.iter().chain(&bundled).cloned().collect(),
            Some("hidden") => loaded.iter().filter(|s| s.disable_model_invocation).cloned().collect(),
            _ => vec![],
        };
        options.context_files = if case["contextFrom"] == json!("loaded") {
            context.clone()
        } else {
            vec![]
        };
        if let Some(error) = case["error"].as_str() {
            let bad = name.strip_prefix("invalid:").unwrap();
            let options = BuildOptions {
                sections: vec![(bad.to_string(), "x".to_string())],
                ..BuildOptions::new("/")
            };
            assert_eq!(build_system_prompt_sections(&options).unwrap_err(), error);
            checked += 1;
            continue;
        }
        let text = build_system_prompt(&options, None).unwrap();
        assert_text(&scrub(&text, &root), case["text"].as_str().unwrap(), name);
        let sections: Vec<Value> = build_system_prompt_sections(&options)
            .unwrap()
            .iter()
            .map(|(k, v)| json!([k, scrub(v, &root)]))
            .collect();
        assert_eq!(Value::Array(sections), case["sections"], "prompt case {name}");
        checked += 1;
    }
    assert_eq!(checked, 15);

    // diffSystemPromptSections between the defaults and an rlm-only prompt with an addendum.
    let mut a = BuildOptions::new(&format!("{root}/proj"));
    a.skills = loaded.iter().chain(&bundled).cloned().collect();
    a.context_files = context;
    a.doc_paths = doc_paths;
    a.bundled_skills_dir = Some(bundled_dir());
    let b = BuildOptions {
        selected_tools: vec!["rlm".to_string()],
        append_system_prompt: "Now appended.".to_string(),
        ..a.clone()
    };
    let (sa, sb) = (
        build_system_prompt_sections(&a).unwrap(),
        build_system_prompt_sections(&b).unwrap(),
    );
    let patch = diff_system_prompt_sections(&sa, &sb).unwrap();
    let patch: serde_json::Map<String, Value> = patch
        .into_iter()
        .map(|(k, v)| (k, v.map_or(Value::Null, |t| json!(scrub(&t, &root)))))
        .collect();
    assert_eq!(Value::Object(patch), fx["diffs"][0]["patch"]);
    assert_eq!(diff_system_prompt_sections(&sa, &sa), None);
    assert!(fx["diffs"][1]["patch"].is_null());
}

#[test]
fn resources_runtime_guide_matches_typescript() {
    let fx = fixture();
    for case in fx["runtime"].as_array().unwrap() {
        let tools = strings(&case["tools"]);
        let tools: Vec<&str> = tools.iter().map(String::as_str).collect();
        let o = &case["options"];
        let delivery = match o["delivery"].as_str() {
            Some("next-call") => EventDelivery::NextCall,
            Some("off") => EventDelivery::Off,
            Some(_) => EventDelivery::Wake,
            None if o["asyncEvents"] == json!(false) => EventDelivery::Off,
            None => EventDelivery::Wake,
        };
        let options = RuntimeGuideOptions {
            delivery,
            external_root: o["externalRoot"] == json!(true),
            hints: o["hints"] == json!(true),
            extension_tools: o["extensionTools"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| ExtensionToolSummary {
                            name: t["name"].as_str().unwrap().to_string(),
                            description: t["description"].as_str().unwrap().to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            mcp_servers: strings(&o["mcpServers"]),
            native_extension_tools: strings(&o["nativeExtensionTools"]),
            // The TS guide never names the Rust-only APIs (its host does not serve them).
            durable_apis: false,
        };
        match case["text"].as_str() {
            Some(expected) => assert_text(
                &rlm_runtime_prompt(&tools, &options).unwrap(),
                expected,
                "runtime guide",
            ),
            None => assert_eq!(rlm_runtime_prompt(&tools, &options), None),
        }
    }
    for case in fx["guidelines"].as_array().unwrap() {
        let tools = strings(&case["tools"]);
        let tools: Vec<&str> = tools.iter().map(String::as_str).collect();
        assert_eq!(rlm_tool_guidelines(&tools), strings(&case["lines"]));
    }
}

#[test]
fn resources_prompt_templates_match_typescript() {
    let fx = fixture();
    let (_guard, root) = make_tree(&fx["trees"]["prompts"]);
    let (cwd, agent) = (format!("{root}/proj"), format!("{root}/agent"));
    for case in fx["templates"].as_array().unwrap() {
        let paths: Vec<String> = strings(&case["promptPaths"])
            .iter()
            .map(|p| p.replace("$ROOT", &root))
            .collect();
        let result = load_prompt_templates(&cwd, &agent, &paths, case["includeDefaults"].as_bool().unwrap());
        let templates: Vec<Value> = result
            .templates
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "argumentHint": t.argument_hint,
                    "content": t.content,
                    "scope": if t.scope == "path" { "temporary" } else { t.scope },
                    "filePath": scrub(&t.file_path, &root),
                })
            })
            .collect();
        assert_eq!(
            Value::Array(templates),
            case["templates"],
            "paths {:?}",
            case["promptPaths"]
        );
        let diagnostics: Vec<Value> = result.diagnostics.iter().map(|d| diag_json(d, &root)).collect();
        assert_eq!(
            without_yaml_messages(Value::Array(diagnostics)),
            without_yaml_messages(case["diagnostics"].clone())
        );
    }
    let loaded = load_prompt_templates(&cwd, &agent, &[], true).templates;
    for case in fx["expand"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            expand_prompt_template(text, &loaded),
            case["out"].as_str().unwrap(),
            "expand {text:?}"
        );
    }
}

#[test]
fn resources_template_arguments_match_typescript() {
    let fx = fixture();
    for case in fx["parseArgs"].as_array().unwrap() {
        assert_eq!(
            parse_command_args(case["input"].as_str().unwrap()),
            strings(&case["args"]),
            "{}",
            case["input"]
        );
    }
    for case in fx["substitute"].as_array().unwrap() {
        let (content, args) = (case["content"].as_str().unwrap(), strings(&case["args"]));
        assert_eq!(
            substitute_args(content, &args),
            case["out"].as_str().unwrap(),
            "{content:?} {args:?}"
        );
    }
}

#[test]
fn resources_system_prompt_files_match_typescript() {
    let fx = fixture();
    let (_guard, root) = make_tree(&fx["trees"]["systemFiles"]);
    for case in fx["promptFiles"].as_array().unwrap() {
        let fix = |v: &Value| v.as_str().map(|s| s.replace("$ROOT", &root));
        let system = fix(&case["systemPrompt"]);
        let appends: Option<Vec<String>> = case["appendSystemPrompt"]
            .as_array()
            .map(|a| a.iter().filter_map(fix).collect());
        let files = load_prompt_files(
            &format!("{root}/{}", case["cwd"].as_str().unwrap()),
            &format!("{root}/{}", case["agentDir"].as_str().unwrap()),
            case["trusted"].as_bool().unwrap(),
            system.as_deref(),
            appends.as_deref(),
        );
        let actual = json!({
            "systemPrompt": files.system_prompt.map(|s| scrub(&s, &root)),
            "systemPromptSource": files.system_prompt_source.map(|s| scrub(&s, &root)),
            "appendSystemPrompt": files.append_system_prompt.iter().map(|s| scrub(s, &root)).collect::<Vec<_>>(),
            "appendSources": files.append_sources.iter().map(|s| scrub(s, &root)).collect::<Vec<_>>(),
        });
        assert_eq!(actual, case["out"], "case {case}");
    }
}

#[test]
fn resources_runtime_guide_names_the_swarm_apis_only_when_the_host_serves_them() {
    let tools = ["rlm"];
    let plain = rlm_runtime_prompt(&tools, &RuntimeGuideOptions::default()).unwrap();
    let swarm = rlm_runtime_prompt(
        &tools,
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!plain.contains("rlm.ask"), "the TS guide has no swarm section");
    for needle in [
        "keep=True",
        "await rlm.ask(h, question)",
        "swarm.route",
        "board",
        "notes",
        "help(swarm)",
    ] {
        assert!(swarm.contains(needle), "{needle}");
    }
    // The section follows the delegation section; with the typed-memory sentence removed too, nothing else changes.
    let at = swarm.find("Persistent agents:").unwrap();
    assert!(swarm[..at].contains("## Delegation"));
    let end = at + swarm[at..].find("`help(swarm)`.").unwrap() + "`help(swarm)`.".len();
    let typed = swarm.find(" A fact tied to files can be typed:").unwrap();
    let typed_end =
        typed + swarm[typed..].find("(`await memory.get(id)`).").unwrap() + "(`await memory.get(id)`).".len();
    assert!(swarm[typed..typed_end].contains("path_globs"));
    let without = format!("{}{}{}", &swarm[..at - 1], &swarm[end..typed], &swarm[typed_end..]);
    assert_eq!(without, plain);
}
