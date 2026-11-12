type DialogResult = string | boolean | null;
export interface ApplicationDialog {
  id: number;
  kind: "prompt" | "confirm";
  message: string;
  initialValue: string;
  resolve: (result: DialogResult) => void;
}

const queue: ApplicationDialog[] = [];
let nextId = 0;
const listeners = new Set<() => void>();
const notify = () => listeners.forEach((listener) => listener());

export const applicationDialogs = {
  current: () => queue[0] ?? null,
  subscribe: (listener: () => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  },
  complete: (request: ApplicationDialog, result: DialogResult) => {
    // Radix can report close after an action resolves. That stale close must
    // never cancel the next queued prompt or confirmation.
    if (queue[0] !== request) return;
    queue.shift()?.resolve(result);
    notify();
  },
  cancel: () => {
    queue
      .splice(0)
      .forEach((request) =>
        request.resolve(request.kind === "confirm" ? false : null),
      );
    notify();
  },
};

export function requestPrompt(
  message: string,
  initialValue = "",
): Promise<string | null> {
  return new Promise((resolve) => {
    queue.push({
      id: ++nextId,
      kind: "prompt",
      message,
      initialValue,
      resolve: (result) => resolve(typeof result === "string" ? result : null),
    });
    notify();
  });
}

export function requestConfirmation(message: string): Promise<boolean> {
  return new Promise((resolve) => {
    queue.push({
      id: ++nextId,
      kind: "confirm",
      message,
      initialValue: "",
      resolve: (result) => resolve(result === true),
    });
    notify();
  });
}
