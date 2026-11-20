"""校验 docs/notes-feature-plan.md 里引用的 Rust 文件与符号是否真实存在。

文档里的引用写错不会让编译失败，但会误导读者。先前的 `wav_writer.rs` 路径
与 `history_service.rs:810` 这类行号就是这样失准的，所以定期跑一遍。
"""

import glob
import io
import os
import re
import sys

DOC = "docs/notes-feature-plan.md"


def main() -> int:
    if not os.path.exists(DOC):
        print(f"找不到 {DOC}")
        return 1

    text = io.open(DOC, encoding="utf-8").read()
    sources = [
        p.replace(os.sep, "/")
        for p in glob.glob("src-tauri/src/**/*.rs", recursive=True)
    ]
    contents = {}
    for path in sources:
        try:
            contents[path] = io.open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue

    # 前端源码也参与符号校验（`notesStore::createNoteFromHistory` 这类引用）
    frontend = [
        p.replace(os.sep, "/")
        for p in glob.glob("src/**/*.ts", recursive=True)
        + glob.glob("src/**/*.tsx", recursive=True)
    ]
    for path in frontend:
        try:
            contents[path] = io.open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue

    problems = []

    # 文件引用：`services/notes_service.rs`、`src/db.rs` 这类
    file_refs = set(re.findall(r"`([a-zA-Z0-9_/]+\.rs)(?:::[a-zA-Z0-9_]+)?`", text))
    for ref in sorted(file_refs):
        if not any(path.endswith(ref) or path.endswith("/" + ref) for path in sources):
            problems.append(f"文件不存在: {ref}")

    # 符号引用：`notes_service::create_note_from_history` 这类
    symbol_refs = set(re.findall(r"`([a-zA-Z0-9_]+)::([a-zA-Z0-9_]+)`", text))
    for module, symbol in sorted(symbol_refs):
        pattern = re.compile(
            r"\b(fn|struct|enum|const|static|trait|type|export|function|async function)\s+"
            + re.escape(symbol)
            + r"\b"
        )
        if not any(
            module in path and pattern.search(body)
            for path, body in contents.items()
        ):
            problems.append(f"符号不存在: {module}::{symbol}")

    # 行号引用：`foo.rs:123` 这种一改就漂移，不该出现在文档里
    for match in re.findall(r"`([a-zA-Z0-9_/]+\.rs):\d+", text):
        problems.append(f"行号引用（会漂移）: {match}")

    print(f"检查 {DOC}：{len(file_refs)} 个文件引用，{len(symbol_refs)} 个符号引用")
    if problems:
        for item in problems:
            print("  -", item)
        return 1
    print("全部引用有效")
    return 0


if __name__ == "__main__":
    sys.exit(main())
