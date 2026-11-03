/**
 * Shell-ready barrier and marker scanning for the PTY daemon.
 *
 * The shell-ready marker (`\x1b]777;crab-shell-ready\x07`) is emitted by
 * shell rcfile wrappers after initialization completes. The daemon queues
 * stdin writes until the marker fires (or a timeout expires), then uses
 * the PostReadyFlushGate to delay the actual flush until the shell has
 * drawn its prompt and switched the PTY into raw mode.
 *
 * Why the gate exists: the marker fires from precmd_functions (zsh) or
 * PROMPT_COMMAND (bash) — before the shell draws its prompt and before
 * zle/readline flips the PTY into raw mode. Flushing immediately causes
 * the kernel (ECHO still on) to echo the command once, and the line
 * editor redraws it — producing a visible duplicate.
 *
 * Strategy: after the marker, wait for the next PTY data chunk (the
 * prompt draw) plus a 30ms delay for tcsetattr() to enable raw mode.
 * A 50ms wall-clock fallback covers the case where the prompt arrives
 * in the same chunk as the marker.
 */

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** The shell-ready OSC marker emitted by our rcfile wrappers. */
export const SHELL_READY_MARKER = "\x1b]777;crab-shell-ready\x07";

/** Delay after prompt data arrives, waiting for tcsetattr to enable raw mode. */
const POST_DATA_DELAY_MS = 30;

/** Wall-clock fallback if no PTY data arrives after the marker. */
const FALLBACK_TIMEOUT_MS = 50;

// ---------------------------------------------------------------------------
// ShellReadyScanner — partial-match buffering for the marker
// ---------------------------------------------------------------------------

/** Mutable state for the character-by-character marker scanner. */
export interface ShellReadyScanState {
  matchPos: number;
  heldChars: string;
}

/** Result of scanning a chunk of PTY output. */
export interface ShellReadyScanResult {
  /** Output with the marker stripped (if matched). */
  output: string;
  /** Whether the full marker was detected in this chunk. */
  matched: boolean;
}

/**
 * Scanner that detects the shell-ready marker in PTY output, handling
 * partial matches that span multiple data chunks.
 */
export class ShellReadyScanner {
  private state: ShellReadyScanState = { matchPos: 0, heldChars: "" };

  /** Reset scanner state (e.g., on session teardown). */
  reset(): void {
    this.state.matchPos = 0;
    this.state.heldChars = "";
  }

  /**
   * Scan a chunk of PTY output for the shell-ready marker.
   *
   * Matching characters are held back from output. On full match (the
   * complete marker sequence), they're discarded and `matched` is true.
   * On mismatch, held characters are flushed as regular terminal output.
   *
   * Handles the marker spanning multiple data chunks via partial-match
   * buffering.
   */
  scan(data: string): ShellReadyScanResult {
    const marker = SHELL_READY_MARKER;
    let output = "";

    for (let i = 0; i < data.length; i++) {
      const ch = data[i]!;

      if (this.state.matchPos < marker.length) {
        if (ch === marker[this.state.matchPos]) {
          this.state.heldChars += ch;
          this.state.matchPos++;

          // Check if we just completed the full marker
          if (this.state.matchPos === marker.length) {
            // Full marker matched — discard held chars, return remaining
            const remaining = data.slice(i + 1);
            this.state.heldChars = "";
            this.state.matchPos = 0;
            return { output: output + remaining, matched: true };
          }
        } else {
          // Mismatch — flush held chars as regular output
          output += this.state.heldChars;
          this.state.heldChars = "";
          this.state.matchPos = 0;

          // Check if current char starts a new potential match
          if (ch === marker[0]) {
            this.state.heldChars = ch;
            this.state.matchPos = 1;
          } else {
            output += ch;
          }
        }
      }
    }

    return { output, matched: false };
  }

  /** Get any held characters (partial match in progress). */
  getHeldChars(): string {
    return this.state.heldChars;
  }
}

// ---------------------------------------------------------------------------
// PostReadyFlushGate — delays stdin flush until after prompt draw
// ---------------------------------------------------------------------------

/**
 * Gate that delays flushing queued stdin until the shell has drawn its
 * prompt and (likely) switched the PTY into raw mode.
 *
 * Lifecycle:
 * 1. `arm()` — called when the shell-ready marker is detected
 * 2. `notifyData()` — called on each subsequent PTY data chunk
 * 3. After the next data chunk + 30ms (or 50ms fallback), the flush
 *    callback fires
 * 4. `clear()` — called on session teardown to cancel pending timers
 */
export class PostReadyFlushGate {
  private armed = false;
  private flushed = false;
  private dataTimer: ReturnType<typeof setTimeout> | null = null;
  private fallbackTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly onFlush: () => void;

  constructor(onFlush: () => void) {
    this.onFlush = onFlush;
  }

