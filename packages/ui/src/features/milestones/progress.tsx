import type { Milestone } from "./model";

export function MilestoneProgress({ milestone }: { milestone: Milestone }) {
  const total = milestone.open_count + milestone.closed_count;
  const percent = total
    ? Math.round((milestone.closed_count / total) * 100)
    : 0;
  return (
    <div className="milestone-progress">
      <div
        className="milestone-progress-track"
        role="progressbar"
        aria-label={`${milestone.title} progress`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
        aria-valuetext={`${milestone.closed_count} of ${total} completed`}
      >
        <span style={{ width: `${percent}%` }} />
      </div>
      <p className="muted">
        <strong>{percent}%</strong> complete
        <span>
          {milestone.open_count} open · {milestone.closed_count} closed
        </span>
      </p>
    </div>
  );
}

export function MilestoneDate({ milestone }: { milestone: Milestone }) {
  if (!milestone.due_on) return <span className="muted">No due date</span>;
  const now = new Date();
  const today = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
  const overdue = milestone.state === "open" && milestone.due_on < today;
  const value = new Date(`${milestone.due_on}T00:00:00Z`).toLocaleDateString(
    undefined,
    {
      year: "numeric",
      month: "short",
      day: "numeric",
      timeZone: "UTC",
    },
  );
  return (
    <span className={overdue ? "milestone-overdue" : "muted"}>
      {overdue ? "Overdue since " : "Due "}
      <time dateTime={milestone.due_on}>{value}</time>
    </span>
  );
}
