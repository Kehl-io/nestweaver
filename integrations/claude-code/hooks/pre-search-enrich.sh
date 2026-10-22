#!/usr/bin/env bash
set -euo pipefail

# Every hook here is advisory: without jq there is nothing to parse, so stay
# silent rather than fail.
command -v jq >/dev/null 2>&1 || exit 0

INPUT=$(cat)
# Claude Code sends the tool's arguments under `tool_input`, not at the top
# level of the event JSON.
EVENT=$(printf '%s' "$INPUT" | jq -r '.hook_event_name // "PreToolUse"')

# Context for Claude must be JSON on stdout under
# hookSpecificOutput.additionalContext. stderr from a hook that exits 0 is never
# shown to Claude, and a top-level `additionalContext` key is ignored.
emit_context() {
  jq -n --arg event "$EVENT" --arg ctx "$1" \
    '{hookSpecificOutput: {hookEventName: $event, additionalContext: $ctx}}'
}

COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty')
[ -n "$COMMAND" ] || exit 0

# Hooks run with the session's working directory; anchor the default database
# at the project root Claude Code reports, not wherever the shell happens to be.
DB="${NESTWEAVER_DB:-${CLAUDE_PROJECT_DIR:-.}/nestweaver.lbug}"
[ -e "$DB" ] || exit 0

# The search pattern of the first grep/rg that is the INVOKED command of a
# pipeline segment — not a word inside `echo grep foo` or a commit message, and
# not the last token (that is usually the path: `grep -rn "foo bar" src`).
# Quotes and escapes are honoured; -e/--regexp, `--`, redirections and the
# options that take a value are understood. When the pattern is not on the
# command line (-f/--file, rg --files) nothing is searched: an advisory hook
# says nothing rather than guess.
# `read -d ''` returns 1 at end of input; `|| true` keeps `set -e` quiet.
read -r -d '' EXTRACT_PATTERN <<'JQ' || true
# Shell words: quotes kept, so an operator inside quotes stays part of a word.
def words:
  [scan("[0-9]*[<>]+&?[0-9-]*|[|;&()`\n]+|(?:'[^']*'|\"(?:\\\\.|[^\"\\\\])*\"|\\\\.|[^\\s'\"\\\\|;&()`<>])+")];
def is_op: test("^[|;&()`\n]+$");
def is_redirect: test("^[0-9]*[<>]+&?[0-9-]*$");
# One word as the shell passes it: quotes removed, escapes applied.
def unquote:
  [scan("'([^']*)'|\"((?:\\\\.|[^\"\\\\])*)\"|\\\\(.)|([^'\"\\\\]+)")]
  | map(if .[0] != null then .[0]
        elif .[1] != null then (.[1] | gsub("\\\\(?<c>[\"\\\\$`])"; "\(.c)"))
        elif .[2] != null then .[2]
        else .[3] end)
  | join("");
def segments: reduce .[] as $t ([[]]; if ($t | is_op) then . + [[]] else .[-1] += [$t] end);
def drop_redirects:
  if length == 0 then .
  elif (.[0] | is_redirect) then
    (if (.[0] | test("&")) then .[1:] else .[2:] end) | drop_redirects
  else [.[0]] + (.[1:] | drop_redirects) end;
def strip_prefix:
  if length == 0 then .
  elif (.[0] | test("^[A-Za-z_][A-Za-z0-9_]*=")) then .[1:] | strip_prefix
  elif (.[0] | test("^(sudo|command|exec|time|nice|env|xargs|if|then|else|elif|do|while|until|!)$")) then .[1:] | strip_prefix
  else . end;
def tool_name: unquote | sub("^.*/"; "");
# The first option letter in a short cluster that decides anything.
def short_cluster($vshort):
  . as $c
  | first(
      (range(0; $c | length) as $k
       | $c[$k:$k + 1] as $ch
       | $c[$k + 1:] as $rest
       | if $ch == "e" then {e: $rest}
         elif $ch == "f" then {unclear: true}
         elif ($vshort | contains($ch)) then {skip: ($rest == "")}
         else empty end),
      {});
def pattern($tool):
  . as $args
  | (if $tool == "rg" then "ABCEMTdgjmrt" else "ABCDdm" end) as $vshort
  | (if $tool == "rg" then
       ["after-context", "before-context", "context", "max-count", "glob", "iglob",
        "type", "type-not", "type-add", "type-clear", "max-depth", "maxdepth",
        "max-filesize", "threads", "encoding", "replace", "sort", "sortr", "colors",
        "color", "context-separator", "field-context-separator",
        "field-match-separator", "max-columns", "path-separator", "pre", "pre-glob",
        "engine", "dfa-size-limit", "regex-size-limit", "ignore-file",
        "hostname-bin", "hyperlink-format", "generate"]
     else
       ["after-context", "before-context", "context", "max-count", "include",
        "exclude", "exclude-dir", "exclude-from", "label", "binary-files",
        "devices", "directories", "group-separator"]
     end) as $vlong
  | reduce range(0; length) as $i (
      {skip: false, ends: false, e: null, pos: null, unclear: false};
      if .skip then .skip = false
      else $args[$i] as $w
      | if .ends then (if .pos == null then .pos = $w else . end)
        elif $w == "--" then .ends = true
        elif $w == "-e" or $w == "--regexp" then
          (if .e == null then .e = $args[$i + 1] else . end) | .skip = true
        elif ($w | startswith("--regexp=")) then
          (if .e == null then .e = $w[9:] else . end)
        elif $w == "--file" or ($w | startswith("--file="))
             or $w == "--files" or $w == "--type-list"
             or $w == "--help" or $w == "--version" then .unclear = true
        elif ($w | startswith("--")) then
          (if ($w | contains("=")) then .
           elif any($vlong[]; . == $w[2:]) then .skip = true
           else . end)
        elif ($w | test("^-.")) then
          ($w[1:] | short_cluster($vshort)) as $r
          | if $r.unclear then .unclear = true
            elif ($r | has("e")) then
              (if $r.e == "" then
                 (if .e == null then .e = $args[$i + 1] else . end) | .skip = true
               else
                 (if .e == null then .e = $r.e else . end)
               end)
            elif $r.skip then .skip = true
            else . end
        else (if .pos == null then .pos = $w else . end)
        end
      end)
  | if .unclear then empty elif .e != null then .e else .pos end
  | select(. != null and . != "");

(.tool_input.command // "")
| words
| segments
| map(drop_redirects | strip_prefix | select(length > 0))
| map(select(.[0] | tool_name | test("^(grep|egrep|fgrep|rg)$")))
| .[0] // empty
| (if (.[0] | tool_name) == "rg" then "rg" else "grep" end) as $tool
| .[1:]
| map(unquote)
| pattern($tool)
JQ
SEARCH_TERM=$(printf '%s' "$INPUT" | jq -r "$EXTRACT_PATTERN" 2>/dev/null || true)
if [ -n "$SEARCH_TERM" ]; then
  RESULTS=$(nestweaver search --json --db "$DB" -- "$SEARCH_TERM" 2>/dev/null || true)
  # `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
  # array before it. Normalise so this hook works against either binary.
  ROWS=$(printf '%s' "$RESULTS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
  if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
    TEXT="NestWeaver graph results for \"$SEARCH_TERM\" (brain_search / regex_search cover indexed repos):
$(printf '%s' "$ROWS" | jq -r '.[0:3] | .[] | "  \(.name) @ \(.file_path):\(.start_line)"')"
    emit_context "$TEXT"
  fi
fi
