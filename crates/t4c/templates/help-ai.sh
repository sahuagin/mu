#!/bin/sh
# Conforming `--help-ai --json` for a shell tool. Drop t4c_help_ai() into your
# tool and dispatch to it early in arg parsing. See docs/help-ai-standard.md.
#
#   case "$*" in *--help-ai*) t4c_help_ai; exit 0;; esac

t4c_help_ai() {
  cat <<'JSON'
{
  "name": "mytool",
  "summary": "one line describing what mytool is for",
  "keywords": ["example", "shell"],
  "subcommands": [
    { "name": "run", "summary": "do the main thing" },
    { "name": "status", "summary": "show current state" }
  ]
}
JSON
}
