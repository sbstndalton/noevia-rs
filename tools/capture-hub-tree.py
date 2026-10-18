#!/usr/bin/env python3
"""Capture the public `expand=true` tree listing of one model-hub repository (noevia#1999).

    python3 -I tools/capture-hub-tree.py <repo-id> <out.json> [max-pages]

Sends the request the native model manager sends (`GET {API}/models/<repo>/tree/main?recursive=true&expand=true`) and
follows each `Link: <...>; rel="next"` header like `hf.repo_detail`, at most `max-pages` pages (default 40, the
manager's `_TREE_MAX_PAGES`). Public metadata only: no token or other credential is ever sent, and no model file
is requested. The pages are written verbatim, as the Hub sent them, inside a small wrapper object
(see bins/noevia-model-manager/tests/fixtures/hub-tree/README.md).
"""
from __future__ import annotations

import datetime
import json
import re
import sys
import time
import urllib.request

# The model-hub API endpoint: the same value as HUB_API in bins/noevia-model-manager/src/hub_page.rs.
API = "https://huggingface.co/api"
NEXT = re.compile(r'<([^>]+)>\s*;\s*rel="next"')


def main(argv: list[str]) -> int:
    if len(argv) not in (3, 4):
        print(__doc__, file=sys.stderr)
        return 2
    repo, out = argv[1], argv[2]
    limit = int(argv[3]) if len(argv) == 4 else 40
    first = f"{API}/models/{repo}/tree/main?recursive=true&expand=true"
    url, pages = first, []
    while url and len(pages) < limit:
        if not url.startswith(f"{API}/models/"):
            raise SystemExit("refusing to follow a link to another origin")
        request = urllib.request.Request(url, headers={"User-Agent": "tree-fixture-capture"})
        with urllib.request.urlopen(request, timeout=60) as response:
            body = response.read().decode("utf-8")
            link = response.headers.get("Link", "")
        json.loads(body)
        pages.append(body)
        match = NEXT.search(link)
        url = match.group(1) if match else None
        time.sleep(1)
    header = json.dumps(
        {
            "captured": datetime.date.today().isoformat(),
            "request": first,
            "complete": url is None,
        }
    )[:-1]
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write(header + ', "pages": [' + ",\n".join(pages) + "]}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
