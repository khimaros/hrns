#!/bin/sh
# example hook for hrns. logs every stage invocation along with its
# stdin payload, then exits cleanly without modifying the request.
#
# install: place under .agents/hooks/ (or .claude/hooks/, .opencode/hooks/)
# and `chmod +x`. verify with `hrns --list-hooks`.
#
# stage is in argv[1]; the host writes a single json object on stdin
# (terminated by EOF). lines emitted with `{"log": "..."}` are routed
# to hrns's stderr and are not merged into the hook result.

stage="$1"
payload=$(cat)

# escape for embedding inside a json string literal.
escaped=$(printf '%s' "$payload" | awk 'BEGIN{ORS=""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); gsub(/\t/,"\\t"); print; print "\\n"}')
printf '{"log": "stage=%s payload=%s"}\n' "$stage" "$escaped"
