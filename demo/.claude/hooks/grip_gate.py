#!/usr/bin/env python3
"""GripApprover の PreToolUse hook

ツール名と入力から必要な握り回数を決め、デーモンに問い合わせて
permissionDecision を返す。標準ライブラリのみで動く。
"""
import json
import re
import sys
import urllib.parse
import urllib.request

DAEMON_URL = "http://127.0.0.1:8766/wait"

DANGEROUS_PATTERNS = [
    r"\brm\b[^|;&]*\s(-[a-z]*[rf][a-z]*|--recursive|--force)\b",
    r"\bgit\s+push\b[^|;&]*\s(--force(-with-lease)?|-f)\b",
    r"\bgit\s+reset\b[^|;&]*\s--hard\b",
    r"\bgit\s+clean\b[^|;&]*\s-[a-z]*f",
    r"\bsudo\b",
    r"\bdrop\s+(table|database)\b",
    r"\bchmod\s+-r\s+777\b",
    r"\bcurl\b[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b",
]
EDIT_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}


def is_dangerous(command):
    return any(re.search(p, command, re.IGNORECASE) for p in DANGEROUS_PATTERNS)


def required_reps(tool_name, tool_input):
    if tool_name == "Bash":
        return 50 if is_dangerous(tool_input.get("command", "")) else 10
    if tool_name in EDIT_TOOLS or tool_name.startswith("mcp__"):
        return 5
    if tool_name == "WebFetch":
        return 3
    return 0


def make_label(tool_name, tool_input):
    detail = tool_input.get("command") or tool_input.get("file_path") or tool_input.get("url") or ""
    detail = " ".join(str(detail).split())
    if len(detail) > 40:
        detail = detail[:39] + "…"
    return f"{tool_name}: {detail}" if detail else tool_name


def decide(decision, reason):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }, ensure_ascii=False))


def main():
    try:
        event = json.load(sys.stdin)
        tool_name = event.get("tool_name", "")
        tool_input = event.get("tool_input") or {}
        reps = required_reps(tool_name, tool_input)
        if reps == 0:
            return  # 通常の権限フローに任せる

        timeout = min(15 + reps * 3, 110)
        query = urllib.parse.urlencode({
            "reps": reps,
            "timeout": timeout,
            "label": make_label(tool_name, tool_input),
        })
        with urllib.request.urlopen(f"{DAEMON_URL}?{query}", timeout=timeout + 5) as res:
            data = json.load(res)
    except Exception as e:  # noqa: BLE001
        decide("ask", f"GripApprover に接続できないため、通常の確認に切り替えます（{e.__class__.__name__}）")
        return

    result = data.get("result")
    done = data.get("done", 0)
    if result == "approved":
        decide("allow", f"{reps}回の握力で承認されました")
    elif result == "timeout":
        decide("deny", f"握力が足りませんでした（{done}/{reps}回）")
    else:
        decide("ask", "グリップが接続されていないため、通常の確認に切り替えます")


if __name__ == "__main__":
    main()
