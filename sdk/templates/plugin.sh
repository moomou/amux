#!/bin/sh
# amux plugin: {{NAME}} (raw protocol, no SDK). Saving this file reloads it.
# Try it: amux {{NAME}}      Guide: amux plugin guide
echo '{"id":0,"method":"register","params":{"commands":[{"name":"{{NAME}}","help":"Example command (shell plugin)"}]}}'
while IFS= read -r line; do
  case $line in
  *'"method":"command"'*)
    id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\).*/\1/p')
    echo "{\"id\":$id,\"result\":\"hello from the {{NAME}} shell plugin\"}"
    ;;
  esac
done
