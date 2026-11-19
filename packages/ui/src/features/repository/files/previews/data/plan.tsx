import { displayCell, type TableData } from "../model";
import { DataTable } from "./explorer";

export function QueryPlan({
  data,
  engine,
}: {
  data: TableData;
  engine: "DuckDB" | "SQLite";
}) {
  return (
    <section className="data-query-plan" aria-label="Query plan" tabIndex={0}>
      {engine === "SQLite" ? (
        <DataTable data={data} name="Query plan steps" />
      ) : (
        data.rows.map(([name, text], index) => (
          <section key={index}>
            <h3>{displayCell(name).replaceAll("_", " ")}</h3>
            <pre>{displayCell(text)}</pre>
          </section>
        ))
      )}
    </section>
  );
}
