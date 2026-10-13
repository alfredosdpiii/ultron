//! A55 (partial): an agent class defined in a cell is invokable typed end to end, its state persists across cells
//! and snapshots, and a bad return is rejected with the schema error.
//! Ported from packages/coding-agent/test/ultron-agent-classes.test.ts: the cases runtime.py decides alone, on the
//! real kernel with the Rust host stack's refusal of unknown requests behind it: schema derivation from annotations,
//! contracts refused at class definition, and field state (type-checked assignment, required fields, `load`) that
//! needs no model-driven method.
//! Not ported: every case that invokes a model-driven method or registers a definition (`agents.register`,
//! `agents.invoke`, `agents.list`, the definition registry and its journal) and the RPC end-to-end case of
//! ultron-a55-agent-classes.test.ts: the Rust host has no definition registry yet.

mod common;

use common::{null_handler, runtime};
use kernel::{Kernel, KernelOptions, Status};
use serde_json::{Value, json};

fn kernel() -> Kernel {
    Kernel::new(
        KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime()),
        null_handler(),
    )
    .unwrap()
}

fn stdout(kernel: &Kernel, code: &str) -> String {
    let result = kernel.execute(code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.stdout
}

fn evalue(kernel: &Kernel, code: &str) -> String {
    let result = kernel.execute(code, None, None).unwrap();
    assert_eq!(result.status, Status::Error, "{code}: {result:?}");
    result.error.unwrap().evalue
}

/// `toMatchObject`: every key of `want` is in `got` with a matching value (arrays element-wise, same length).
fn matches(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Object(got), Value::Object(want)) => {
            want.iter().all(|(k, w)| got.get(k).is_some_and(|g| matches(g, w)))
        }
        (Value::Array(got), Value::Array(want)) => {
            got.len() == want.len() && got.iter().zip(want).all(|(g, w)| matches(g, w))
        }
        _ => got == want,
    }
}

#[test]
fn acceptance_a55_annotations_become_json_schemas_ported_part() {
    let k = kernel();
    let out = stdout(
        &k,
        r##"import json, enum
from dataclasses import dataclass, field
from typing import Literal, Optional, TypedDict, NotRequired, Annotated, Any

class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"

@dataclass
class Point:
    x: int
    y: float = 0.0
    tags: list[str] = field(default_factory=list)

class Movie(TypedDict):
    title: str
    year: NotRequired[int]

class FakeModel:  # duck-typed pydantic: schema with $defs, $ref and defaults
    @classmethod
    def model_json_schema(cls):
        return {"$defs": {"Inner": {"type": "object", "properties": {"n": {"type": "integer", "default": 1}}}},
                "type": "object", "properties": {"inner": {"$ref": "#/$defs/Inner"}}, "required": ["inner"], "title": "FakeModel"}
    @classmethod
    def model_validate(cls, value):
        return ("validated", value)

cases = {
    "str": str, "int": int, "float": float, "bool": bool, "none": None, "any": Any,
    "list": list[int], "dict": dict[str, bool], "tuple": tuple[int, str], "tuple_var": tuple[int, ...], "set": set[str],
    "optional": Optional[int], "union": int | str, "literal": Literal["a", "b"], "enum": Color,
    "annotated": Annotated[str, "a short title"], "dataclass": Point, "typeddict": Movie, "pydantic": FakeModel,
}
print(json.dumps({name: agent.schema(tp) for name, tp in cases.items()}))"##,
    );
    let schemas: Value = serde_json::from_str(&out).unwrap();
    let want = json!({
        "str": {"type": "string"},
        "int": {"type": "integer"},
        "float": {"type": "number"},
        "bool": {"type": "boolean"},
        "none": {"type": "null"},
        "any": {},
        "list": {"type": "array", "items": {"type": "integer"}},
        "dict": {"type": "object", "additionalProperties": {"type": "boolean"}},
        "tuple": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "string"}], "minItems": 2, "maxItems": 2, "items": false},
        "tuple_var": {"type": "array", "items": {"type": "integer"}},
        "set": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
        "optional": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
        "union": {"anyOf": [{"type": "integer"}, {"type": "string"}]},
        "literal": {"enum": ["a", "b"]},
        "enum": {"enum": ["red", "blue"]},
        "annotated": {"type": "string", "description": "a short title"},
        "dataclass": {
            "type": "object", "title": "Point",
            "properties": {"x": {"type": "integer"}, "y": {"type": "number"}, "tags": {"type": "array"}},
            "required": ["x"], "additionalProperties": false,
        },
        "typeddict": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]},
        "pydantic": {
            "type": "object",
            "properties": {"inner": {"type": "object", "properties": {"n": {"type": "integer"}}}},
            "required": ["inner"],
        },
    });
    assert!(matches(&schemas, &want), "{schemas:#}");
    // Keywords the registry refuses are dropped, and references are inlined.
    let pydantic = schemas["pydantic"].to_string();
    assert!(!pydantic.contains("$ref") && !pydantic.contains("$defs") && !pydantic.contains("default"));
}

