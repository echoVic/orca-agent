import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import Docs from "./Docs";
import "../styles.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Docs />
  </StrictMode>,
);
