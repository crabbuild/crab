import { useMemo, useState } from "react";
import { SearchIcon } from "@primer/octicons-react";
import { Button } from "@primer/react";
import { displayCell, type TableData } from "./file-preview-model";

const PAGE_SIZE = 100;

export function DataTable({ data, name }: { data: TableData; name: string }) {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(0);
  const filtered = useMemo(() => {
    const term = query.trim().toLocaleLowerCase();
    if (!term) return data.rows;
    return data.rows.filter((row) =>
      row.some((value) =>
        displayCell(value).toLocaleLowerCase().includes(term),
      ),
    );
  }, [data.rows, query]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const current = Math.min(page, pages - 1);
  const rows = filtered.slice(current * PAGE_SIZE, (current + 1) * PAGE_SIZE);
  if (!data.columns.length)
    return (
      <div className="file-preview-notice" role="status">
        No tabular records were found in this file.
      </div>
    );
  return (
    <section className="data-explorer" aria-label={`${name} data explorer`}>
      <header className="data-explorer-toolbar">
        <label className="data-search">
          <SearchIcon aria-hidden="true" />
          <span className="sr-only">Search rows</span>
          <input
            type="search"
            value={query}
            placeholder="Search loaded rows"
            onChange={(event) => {
              setQuery(event.target.value);
              setPage(0);
            }}
          />
        </label>
        <span className="muted">
          {filtered.length.toLocaleString()} loaded
          {data.totalRows !== data.rows.length
            ? ` · ${data.totalRows.toLocaleString()} total`
            : ""}
          {" · "}
          {data.columns.length.toLocaleString()} columns
        </span>
      </header>
      <div className="data-grid-scroll" tabIndex={0}>
        <table className="data-grid">
          <caption className="sr-only">{name}</caption>
          <thead>
            <tr>
              <th className="data-row-number" scope="col">
                #
              </th>
              {data.columns.map((column, columnIndex) => (
                <th key={`${column}:${columnIndex}`} scope="col">
                  {column}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, rowIndex) => (
              <tr key={current * PAGE_SIZE + rowIndex}>
                <th className="data-row-number" scope="row">
                  {current * PAGE_SIZE + rowIndex + 1}
                </th>
                {data.columns.map((column, columnIndex) => (
                  <td
                    key={`${column}:${columnIndex}`}
                    title={displayCell(row[columnIndex])}
                  >
                    {displayCell(row[columnIndex])}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <footer className="data-explorer-footer">
        <span className="muted">
          {data.note ?? "All loaded rows are shown."}
        </span>
        {pages > 1 && (
          <div className="data-pagination">
            <Button
              disabled={current === 0}
              onClick={() => setPage(current - 1)}
            >
              Previous
            </Button>
            <span>
              Page {current + 1} of {pages}
            </span>
            <Button
              disabled={current + 1 === pages}
              onClick={() => setPage(current + 1)}
            >
              Next
            </Button>
          </div>
        )}
      </footer>
    </section>
  );
}
