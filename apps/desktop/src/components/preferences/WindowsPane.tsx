import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type WinIndexStatus = "Indexed" | "NotIndexed" | "Unknown" | "NotApplicable";

/**
 * BETA-47：「Windows」系统集成面板（仅 Windows tab 树中出现）。
 * Windows 搜索服务（WSearch）状态检测 + 打开系统「索引选项」——与快速入门第 1 步
 * （WindowsSearchCheckStep）同一探测命令，装完后从选项页也能随时复查。
 * 2026-07-26：曾加过「关闭到托盘」开关（settings.close_to_tray，Rust 侧 main.rs 的
 * CloseRequested 拦截 + tray.rs 消费）；2026-08-08 挪去 GeneralPane 顶部与「全局
 * 唤起快捷键」合并——两者是同一件事的一体两面（关窗不驻留托盘则快捷键唤不起已退出
 * 的进程），合并后快捷键录制器能直接反映这层依赖，不再各自为政。
 */
export function WindowsPane() {
  // null = 首次检测中。
  const [status, setStatus] = useState<WinIndexStatus | null>(null);
  const [msg, setMsg] = useState("");

  const check = useCallback(async () => {
    try {
      setStatus(await invoke<WinIndexStatus>("check_windows_search_indexed"));
    } catch (err) {
      console.error("[WindowsPane] check_windows_search_indexed failed:", err);
      setStatus("Unknown");
    }
  }, []);

  useEffect(() => {
    void check();
    // 2026-07-30：3s→15s——纯 UI 状态刷新节奏，与 EverythingPane 同步拉长，
    // 不影响实际搜索时的可用性判断（各后端在查询时独立探测）。
    const t = setInterval(() => void check(), 15000);
    return () => clearInterval(t);
  }, [check]);

  const openIndexingOptions = async () => {
    try {
      await invoke("open_windows_indexing_options");
    } catch (err) {
      setMsg(`打开索引选项失败: ${err}`);
      setTimeout(() => setMsg(""), 5000);
    }
  };

  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">Windows 搜索服务（系统搜索臂）</label>
        {status === null ? (
          <p className="prefs-status">正在检测 Windows 搜索服务…</p>
        ) : status === "Indexed" ? (
          <p className="prefs-status status-text-ok">
            ✓ Windows 搜索服务运行中——系统索引可用
          </p>
        ) : status === "NotIndexed" ? (
          <p className="prefs-status status-text-warn">
            ⚠ Windows 搜索服务未运行——Scout 仍可用本地索引 / Everything
            搜索，但系统搜索臂不可用。可在「服务」中启动 Windows Search（服务名
            WSearch）。
          </p>
        ) : (
          <p className="prefs-status">
            无法检测 Windows 搜索服务状态（不影响使用）。
          </p>
        )}
        <p className="prefs-hint">
          Scout 的系统搜索臂查询 Windows 自带索引（SystemIndex）；服务停止时该臂
          不可用，本地索引与语义召回不受影响。每 15 秒自动检测一次。
        </p>
      </div>

      <div className="prefs-field">
        <label className="prefs-label">系统索引范围</label>
        <p className="prefs-hint">
          哪些目录进 Windows 自带索引由系统「索引选项」管理（不属于 Scout
          配置）。若常用目录不在系统索引范围内，系统搜索臂会漏结果。
        </p>
        <div style={{ display: "flex", gap: "10px", alignItems: "center" }}>
          <button
            type="button"
            className="prefs-btn"
            onClick={() => void openIndexingOptions()}
          >
            打开系统「索引选项」…
          </button>
          {msg && <span className="prefs-status">{msg}</span>}
        </div>
      </div>
    </div>
  );
}
