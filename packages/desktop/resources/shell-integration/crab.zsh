#!/usr/bin/env zsh
# Crab Desktop — Zsh Shell Integration
#
# Emits OSC 133 (prompt/command marks) and OSC 7 (cwd reporting) sequences
# so the terminal can track prompt boundaries, command execution, and the
# current working directory.
#
# This script is sourced automatically when CRAB_SHELL_INTEGRATION is set.
# It chains onto existing precmd/preexec hooks without overwriting them.

# Guard: only activate when the env var is set
[[ -n "$CRAB_SHELL_INTEGRATION" ]] || return 0

# Avoid double-sourcing
[[ -z "$__crab_shell_integration_active" ]] || return 0
__crab_shell_integration_active=1

# Track the last command's exit code
__crab_last_exit_code=0

# --- precmd hook ---
# Fires after a command finishes and before the next prompt is drawn.
# Emits:
#   OSC 133 D;<exit_code> — command finished (with exit code from previous command)
#   OSC 133 A             — prompt start
#   OSC 7                 — current working directory
__crab_precmd() {
    local exit_code=$?
    __crab_last_exit_code=$exit_code

    # Emit command-finished mark (skip on first prompt where no command ran)
    if [[ -n "$__crab_command_started" ]]; then
        printf '\033]133;D;%d\007' "$exit_code"
        unset __crab_command_started
    fi

    # Emit prompt-start mark
    printf '\033]133;A\007'

    # Emit cwd via OSC 7
    printf '\033]7;file://%s%s\007' "${HOST:-$(hostname)}" "${PWD}"
}

# --- preexec hook ---
# Fires after the user presses Enter but before the command executes.
# Emits:
#   OSC 133 B — command start (end of prompt, user input accepted)
#   OSC 133 C — command executed (shell is now running the command)
__crab_preexec() {
    __crab_command_started=1

    # Emit command-start mark
    printf '\033]133;B\007'

    # Emit command-executed mark
    printf '\033]133;C\007'
}

# --- Hook registration ---
# Use zsh's hook arrays to chain without overwriting existing hooks.
autoload -Uz add-zsh-hook
add-zsh-hook precmd __crab_precmd
add-zsh-hook preexec __crab_preexec
