#!/bin/sh
# Shared MOTD is cached across accounts; personal summaries belong in user shells.
if [ -x /usr/local/bin/sgpu ] && command -v timeout >/dev/null 2>&1; then
    sgpu_login_color=always
    if [ -n "${NO_COLOR+x}" ] || [ "${TERM:-}" = dumb ]; then
        sgpu_login_color=never
    fi
    if timeout -k 1 1 /usr/local/bin/sgpu welcome --help >/dev/null 2>&1; then
        timeout -k 1 2 /usr/local/bin/sgpu welcome --scope cluster --color "$sgpu_login_color" || :
    elif timeout -s KILL 1 /usr/local/bin/sgpu welcome --help >/dev/null 2>&1; then
        timeout -s KILL 2 /usr/local/bin/sgpu welcome --scope cluster --color "$sgpu_login_color" || :
    fi
fi
