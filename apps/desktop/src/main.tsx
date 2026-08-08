import React from "react";
import ReactDOM from "react-dom/client";
import { HashRouter } from "react-router-dom";
import App from "./App";
import "./styles.css";

// Tauri 用 HashRouter（而非 BrowserRouter）：webview 不走标准 http history，
// 用 hash 路由避免 reload / 深链接路径解析问题。
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <HashRouter>
      <App />
    </HashRouter>
  </React.StrictMode>,
);
