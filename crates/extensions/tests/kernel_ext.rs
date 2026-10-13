//! The runtime's `ext` namespace (rlm/ext_api.py) against the real runtime.py in crates/kernel, with the Rust
//! handlers: the listing reaches the kernel through [`kernel_api::kernel_env`], `help(ext)` lists the functions,
//! `await ext.<extension>.<function>(**kwargs)` goes through `ext.call` ([`kernel_api::handle`]) to the extension
//! process, `await ext.refresh()` reloads through `ext.list`, and `ext` is absent with no extension functions.

use extensions::{ExtensionHost, NoHost, SessionInfo, kernel_api, parse_manifest};
use kernel::{ExecutionResult, HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn load_hello(dir: &Path) -> ExtensionHost {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello/hello.py");
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    // The startup timeout is an input no test here exercises; long so a loaded machine never trips it.
    let text =
        json!({ "name": "hello", "command": python, "args": [script.to_string_lossy()], "startupTimeoutMs": 120_000 })
            .to_string();
    let path = dir.join("extension.json");
    std::fs::write(&path, &text).unwrap();
    let session = SessionInfo {
        cwd: "/work".into(),
        mode: "print".into(),
        has_ui: false,
        flags: Map::new(),
    };
    ExtensionHost::load(vec![parse_manifest(&path, &text).unwrap()], session, Arc::new(NoHost))
}

fn kernel(dir: &Path, env: Option<(String, String)>, handler: Arc<dyn HostHandler>) -> Kernel {
    let mut options = KernelOptions::new(dir.to_path_buf(), runtime());
    let private = dir.join("agent").display().to_string();
    options.env = vec![
        ("ULTRON_CODING_AGENT_DIR".into(), private.clone()),
        ("ULTRON_SERVER_DIR".into(), private),
    ];
    options.env.extend(env);
    Kernel::new(options, handler).unwrap()
}

fn ok(kernel: &Kernel, code: &str) -> ExecutionResult {
    let result = kernel.execute(code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result
}

fn value(kernel: &Kernel, code: &str) -> String {
    ok(kernel, code).result.unwrap_or_default()
}

fn raised(kernel: &Kernel, call: &str) -> String {
    let code = format!("try:\n    {call}\nexcept Exception as e:\n    print(f'{{type(e).__name__}}: {{e}}')");
    ok(kernel, &code).stdout.trim_end().to_string()
}

const HELLO: &str = "ext.hello.greet(name: string) - Greet someone
ext.hello.slow(**kwargs) - Sleep, stopping early when cancelled
ext.hello.cancelled(**kwargs) - Request ids the host cancelled
ext.hello.crash(**kwargs) - Exit without answering
ext.hello.info(**kwargs) - Process facts
ext.hello.shout(**kwargs) - Emit on the extension bus";

#[test]
fn extensions_ext_namespace_calls_extension_functions_from_a_cell() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(load_hello(dir.path()));
    let env = kernel_api::kernel_env(&host.functions()).unwrap();
    assert_eq!(env.0, "ULTRON_EXT_FUNCTIONS");
    let served = host.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
        kernel_api::handle(
            &served,
            kind,
            &Value::Object(payload),
            Some(Duration::from_secs(120)),
            None,
        )
        .unwrap_or_else(|| Err(extensions::RpcError::Other(format!("Unknown host request: {kind}"))))
        .map_err(|e| e.to_string())
    });
    let k = kernel(dir.path(), Some(env), handler);

    // help(ext) lists every function with its signature and description, as the Rust guide section does.
    let help = ok(&k, "help(ext)").stdout;
    assert!(
        help.contains(&format!(" |  {}\n", HELLO.replace('\n', "\n |  "))),
        "{help}"
    );
    let guide = kernel_api::guide_section(&host.functions()).unwrap();
    for line in HELLO.lines() {
        assert!(guide.contains(&format!("- {line}")), "{guide}");
    }
    assert_eq!(value(&k, "dir(ext)"), "['hello', 'refresh']");
    assert_eq!(
        value(&k, "dir(ext.hello)"),
        "['cancelled', 'crash', 'greet', 'info', 'shout', 'slow']"
    );
    assert_eq!(
        value(&k, "ext.hello.greet.__doc__.splitlines()[0]"),
        "'ext.hello.greet(name: string) - Greet someone'"
    );

    // A call goes to the extension process and returns a ToolResult: the text, with .details.
    assert_eq!(
        value(&k, "r = await ext.hello.greet(name='REPL')\n(str(r), r.name, r.ok)"),
        "('Hello, REPL!', 'ext.hello.greet', True)"
    );
    assert_eq!(
        value(&k, "r = await ext.hello.info()\n(str(r), sorted(r.details))"),
        "('', ['bus', 'pid', 'prompts', 'restart', 'running', 'session'])"
    );

    // Unknown names fail in Python with what exists; positional arguments are refused.
    assert_eq!(
        raised(&k, "ext.hello.nope"),
        "AttributeError: extension hello has no function nope (it has: cancelled, crash, greet, info, shout, slow); help(ext) lists them"
    );
    assert_eq!(
        raised(&k, "ext.nope"),
        "AttributeError: no extension nope (loaded: hello); help(ext) lists their functions"
    );
    assert!(raised(&k, "await ext.hello.greet('x')").starts_with("TypeError: "));

    // refresh reloads through ext.list, whose JSON-schema parameters give the same signatures.
    assert_eq!(
        value(&k, "'\\n'.join(await ext.refresh())"),
        format!("{:?}", HELLO).replace('"', "'")
    );
    assert_eq!(value(&k, "str(await ext.hello.greet(name='again'))"), "'Hello, again!'");
    assert!(kernel_api::handle(&host, "rlm.spawn", &json!({}), None, None).is_none());
    host.shutdown();
}