  /** Whether the gate is armed and waiting for the flush condition. */
  get isPending(): boolean {
    return this.armed && !this.flushed;
  }

  /**
   * Arm the gate. Called when the shell-ready marker is detected.
   * Starts the 50ms wall-clock fallback timer.
   */
  arm(): void {
    if (this.armed) return;
    this.armed = true;

    // Fallback: if no PTY data arrives within 50ms (prompt was in the
    // same chunk as the marker), flush anyway.
    this.fallbackTimer = setTimeout(() => {
      this.fallbackTimer = null;
      this.flush();
    }, FALLBACK_TIMEOUT_MS);
  }

  /**
   * Notify the gate that a PTY data chunk arrived. If armed and not yet
   * flushed, this starts the 30ms post-data delay (cancelling the
   * fallback timer since we now have a more precise signal).
   */
  notifyData(): void {
    if (!this.armed || this.flushed) return;

    // Cancel fallback — we have a data-driven signal now
    if (this.fallbackTimer !== null) {
      clearTimeout(this.fallbackTimer);
      this.fallbackTimer = null;
    }

    // Cancel any previous data timer (in case of rapid chunks)
    if (this.dataTimer !== null) {
      clearTimeout(this.dataTimer);
    }

    // Wait 30ms after this data chunk for tcsetattr to enable raw mode
    this.dataTimer = setTimeout(() => {
      this.dataTimer = null;
      this.flush();
    }, POST_DATA_DELAY_MS);
  }

  /**
   * Clear the gate, cancelling all pending timers. Called on session
   * teardown or when the gate is no longer needed.
   */
  clear(): void {
    if (this.dataTimer !== null) {
      clearTimeout(this.dataTimer);
      this.dataTimer = null;
    }
    if (this.fallbackTimer !== null) {
      clearTimeout(this.fallbackTimer);
      this.fallbackTimer = null;
    }
    this.armed = false;
    this.flushed = false;
  }

  private flush(): void {
    if (this.flushed) return;
    this.flushed = true;

    // Clean up any remaining timer
    if (this.dataTimer !== null) {
      clearTimeout(this.dataTimer);
      this.dataTimer = null;
    }
    if (this.fallbackTimer !== null) {
      clearTimeout(this.fallbackTimer);
      this.fallbackTimer = null;
    }

    this.onFlush();
  }
}

// ---------------------------------------------------------------------------
// Shell rcfile content — wrappers that emit the shell-ready marker
// ---------------------------------------------------------------------------

const MARKER_ESCAPED = "\\033]777;crab-shell-ready\\007";

/**
 * Returns the content for a bash rcfile wrapper that sources the user's
 * login files and emits the shell-ready marker via PROMPT_COMMAND.
 */
export function getBashRcfileContent(): string {
  return `# Crab bash shell-ready wrapper
[[ -f /etc/profile ]] && source /etc/profile
if [[ -f "$HOME/.bash_profile" ]]; then
  source "$HOME/.bash_profile"
elif [[ -f "$HOME/.bash_login" ]]; then
  source "$HOME/.bash_login"
elif [[ -f "$HOME/.profile" ]]; then
  source "$HOME/.profile"
fi
# Emit shell-ready marker via PROMPT_COMMAND so it fires after login
# startup files have rebuilt the prompt.
__crab_prompt_mark() {
  printf "${MARKER_ESCAPED}"
}
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
  PROMPT_COMMAND=("\${PROMPT_COMMAND[@]}" "__crab_prompt_mark")
else
  _crab_prev_prompt_command="\${PROMPT_COMMAND}"
  if [[ -n "\${_crab_prev_prompt_command}" ]]; then
    PROMPT_COMMAND="\${_crab_prev_prompt_command};__crab_prompt_mark"
  else
    PROMPT_COMMAND="__crab_prompt_mark"
  fi
fi
`;
}

/**
 * Returns the content for a zsh rcfile wrapper that sources the user's
 * .zshrc and emits the shell-ready marker via precmd_functions.
 */
export function getZshRcfileContent(): string {
  return `# Crab zsh shell-ready wrapper
_crab_home="\${CRAB_ORIG_ZDOTDIR:-$HOME}"
if [[ "$_crab_home" != "$ZDOTDIR" && -o interactive && -f "$_crab_home/.zshrc" ]]; then
  source "$_crab_home/.zshrc"
fi
# Emit shell-ready marker via precmd_functions so it fires before the
# prompt is drawn. Keep our hook LAST so it fires after direnv and other
# precmd hooks complete.
__crab_prompt_mark() {
  printf "${MARKER_ESCAPED}"
}
precmd_functions=(\${precmd_functions[@]} __crab_prompt_mark)
`;
}

/** Set of shell names that support the shell-ready marker via our wrappers. */
export const SHELLS_WITH_READY_MARKER = new Set(["zsh", "bash"]);
