#!/bin/bash
# privacy-guard.sh — refuse to commit, message, or push anything that looks
# like the household's personal data or a secret.
#
# Rule (the owner, 2026-09-06): no personal information, questions, names,
# devices, places, handles, ids, log lines or secrets may EVER reach the
# public repository — in source, tests, docs, changelog, or commit messages.
#
# Generic shapes are matched here. The household-specific list (names,
# handles, device and place names, real LAN addresses, peer/thread ids)
# lives in an UNTRACKED file so this guard can never publish it:
#     ~/.kinai/privacy-denylist.txt   (one extended regex per line)
#
# Usage:  privacy-guard.sh staged           # pre-commit: staged additions
#         privacy-guard.sh msg <file>       # commit-msg: the message text
#         privacy-guard.sh range <a>..<b>   # pre-push / CI: added lines and
#                                           #   messages of every commit in range
# Exit 0 = clean, 1 = blocked. Output names the file/line and the pattern
# NUMBER that matched — never the matching text.
set -u
DENY="${KINAI_PRIVACY_DENYLIST:-$HOME/.kinai/privacy-denylist.txt}"
GENERIC=(
  'bot[0-9]{6,}:[-_A-Za-z0-9]{25,}'                          # Telegram bot token
  '(sk|xai)-[A-Za-z0-9]{20,}'                                 # OpenAI/xAI-style keys
  'AIza[-_A-Za-z0-9]{30,}'                                    # Google API key
  'gh[pousr]_[A-Za-z0-9]{30,}'                                # GitHub tokens
  '2026-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9.]+Z'         # raw log timestamps = pasted log lines
  'peer=[a-z0-9]{6}\b|thread=[0-9a-f]{8}'          # host-log turn fragments
  'chat_id[=:] *[0-9]{6,}'                                    # Telegram chat ids
  '[-_.%+A-Za-z0-9]+@[-A-Za-z0-9]+\.(me|com|de|net|org)\b'  # email addresses
)
ALLOW='noreply@anthropic\.com|users\.noreply\.github\.com|example\.com|Developer ID Application: Your Name'
scan() {  # $1 = label, stdin = text to scan; prints hits, returns 1 if any.
  # Matching is done in Python so the result does not depend on which grep
  # (BSD, GNU, ugrep) happens to be on PATH — BSD grep silently rejected one
  # pattern during self-test and let a token through.
  local label="$1" tmp
  tmp="$(mktemp)"; cat > "$tmp"
  PG_LABEL="$label" PG_DENY="$DENY" PG_ALLOW="$ALLOW" PG_FILE="$tmp" python3 - "${GENERIC[@]}" << 'PY'
import os, re, sys
label, deny, allow = os.environ["PG_LABEL"], os.environ["PG_DENY"], os.environ["PG_ALLOW"]
generic = sys.argv[1:]
text = open(os.environ["PG_FILE"], encoding="utf-8", errors="replace").read()
lines = [l for l in text.splitlines() if not re.search(allow, l)]
hits = 0
for i, pat in enumerate(generic):
    n = sum(1 for l in lines if re.search(pat, l))
    if n: print(f"  BLOCKED {label}: generic pattern #{i} ({n} line(s))"); hits = 1
if os.path.isfile(deny):
    for i, pat in enumerate(open(deny, encoding="utf-8"), 1):
        pat = pat.strip()
        if not pat or pat.startswith("#"): continue
        # smart-case: an entry containing a capital letter is matched exactly
        # (a capitalised first name does not match the same word used as a
        # common noun); an all-lowercase entry matches every casing.
        flags = 0 if re.search(r"[A-Z]", re.sub(r"\\[A-Za-z]", "", pat)) else re.IGNORECASE
        n = sum(1 for l in lines if re.search(pat, l, flags))
        if n: print(f"  BLOCKED {label}: household denylist entry #{i} ({n} line(s))"); hits = 1
else:
    print(f"  WARNING: {deny} missing — only generic checks ran")
sys.exit(hits)
PY
  local rc=$?; rm -f "$tmp"; return $rc
}
rc=0
case "${1:-}" in
  staged) git diff --cached -U0 --no-color | grep -E '^[+]' | grep -E -v '^[+]{3}' | scan "staged changes" || rc=1 ;;
  msg)    scan "commit message" < "$2" || rc=1 ;;
  range)  for c in $(git rev-list $2); do
            git log -1 --format=%B "$c" | scan "message of $(git log -1 --format=%h "$c")" || rc=1
            git show "$c" -U0 --no-color --format= | grep -E '^[+]' | grep -E -v '^[+]{3}' | scan "diff of $(git log -1 --format=%h "$c")" || rc=1
          done ;;
  *) echo "usage: $0 staged | msg <file> | range <a>..<b>"; exit 2 ;;
esac
[ $rc -eq 0 ] && echo "  privacy-guard: clean" || echo "  privacy-guard: REFUSED — personal data or a secret would go online. Nothing personal leaves this machine. Ever."
exit $rc
