import { describe, it, expect } from "vitest";
import {
  restore,
  initialQuery,
  initialSpec,
} from "../../workflow-results/model";

describe("Results comparison preferences", () => {
  const selection = {
    id: "01900000-0000-7000-8000-000000000001",
    digest: "a".repeat(64),
  };
  it("rejects malformed cached selections without crashing the workspace", () => {
    expect(
      restore({ query: initialQuery, spec: initialSpec, selected: [null] })
        .selected,
    ).toEqual([]);
  });
  it("restores pinned selections and an explicit empty column selection without native identities or payloads", () => {
    const cached = {
      query: { ...initialQuery, columns: [], native_session_id: "discard" },
      selected: [{ ...selection, metrics: { score: 1 } }],
      spec: { ...initialSpec, columns: [], baseline: selection.id },
      report: "b".repeat(64),
    };
    expect(restore(cached)).toEqual({
      query: { ...initialQuery, columns: [] },
      selected: [selection],
      spec: { ...initialSpec, columns: [], baseline: selection.id },
      report: "b".repeat(64),
    });
  });
  it("restores saved axis bounds and rejects invalid ranges", () => {
    const key = "c".repeat(64);
    const cached = {
      query: initialQuery,
      spec: {
        ...initialSpec,
        axes: { [key]: { lower: 0, upper: 0.5, discard: true } },
      },
      selected: [selection],
    };
    expect(restore(cached).spec.axes).toEqual({
      [key]: { lower: 0, upper: 0.5 },
    });
    cached.spec.axes[key].upper = Infinity;
    expect(restore(cached).selected).toEqual([]);
  });
});
