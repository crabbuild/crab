#!/usr/bin/env fish
# Crab Desktop — Fish Shell Integration
#
# Emits OSC 133 (prompt/command marks) and OSC 7 (cwd reporting) sequences
# so the terminal can track prompt boundaries, command execution, and the
# current working directory.
#
# This script is sourced automatically when CRAB_SHELL_INTEGRATION is set.
# It uses fish event functions (fish_prompt, fish_preexec, fish_postexec)
# which chain naturally without overwriting existing handlers.

# Guard: only activate when the env var is set
if not set -q CRAB_SHELL_INTEGRATION
    exit 0
end

# Avoid double-sourcing
if set -q __crab_shell_integration_active
    exit 0
end
set -g __crab_shell_integration_active 1

# --- fish_prompt handler ---
# Fires before each prompt is drawn.
# Emits:
#   OSC 133 A — prompt start
#   OSC 7     — current working directory
function __crab_fish_prompt --on-event fish_prompt
    # Emit prompt-start mark
    printf '\033]133;A\007'

    # Emit cwd via OSC 7
    printf '\033]7;file://%s%s\007' (hostname) $PWD
end

# --- fish_preexec handler ---
# Fires after the user presses Enter but before the command executes.
# Emits:
#   OSC 133 B — command start (end of prompt, user input accepted)
#   OSC 133 C — command executed (shell is now running the command)
function __crab_fish_preexec --on-event fish_preexec
    # Emit command-start mark
    printf '\033]133;B\007'

    # Emit command-executed mark
    printf '\033]133;C\007'
end

# --- fish_postexec handler ---
# Fires after a command finishes executing.
# Emits:
#   OSC 133 D;<exit_code> — command finished with exit status
function __crab_fish_postexec --on-event fish_postexec
    # Emit command-finished mark with the exit status of the last command
    printf '\033]133;D;%d\007' $status
end
