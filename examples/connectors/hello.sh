#!/bin/sh
# A complete benchy exec connector, in shell, with no dependencies.
#
# The point of this file is the *floor*: if the smallest possible connector is this small, the
# contract is not asking much of you. A real one fetches instead of `cat`-ing, and almost
# certainly uses an Arrow library — see meters.py next door.
#
#   argv[1]  is the subcommand, before any operator-supplied arguments
#   stdin    is one JSON document (this one ignores it — it has nothing to narrow)
#   stdout   is one Arrow streaming IPC stream, and nothing else
#   stderr   is NDJSON control events, and anything else is treated as log noise
#
# Register it with:
#
#   [[source]]
#   name = "readings"
#   type = "exec"
#   command = "./connectors/hello.sh"
#     [[source.schema]]
#     name = "id"
#     type = "int64"
#     nullable = false
#   ...

here=$(dirname "$0")

case "$1" in
  describe)
    # Pure: no network, no credentials, no side effects. 5s cap.
    cat <<'JSON'
{ "protocol": [1],
  "name": "hello",
  "version": "0.1.0",
  "narrowing": { "time": "unsupported", "entity": "unsupported",
                 "columns": "unsupported", "params": "unsupported" } }
JSON
    ;;

  fetch)
    echo '{"type":"log","level":"info","message":"serving the sample stream"}' >&2
    cat "$here/hello.arrows" || exit 1
    echo '{"type":"end","rows":4}' >&2
    ;;

  *)
    # `estimate` lands here too: not implementing it is allowed, and the fetch goes ahead.
    echo "usage: $0 describe|fetch" >&2
    exit 2
    ;;
esac
