#!/bin/sh
# example hook for hrns. registers a `datetime_now` tool that returns
# the current date and time.
#
# install: place under .agents/hooks/ (or .claude/hooks/, .opencode/hooks/)
# and `chmod +x`. verify with `hrns --list-hooks`.

stage="$1"
payload=$(cat)

case "$stage" in
    discover)
        # register one custom tool: datetime_now.
        cat <<'EOF'
{"name": "datetime", "tools": [{"name": "now", "description": "return the current date and time", "parameters": {"tz": {"type": "string", "description": "optional IANA time zone (e.g. America/New_York, UTC). defaults to system local time.", "optional": true}}}]}
EOF
        ;;
    execute_tool)
        # convention: tool results are json-encoded objects (matches the
        # built-in `read` and `bash` tools). hrns also accepts plain
        # strings here, but structured output is easier for the model.
        tz=$(printf '%s' "$payload" | sed -n 's/.*"tz"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
        if [ -n "$tz" ]; then
            now=$(TZ="$tz" date -Iseconds)
        else
            now=$(date -Iseconds)
        fi
        printf '{"result": "{\\"datetime\\":\\"%s\\"}"}\n' "$now"
        ;;
esac
