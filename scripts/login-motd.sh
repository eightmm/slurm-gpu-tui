#!/bin/sh
# Shared MOTD is cached across accounts; personal summaries belong in user shells.
if [ -x /usr/local/bin/sgpu ]; then
    sgpu_login_color=always
    if [ -n "${NO_COLOR+x}" ] || [ "${TERM:-}" = dumb ]; then
        sgpu_login_color=never
    fi
    timeout --kill-after=1 2 /usr/local/bin/sgpu welcome --scope cluster --color "$sgpu_login_color" || :
fi
