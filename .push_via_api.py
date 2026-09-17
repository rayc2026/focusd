"""git push 被代理拦截时的兜底：用 GitHub Git Data API 直接提交。

用法：python .push_via_api.py <commit message 文件> <相对路径1> [相对路径2 ...]
大文件一律走 base64 blob + --input 文件（避免命令行参数过长 / 非 Base64 判定）。
"""
import base64
import json
import os
import subprocess
import sys
import tempfile

REPO = "rayc2026/focusd"
BRANCH = "main"


def gh(args, jq=None):
    cmd = ["gh", "api"] + args
    if jq:
        cmd += ["--jq", jq]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("gh api 失败: %s\n%s\n%s" % (cmd, r.stdout, r.stderr))
    return r.stdout.strip()


def gh_input(args, body: dict, jq=None):
    fd, path = tempfile.mkstemp(suffix=".json")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(body, f, ensure_ascii=False)
    try:
        return gh(["--input", path.replace("\\", "/")] + args, jq=jq)
    finally:
        os.unlink(path)


def main():
    msg_file = sys.argv[1]
    paths = sys.argv[2:]
    with open(msg_file, encoding="utf-8") as f:
        message = f.read().strip()

    base = gh(["repos/%s/git/ref/heads/%s" % (REPO, BRANCH)], jq=".object.sha")
    print("remote %s = %s" % (BRANCH, base))

    tree = []
    for p in paths:
        with open(p, "rb") as f:
            raw = f.read()
        blob_sha = gh_input(
            ["-X", "POST", "repos/%s/git/blobs" % REPO],
            {"content": base64.b64encode(raw).decode("ascii"), "encoding": "base64"},
            jq=".sha",
        )
        tree.append({"path": p.replace("\\", "/"), "mode": "100644", "type": "blob", "sha": blob_sha})
        print("blob %s -> %s" % (p, blob_sha))

    tree_sha = gh_input(
        ["-X", "POST", "repos/%s/git/trees" % REPO],
        {"base_tree": base, "tree": tree},
        jq=".sha",
    )
    print("tree = %s" % tree_sha)

    commit_sha = gh_input(
        ["-X", "POST", "repos/%s/git/commits" % REPO],
        {"message": message, "tree": tree_sha, "parents": [base]},
        jq=".sha",
    )
    print("commit = %s" % commit_sha)

    gh_input(
        ["-X", "PATCH", "repos/%s/git/refs/heads/%s" % (REPO, BRANCH)],
        {"sha": commit_sha},
        jq=".object.sha",
    )
    print("pushed %s -> %s" % (BRANCH, commit_sha))


if __name__ == "__main__":
    main()
