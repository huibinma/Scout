import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** 后端 `McpServiceStatus`（apps/desktop/src-tauri/src/mcp_service.rs，serde snake_case）。 */
interface McpServiceStatus {
  running: boolean;
  enabled: boolean;
  address: string;
  url: string;
  token: string | null;
  doc_count: number | null;
  semantic: boolean | null;
}

/** 接入客户端类型（决定「接入配置」区块给出的形态）。 */
type McpClient = "claude" | "codex" | "curl";

/** 生成 Claude Code 的 `mcpServers` 接入片段（照 apps/daemon/README §4）。 */
function configSnippet(url: string, token: string): string {
  return JSON.stringify(
    {
      mcpServers: {
        "scout-local": {
          type: "http",
          url,
          headers: { Authorization: `Bearer ${token}` },
        },
      },
    },
    null,
    2,
  );
}

/** Codex：把当前令牌写进 `SCOUT_MCP_TOKEN` 环境变量（MSIX 桌面版须注销重登才生效）。 */
function codexSetxCmd(token: string): string {
  return `setx SCOUT_MCP_TOKEN "${token}"`;
}

/** Codex：注册本机 MCP server（令牌从环境变量读，不明文落配置文件）。 */
function codexAddCmd(url: string): string {
  return `codex mcp add scout-local --url ${url} --bearer-token-env-var SCOUT_MCP_TOKEN`;
}

/** 通用 curl 探活：rmcp 要求 Accept 同时声明 `application/json` 与 `text/event-stream`，缺一即报错。 */
function curlSnippet(url: string, token: string): string {
  return [
    `curl -X POST ${url} \\`,
    `  -H "Authorization: Bearer ${token}" \\`,
    `  -H "Content-Type: application/json" \\`,
    `  -H "Accept: application/json, text/event-stream" \\`,
    `  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'`,
  ].join("\n");
}

/**
 * 可复制的代码块 + 复制按钮（复用面板既有的深色代码框配色）。
 * `multiline` 为真时用可横向滚动的 `<pre>`（命令 / 片段），否则用单行 `<code>`（令牌）。
 */
function CopyBlock(props: {
  code: string;
  copied: boolean;
  onCopy: () => void;
  multiline?: boolean;
}) {
  const { code, copied, onCopy, multiline = false } = props;
  return (
    <div
      style={{
        display: "flex",
        gap: "8px",
        alignItems: multiline ? "flex-start" : "center",
      }}
    >
      {multiline ? (
        <pre
          style={{
            flex: 1,
            margin: 0,
            padding: "8px 10px",
            borderRadius: "5px",
            backgroundColor: "#1e1e1e",
            color: "#e6e6e6",
            fontSize: "12px",
            overflowX: "auto",
            whiteSpace: "pre",
          }}
        >
          {code}
        </pre>
      ) : (
        <code
          style={{
            flex: 1,
            padding: "6px 10px",
            borderRadius: "5px",
            backgroundColor: "#1e1e1e",
            color: "#e6e6e6",
            fontSize: "12.5px",
            wordBreak: "break-all",
          }}
        >
          {code}
        </code>
      )}
      <button type="button" className="prefs-btn small" onClick={onCopy}>
        {copied ? "已复制" : "复制"}
      </button>
    </div>
  );
}

/**
 * BETA-53：「本机 MCP 服务」面板（跨平台）。
 *
 * 一个开关把桌面已建的本机索引经 MCP 暴露给**本机** LLM 客户端（Claude Code / Codex）——
 * 内嵌复用桌面检索栈、只读挂载桌面 index.db（零重索引、语义白送），只绑 `127.0.0.1` + token。
 * 起停 / 重置令牌 / 状态全走后端命令（start/stop_mcp_service、reset_mcp_token、mcp_service_status），
 * 不经 AppSettings 表单——token 与开关态由后端直接持久化到 settings.json。
 */
