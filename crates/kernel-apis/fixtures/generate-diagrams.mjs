// Records what the vendored Pele (crates/kernel-apis/assets/pele/pele.js) renders for one sample of each diagram kind,
// for crates/kernel-apis/tests/diagram_contrast.rs: a fake node answers these SVGs, so the default suite never runs
// node itself. Run: node crates/kernel-apis/fixtures/generate-diagrams.mjs (twice, same sha256 of diagrams.json).
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { render } from "../assets/pele/pele.js";

const samples = [
	'pie title Pets\n  "Dogs" : 386\n  "Cats" : 85\n  "Rats" : 15\n  "Birds" : 60\n  "Fish" : 70\n  "Frogs" : 50\n  "Snakes" : 45\n  "Mice" : 40',
	"flowchart LR\n  A[Start] --> B{Ready?}\n  B -->|yes| C[Ship]\n  B -->|no| A\n  C --> D((Done))",
	"flowchart TB\n  subgraph one\n    subgraph two\n      a1-->a2\n    end\n  end",
	"sequenceDiagram\n  participant Alice\n  participant Bob\n  Alice->>Bob: Hello\n  activate Bob\n  Note over Alice,Bob: A note\n  loop Every minute\n    Bob-->>Alice: Hi\n  end\n  deactivate Bob",
	"classDiagram\n  Animal <|-- Duck\n  Animal : +int age\n  class Duck{\n    +swim()\n  }",
	"stateDiagram-v2\n  [*] --> Still\n  Still --> Moving\n  Moving --> Crash\n  Crash --> [*]\n  note right of Still : resting",
	"erDiagram\n  CUSTOMER ||--o{ ORDER : places\n  ORDER ||--|{ LINE-ITEM : contains",
	"gantt\n  title A Gantt\n  dateFormat YYYY-MM-DD\n  section One\n  Task a :done, a1, 2024-01-01, 10d\n  Task b :active, after a1, 5d\n  Crit c :crit, 2024-01-20, 3d\n  Milestone :milestone, m1, 2024-01-25, 0d",
	"journey\n  title My day\n  section Work\n    Code: 5: Me\n    Review: 3: Me, Cat\n  section Home\n    Sleep: 1: Me",
	"mindmap\n  root((Ultron))\n    REPL\n      Python\n    TUI\n    Host",
	"timeline\n  title History\n  2002 : LinkedIn\n  2004 : Facebook : Google\n  2005 : YouTube",
	"quadrantChart\n  title Reach\n  x-axis Low --> High\n  y-axis Low --> High\n  quadrant-1 Expand\n  quadrant-2 Promote\n  quadrant-3 Re-evaluate\n  quadrant-4 Improve\n  Campaign A: [0.3, 0.6]\n  Campaign B: [0.45, 0.23]",
	'xychart-beta\n  title "Sales"\n  x-axis [jan, feb, mar]\n  y-axis "Revenue" 0 --> 100\n  bar [50, 60, 75]\n  line [40, 70, 80]',
	"gitGraph\n  commit\n  branch develop\n  checkout develop\n  commit\n  commit\n  checkout main\n  merge develop\n  commit",
	"sankey-beta\n  A,B,10\n  A,C,5\n  B,D,7",
	"block-beta\n  columns 3\n  a b c\n  d:2 e",
	"kanban\n  Todo\n    t1[Write tests]\n  Doing\n    t2[Fix contrast]\n  Done\n    t3[Ship]",
	'packet-beta\n  0-15: "Source Port"\n  16-31: "Destination Port"\n  32-63: "Sequence Number"',
	"architecture-beta\n  group api(cloud)[API]\n  service db(database)[Database] in api\n  service server(server)[Server] in api\n  db:L -- R:server",
	"requirementDiagram\n  requirement test_req {\n    id: 1\n    text: the test text.\n    risk: high\n    verifymethod: test\n  }\n  element test_entity {\n    type: simulation\n  }\n  test_entity - satisfies -> test_req",
	'radar-beta\n  axis A, B, C\n  curve c1{1,2,3}\n  curve c2{3,2,1}',
	'treemap-beta\n  "Section 1"\n    "Leaf 1.1": 12\n    "Leaf 1.2": 7\n  "Section 2"\n    "Leaf 2.1": 20',
	"venn-beta\n  set A\n  set B\n  union A,B",
];

const out = [];
for (const text of samples) {
	try {
		const r = render(text, { responsive: false });
		out.push({ text, type: r.type, svg: r.svg });
	} catch (error) {
		process.stderr.write(`skipped (${String(error?.message ?? error).split("\n")[0]}): ${text.split("\n")[0]}\n`);
	}
}
const file = new URL("./diagrams.json", import.meta.url);
writeFileSync(file, `${JSON.stringify({ pele: "1.7.0", samples: out }, null, "\t")}\n`);
assertOffline();
process.stdout.write(`${out.length} samples\n`);