#[test]
fn acceptance_a55_unsupported_contracts_fail_at_class_definition_with_the_reason() {
    let k = kernel();
    for (code, want) in [
        (
            "@agent\nclass A:\n    async def m(self, x: bytes) -> str:\n        ...",
            "A.m(x): unsupported annotation 'bytes'",
        ),
        (
            "@agent\nclass B:\n    def m(self) -> str:\n        ...",
            "B.m: a model-driven method must be `async def`",
        ),
        (
            "@agent\nclass C:\n    async def m(self, *items: int) -> str:\n        ...",
            "*args and **kwargs cannot form a typed contract",
        ),
        ("@agent\nclass D:\n    key: str = 'x'", "D.key: field name is reserved"),
        (
            "@agent\nclass E:\n    n: int = 'three'",
            "E.n default must be integer, got str",
        ),
        (
            "@agent\nclass F:\n    async def m(self) -> dict[int, str]:\n        ...",
            "dict keys must be str",
        ),
    ] {
        let message = evalue(&k, code);
        assert!(message.contains(want), "{code}: {message}");
    }
}

#[test]
fn acceptance_a55_fields_are_type_checked_required_and_loadable_ported_part() {
    let k = kernel();
    stdout(
        &k,
        "@agent\nclass Counter:\n    seen: int = 0\n    labels: list[str] = []\nc = Counter(\"main\")\nc.seen += 1\nc.labels.append(\"manual\")",
    );
    assert_eq!(
        stdout(&k, "print(c.seen, c.labels, Counter.instances())"),
        "1 ['manual'] ['main']\n"
    );
    assert_eq!(evalue(&k, "c.seen = \"many\""), "Counter.seen must be integer, got str");
    k.reset_scratch().unwrap();
    // Scratch is fresh, but the declared state reattaches to a new handle of the same instance.
    assert_eq!(
        stdout(
            &k,
            "print('c' in globals())\n@agent\nclass Counter:\n    seen: int = 0\n    labels: list[str] = []\nc = Counter(\"main\")\nprint(c.seen, c.labels, Counter(\"other\").seen)"
        ),
        "False\n1 ['manual'] 0\n"
    );
    assert_eq!(
        k.execute("Counter.load(\"missing\")", None, None)
            .unwrap()
            .error
            .unwrap()
            .ename,
        "KeyError"
    );
    assert_eq!(
        evalue(&k, "@agent\nclass Needs:\n    goal: str\nNeeds(\"x\")"),
        "Needs('x') is missing required field(s): goal"
    );
    assert_eq!(
        stdout(&k, "n = Needs(\"x\", goal=\"ship\")\nprint(Needs.load(\"x\").goal)"),
        "ship\n"
    );
}