export function McpPane() {
  const [status, setStatus] = useState<McpServiceStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 记录「哪个代码块刚复制了」——每个可复制块用一个稳定 key 区分。
  const [copied, setCopied] = useState<string | null>(null);
  // 接入配置区块当前选中的客户端。
  const [client, setClient] = useState<McpClient>("claude");

  const refresh = useCallback(async () => {
    try {
      setStatus(await invoke<McpServiceStatus>("mcp_service_status"));
    } catch (err) {
      console.error("[McpPane] mcp_service_status failed:", err);
    }
  }, []);

  useEffect(() => {
    void refresh();
    // 运行态可能被自动启动 / 其他窗口改变，轻量轮询保持状态新鲜。
    const t = setInterval(() => void refresh(), 3000);
    return () => clearInterval(t);
  }, [refresh]);

  const toggle = async (enable: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const next = await invoke<McpServiceStatus>(
        enable ? "start_mcp_service" : "stop_mcp_service",
      );
      setStatus(next);
    } catch (err) {
      console.error("[McpPane] toggle failed:", err);
      setError(String(err));
      // 失败后拉一次真实状态（可能部分成功）。
      void refresh();
    } finally {
      setBusy(false);
    }
  };

  const resetToken = async () => {
    setBusy(true);
    setError(null);
    try {
      setStatus(await invoke<McpServiceStatus>("reset_mcp_token"));
    } catch (err) {
      console.error("[McpPane] reset_mcp_token failed:", err);
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  const copy = async (text: string, which: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(which);
      setTimeout(() => setCopied(null), 1500);
    } catch (err) {
      console.error("[McpPane] clipboard write failed:", err);
    }
  };

  const running = status?.running ?? false;
  const token = status?.token ?? null;

  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">本机 MCP 服务</label>
        <p className="prefs-hint">
          启用后，本机的 Claude Code / Codex 等 LLM 客户端可通过 MCP 协议
          <strong>用自然语言检索、并读取你电脑里已被 Scout 索引的文件</strong>
          ——复用桌面已建的语义 + 全文索引，无需另起服务、无需手写配置。
          服务只监听本机回环地址（<code>127.0.0.1</code>），不对局域网开放。
        </p>
      </div>

      <div className="prefs-field">
        <label className="prefs-checkbox prefs-checkbox-strong">
          <input
            type="checkbox"
            checked={running}
            disabled={busy}
            onChange={(e) => void toggle(e.target.checked)}
          />
          <strong>{running ? "已启用（正在运行）" : "启用本机 MCP 服务"}</strong>
        </label>
        {status && (
          <p
            className={`prefs-status ${running ? "status-text-ok" : "status-text-neutral"}`}
          >
            {busy
              ? "处理中…"
              : running
                ? `✓ 运行中 · 监听 ${status.address}` +
                  (status.doc_count != null
                    ? ` · 已挂载 ${status.doc_count} 条索引`
                    : "") +
                  (status.semantic === false
                    ? " · 仅全文（未启用语义召回）"
                    : status.semantic === true
                      ? " · 含语义召回"
                      : "")
                : "已停止"}
          </p>
        )}
        {error && (
          <p className="prefs-status status-text-err">
            ⚠ 操作失败：{error}
          </p>
        )}
      </div>

      {token && (
        <>
          <div className="prefs-field">
            <label className="prefs-label">访问令牌（Bearer token）</label>
            <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
              <code
                style={{
                  flex: 1,
                  padding: "6px 10px",
                  borderRadius: "5px",
                  backgroundColor: "#1e1e1e",
                  color: "#e6e6e6",
                  fontSize: "12.5px",
                  wordBreak: "break-all",
                }}
              >
                {token}
              </code>
              <button
                type="button"
                className="prefs-btn small"
                onClick={() => void copy(token, "token")}
              >
                {copied === "token" ? "已复制" : "复制"}
              </button>
            </div>
            <div style={{ marginTop: "8px" }}>
              <button
                type="button"
                className="prefs-btn small"
                onClick={() => void resetToken()}
                disabled={busy}
              >
                重置令牌
              </button>
              <span className="prefs-hint" style={{ marginLeft: "10px" }}>
                重置会作废旧令牌并踢掉已连接的客户端；服务运行中则自动以新令牌重启（无需重新启用）。
              </span>
            </div>
          </div>

          <div className="prefs-field">
            <label className="prefs-label">接入配置</label>
            {/* 客户端切换：不同客户端吃的接入方式不一样（Claude 吃 JSON、Codex 吃 mcp add 命令）。 */}
            <div style={{ display: "flex", gap: "6px", marginBottom: "4px" }}>
              {(
                [
                  ["claude", "Claude Code"],
                  ["codex", "Codex"],
                  ["curl", "通用 / curl"],
                ] as [McpClient, string][]
              ).map(([id, name]) => (
                <button
                  key={id}
                  type="button"
                  className={
                    client === id ? "prefs-btn small primary" : "prefs-btn small"
                  }
                  onClick={() => setClient(id)}
                >
                  {name}
                </button>
              ))}
            </div>

            {client === "claude" && (
              <>
                <p className="prefs-hint">
                  把下面这段加进 Claude Code 的 MCP 配置（
                  <code>~/.claude/settings.json</code>），即可用{" "}
                  <code>search</code> / <code>read_document</code>{" "}
                  工具检索本机文件：
                </p>
                <CopyBlock
                  code={configSnippet(status?.url ?? "", token)}
                  copied={copied === "claude-config"}
                  onCopy={() =>
                    void copy(
                      configSnippet(status?.url ?? "", token),
                      "claude-config",
                    )
                  }
                  multiline
                />
              </>
            )}

            {client === "codex" && (
              <>
                <p className="prefs-hint">
                  Codex 不吃 <code>mcpServers</code> JSON，只认{" "}
                  <code>codex mcp add</code> 命令。依次执行下面两条（第一条写令牌到
                  环境变量，第二条注册本机 server）：
                </p>
                <p className="prefs-hint" style={{ marginTop: "4px" }}>
                  1. 写入令牌环境变量：
                </p>
                <CopyBlock
                  code={codexSetxCmd(token)}
                  copied={copied === "codex-setx"}
                  onCopy={() => void copy(codexSetxCmd(token), "codex-setx")}
                  multiline
                />
                <p className="prefs-hint" style={{ marginTop: "6px" }}>
                  2. 注册 MCP server：
                </p>
                <CopyBlock
                  code={codexAddCmd(status?.url ?? "")}
                  copied={copied === "codex-add"}
                  onCopy={() =>
                    void copy(codexAddCmd(status?.url ?? ""), "codex-add")
                  }
                  multiline
                />
                <p
                  className="prefs-hint status-text-warn"
                  style={{ marginTop: "8px" }}
                >
                  ⚠ Codex 桌面版是 MSIX 应用：<code>setx</code> 设置环境变量后，需
                  <strong>注销并重新登录 Windows</strong>（或重启机器）才会生效
                  ——重启 Codex app 或 explorer 都不够。否则会连上但一直 401。
                </p>
              </>
            )}

            {client === "curl" && (
              <>
                <p className="prefs-hint">
                  用 curl 手动探活 / 调试 <code>/mcp</code>（返回工具清单即通）：
                </p>
                <CopyBlock
                  code={curlSnippet(status?.url ?? "", token)}
                  copied={copied === "curl"}
                  onCopy={() =>
                    void copy(curlSnippet(status?.url ?? "", token), "curl")
                  }
                  multiline
                />
                <p className="prefs-hint" style={{ marginTop: "8px" }}>
                  手动测 <code>/mcp</code> 时 rmcp 要求 <code>Accept</code> 头同时
                  声明 <code>application/json</code> 和{" "}
                  <code>text/event-stream</code>，缺一即报错。
                </p>
                <p className="prefs-hint status-text-warn">
                  ⚠ Windows 请在 Git Bash 或真 <code>curl.exe</code> 中运行：
                  PowerShell 的 <code>curl</code> 是 <code>Invoke-WebRequest</code>{" "}
                  别名，既不认这种多行反斜杠续行，对本响应体也可能抛空引用错误。
                </p>
              </>
            )}
          </div>
        </>
      )}

      <div className="prefs-field">
        <label className="prefs-label">安全提示</label>
        <p className="prefs-hint status-text-warn">
          ⚠ 启用即表示：<strong>本机上任何拿到该令牌的程序</strong>都能通过此服务
          搜索并读取被 Scout 索引的文件内容。服务只绑 <code>127.0.0.1</code>、
          不对外网 / 局域网开放，令牌保存在本机 settings.json。若怀疑令牌泄露，
          请点「重置令牌」。不用时建议关闭。
        </p>
      </div>
    </div>
  );
}
