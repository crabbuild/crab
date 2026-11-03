#!/usr/bin/env bash
# Crab Desktop — Bash Shell Integration
#
# Emits OSC 133 (prompt/command marks) and OSC 7 (cwd reporting) sequences
# so the terminal can track prompt boundaries, command execution, and the
# current working directory.
#
# This script is sourced automatically when CRAB_SHELL_INTEGRATION is set.
# It uses PROMPT_COMMAND for prompt-start/cwd and the DEBUG trap for
# command-start/executed detection.

# Guard: only activate when the env var is set
[[ -n "$CRAB_SHELL_INTEGRATION" ]] || return 0

# Avoid double-sourcing
[[ -z "$__crab_shell_integration_active" ]] || return 0
__crab_shell_integration_active=1

# State tracking
__crab_command_started=""
__crab_at_prompt=1

# --- PROMPT_COMMAND handler ---
# Fires before each prompt is drawn.
# Emits:
#   OSC 133 D;<exit_code> — command finished (with exit code from previous command)
#   OSC 133 A             — prompt start
#   OSC 7                 — current working directory
__crab_prompt_command() {
    local exit_code=$?

    # Emit command-finished mark (only if a command actually ran)
    if [[ -n "$__crab_command_started" ]]; then
        printf '\033]133;D;%d\007' "$exit_code"
        __crab_command_started=""
    fi

    # Emit prompt-start mark
    printf '\033]133;A\007'

    # Emit cwd via OSC 7
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-$(hostname)}" "${PWD}"

    # Mark that we're at a prompt (DEBUG trap uses this)
    __crab_at_prompt=1
}

# --- DEBUG trap handler ---
# Fires before each command executes. We use it to detect the transition
# from prompt to command execution.
#
# The DEBUG trap fires for every simple command in a pipeline and for
# PROMPT_COMMAND itself, so we guard against re-entry using __crab_at_prompt.
__crab_debug_trap() {
    # Skip if we're inside PROMPT_COMMAND or already started a command
    if [[ "$__crab_at_prompt" != "1" ]]; then
        return
    fi

    # Skip if this is PROMPT_COMMAND itself executing
    if [[ "$BASH_COMMAND" == "__crab_prompt_command" ]]; then
        return
    fi

    # We're transitioning from prompt to command execution
    __crab_at_prompt=0
    __crab_command_started=1

    # Emit command-start mark
    printf '\033]133;B\007'

    # Emit command-executed mark
    printf '\033]133;C\007'
}

# --- Hook registration ---
# Chain onto existing PROMPT_COMMAND without overwriting
if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="__crab_prompt_command"
elif [[ "$PROMPT_COMMAND" != *"__crab_prompt_command"* ]]; then
    # Append to existing PROMPT_COMMAND (handle both array and string forms)
    PROMPT_COMMAND="__crab_prompt_command;${PROMPT_COMMAND}"
fi

# Install the DEBUG trap, preserving any existing trap
__crab_existing_debug_trap=$(trap -p DEBUG | sed "s/trap -- '\\(.*\\)' DEBUG/\\1/")
if [[ -n "$__crab_existing_debug_trap" ]]; then
    eval "trap '__crab_debug_trap; ${__crab_existing_debug_trap}' DEBUG"
else
    trap '__crab_debug_trap' DEBUG
fi
