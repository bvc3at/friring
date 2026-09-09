#!/usr/bin/env bash
#
# Make unprivileged user namespaces available on a **disposable CI runner**, or
# fail saying why not — `scripts/ci/allow-user-namespaces.sh`.
#
# bubblewrap needs `unshare(CLONE_NEWUSER)`. Ubuntu 24.04 ships AppArmor's
# restriction on that enabled, so a hosted runner has the bwrap binary and
# refuses it a namespace — which is not a friring failure and not something
# friring may work around: relaxing the *product's* boundary to fit a runner
# would be inverting the whole point. What is legitimate is granting the
# capability the kernel is withholding, on a machine that is re-imaged for every
# job and holds nothing.
#
# Order matters, narrowest first:
#
#   1. Do nothing if a namespace can already be created. The common case on a
#      self-hosted or older-image runner, and nothing below should run then.
#   2. Enable the AppArmor profile Ubuntu ships **for bwrap specifically**
#      (`/etc/apparmor.d/bwrap`), which grants `userns create` to that one
#      program and leaves every other unconfined binary restricted.
#   3. Only if that is absent or insufficient, clear the global restriction
#      (`kernel.apparmor_restrict_unprivileged_userns`). This is broader — it
#      applies to every unconfined program on the machine — and is a
#      disposable-runner-only measure, recorded as such rather than described
#      as narrow.
#
# Every step prints what it found and what it did, and the capability is
# re-checked after each, so a log says which one was needed rather than which
# were attempted.
#
# **Refuses anything but a GitHub-hosted runner.** It changes kernel settings,
# and the only machine that justifies is one re-imaged for every job. `CI` and
# `GITHUB_ACTIONS` do not establish that — both are equally true on a
# *self-hosted* runner, which is a real machine somebody owns and which this
# must never touch. The workflow passes `${{ runner.environment }}` in, and
# anything other than `github-hosted` — including absent — is refused before
# any `sudo`.
set -uo pipefail

if [ "${RUNNER_ENVIRONMENT:-}" != "github-hosted" ]; then
    echo "allow-user-namespaces: this changes kernel settings and runs only on" \
        "a GitHub-hosted runner, which is re-imaged for every job." >&2
    echo "  runner environment: ${RUNNER_ENVIRONMENT:-(not reported)}" >&2
    exit 2
fi

# stdout to /dev/null inside the group, then the group's stderr is what the
# caller captures: the failure's own words are the whole point.
probe() { { bwrap --ro-bind / / --unshare-all true >/dev/null; } 2>&1; }

report() {
    echo "kernel: $(uname -r)"
    for knob in kernel/apparmor_restrict_unprivileged_userns \
        kernel/unprivileged_userns_clone \
        user/max_user_namespaces; do
        if [ -r "/proc/sys/$knob" ]; then
            echo "  $knob = $(cat "/proc/sys/$knob")"
        else
            echo "  $knob = (absent)"
        fi
    done
}

report
if err=$(probe); then
    echo "bwrap: a namespace is already available; changing nothing"
    exit 0
fi
echo "bwrap: refused, and the reason it gave is:"
printf '  %s\n' "$err" | head -5

# (2) The bwrap-specific profile, which is the narrow grant.
if [ -e /etc/apparmor.d/bwrap ]; then
    echo "apparmor: loading the bwrap profile Ubuntu ships"
    sudo apparmor_parser -r /etc/apparmor.d/bwrap || true
    if err=$(probe); then
        echo "bwrap: a namespace is available with the bwrap profile alone"
        exit 0
    fi
    echo "bwrap: still refused after the bwrap profile:"
    printf '  %s\n' "$err" | head -5
else
    echo "apparmor: no /etc/apparmor.d/bwrap on this image"
fi

# (3) The global restriction. Broader than the above, and only here.
knob=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
if [ -e "$knob" ] && [ "$(cat "$knob")" != 0 ]; then
    echo "sysctl: clearing the global unprivileged-userns restriction" \
        "(disposable runner only, and broader than the bwrap profile above)"
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi

if err=$(probe); then
    echo "bwrap: a namespace is available"
    exit 0
fi
echo "this runner cannot create an unprivileged user namespace, so the" \
    "assertions this job exists for cannot run:" >&2
printf '  %s\n' "$err" | head -5 >&2
exit 1
