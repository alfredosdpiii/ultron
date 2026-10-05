"""Export SWE-bench Verified, with the gold patches, to the JSONL file the autoreview benchmark reads.

Run with the SWE-bench harness's private virtualenv (the benchmark does this itself):

    python export_dataset.py <out.jsonl> [dataset] [split]

One line per instance: instance id, repo, base commit, problem statement and the gold patch. The SWE-bench harness's
own export (evals/swebench/export_dataset.py) leaves the gold patch out on purpose, because its agents must not
reach it; this benchmark is built from the gold patch, so it keeps a separate export in its own cache directory.
The dataset is read from the local Hugging Face cache when it is there (`HF_DATASETS_OFFLINE=1` is set by the
caller), so no network is needed after `node evals/swebench/run.mjs setup`.
"""

import json
import sys

from datasets import load_dataset

DEFAULT_DATASET = "SWE-bench/SWE-bench_Verified"


def main() -> None:
    out = sys.argv[1]
    dataset = sys.argv[2] if len(sys.argv) > 2 else DEFAULT_DATASET
    split = sys.argv[3] if len(sys.argv) > 3 else "test"
    rows = load_dataset(dataset, split=split)
    with open(out, "w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(
                json.dumps(
                    {
                        "instance_id": row["instance_id"],
                        "repo": row["repo"],
                        "base_commit": row["base_commit"],
                        "problem_statement": row["problem_statement"],
                        "patch": row["patch"],
                    }
                )
                + "\n"
            )
    print(f"{len(rows)} instances -> {out}")


if __name__ == "__main__":
    main()
