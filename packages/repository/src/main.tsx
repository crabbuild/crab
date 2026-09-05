import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@primer/primitives/dist/css/primitives.css";
import "@primer/primitives/dist/css/functional/themes/light.css";
import "@primer/primitives/dist/css/functional/themes/dark.css";
import { App } from "./App";
import { AppErrorBoundary } from "./ui";
import "./style.css";

const root = document.getElementById("root");
if (root)
  createRoot(root).render(
    <StrictMode>
      <AppErrorBoundary>
        <App />
      </AppErrorBoundary>
    </StrictMode>,
  );
