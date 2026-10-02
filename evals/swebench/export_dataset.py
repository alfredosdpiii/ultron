"""Export SWE-bench Verified to a JSONL file the Node harness reads.

Run with the private virtualenv's Python (the harness does this itself):

    python export_dataset.py <out.jsonl> [dataset] [split]

One line per instance: the fields an agent run needs (instance id, repo, base commit, problem statement) plus the name
of the official prebuilt instance image, taken from the dataset's `image` field so the harness never guesses it. The
gold patch, test patch, hints and failing-test names are deliberately not exported: nothing the agents can reach holds
them.
"""

import json
import sys

from datasets import load_dataset

# swebench 5 reads the image name and eval script from the dataset itself, so it needs the SWE-bench organisation's
# copy of Verified. Same 500 instances as princeton-nlp/SWE-bench_Verified: ids, base commits, problem statements,
# gold and test patches and FAIL_TO_PASS lists are identical (checked 2026-10-02; PASS_TO_PASS differs for
# astropy__astropy-7606 and django__django-10097).
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
                        "version": row["version"],
                        "base_commit": row["base_commit"],
                        "problem_statement": row["problem_statement"],
                        "image": row["image"],
                    }
                )
                + "\n"
            )
    print(f"{len(rows)} instances -> {out}")


if __name__ == "__main__":
    main()