#[test]
fn extensions_ext_namespace_maps_names_and_raises_on_error_results() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
        log.lock().unwrap().push(json!({ "kind": kind, "payload": payload }));
        match payload["function"].as_str() {
            Some("do-it") => Ok(
                json!({ "content": [{ "type": "text", "text": "a" }, { "type": "image", "data": "" }, { "type": "text", "text": "b" }], "details": [1] }),
            ),
            _ => Ok(json!({ "content": [{ "type": "text", "text": "bad input" }], "isError": true })),
        }
    });
    let listing = json!([
        { "extension": "my-ext", "name": "do-it", "description": "", "signature": "a: number" },
        { "extension": "my-ext", "name": "fail", "description": "Fails" },
        { "extension": "9bad", "name": "x" },
        { "extension": "ok", "name": "_private" },
    ]);
    let k = kernel(
        dir.path(),
        Some((kernel_api::KERNEL_ENV.into(), listing.to_string())),
        handler,
    );
    assert_eq!(value(&k, "dir(ext)"), "['my_ext', 'refresh']");
    assert_eq!(
        value(&k, "r = await ext.my_ext.do_it(a=1)\n(str(r), r.details)"),
        "('a\\nb', [1])"
    );
    assert_eq!(
        raised(&k, "await ext.my_ext.fail()"),
        "ToolError: ext.my-ext.fail failed: bad input"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            json!({ "kind": "ext.call", "payload": { "extension": "my-ext", "function": "do-it", "arguments": { "a": 1 } } }),
            json!({ "kind": "ext.call", "payload": { "extension": "my-ext", "function": "fail", "arguments": {} } }),
        ]
    );
    assert!(ok(&k, "help(ext)").stdout.contains("ext.my_ext.do_it(a: number)\n"));
}

#[test]
fn extensions_ext_namespace_is_absent_without_extension_functions() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(kernel_api::kernel_env(&[]), None);
    let null: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let k = kernel(dir.path(), None, null.clone());
    assert_eq!(value(&k, "'ext' in globals()"), "False");
    let empty = kernel(dir.path(), Some((kernel_api::KERNEL_ENV.into(), "[]".into())), null);
    assert_eq!(value(&empty, "'ext' in globals()"), "False");
}
