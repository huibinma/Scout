// 快速入门 Windows 独有步骤：内置原生文件索引（MFT 枚举 + USN Journal）检测。
// 重构：不再集成外部 Everything（es.exe），改用内置服务，无需安装。BETA-78 后
// 索引跑在后台 scoutd 服务（LocalSystem 常驻，能读 NTFS MFT），桌面本身是非管理员
// 瘦客户端——「可用」= 是否连上 scoutd，与桌面进程自身的权限无关。
// 复用 `get_backend_status`（后端已注册 search.native_file_index），前端 filter 判 is_available。
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface BackendSummary {
  id: string;
  name: string;
  backend_kind: string | null;
  is_available: boolean;
  implementation_status: "real" | "stub";
}

export interface NativeIndexCheckStepProps {
  onReady: () => void;
}

export const NativeIndexCheckStep: React.FC<NativeIndexCheckStepProps> = () => {
  // null = 首次加载中；true = 可用；false = 不可用（后台服务 Scoutd 尚未连接）。
  const [available, setAvailable] = useState<boolean | null>(null);
  const [checking, setChecking] = useState(false);

  const check = useCallback(async () => {
    setChecking(true);
    try {
      const list = await invoke<BackendSummary[]>("get_backend_status");
      const native = list.find((b) => b.id === "search.native_file_index");
      setAvailable(native?.is_available ?? false);
    } catch (err) {
      console.error("[NativeIndexCheckStep] get_backend_status failed:", err);
      setAvailable(false);
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check();
    // 用户可能在这一步切去以管理员身份重新启动 Scout。3s 一轮询自动感知。
    const t = setInterval(() => void check(), 3000);
    return () => clearInterval(t);
  }, [check]);

  if (available === null) {
    return (
      <div style={{ padding: "8px 0", color: "var(--muted)", fontSize: "13px" }}>
        正在检测内置原生索引是否可用…
      </div>
    );
  }

  if (available) {
    return (
      <div
        style={{
          padding: "12px 14px",
          borderRadius: "10px",
          backgroundColor: "var(--status-ok-bg)",
          border: "1px solid rgba(34, 197, 94, 0.35)",
        }}
      >
        <div style={{ fontSize: "14px", color: "var(--status-ok-fg)", marginBottom: "3px" }}>
          ✓ 内置原生索引已就绪（MFT 枚举 + USN Journal）
        </div>
        <div style={{ fontSize: "12.5px", color: "var(--status-ok-fg)", lineHeight: 1.5 }}>
          Scout 会在文件名搜索、"忽然想不起在哪个盘"等场景下自动使用它加速；
          你不需要做任何额外配置，也无需安装第三方软件。
        </div>
      </div>
    );
  }

  return (
    <>
      <p
        style={{
          color: "var(--muted)",
          margin: 0,
          marginBottom: "10px",
          lineHeight: 1.55,
          fontSize: "13px",
        }}
      >
        Scout 内置了一套<strong>极速文件名搜索</strong>（直接读取 NTFS 文件系统的
        变更日志，无需安装第三方软件），能加速<strong>按文件名找文件</strong>、
        并在 Windows 索引未覆盖的路径下兜底（如 <code>%TEMP%</code>、外接盘）。
        <span style={{ color: "var(--status-warn-fg)" }}>
          当前不可用，后台服务 Scoutd 尚未连接——可能是刚装好还在启动，
          稍等片刻会自动重连；不影响语义/关键词搜索照常使用。
        </span>
      </p>

      <div
        style={{
          padding: "10px 12px",
          borderRadius: "10px",
          backgroundColor: "var(--header-bg)",
          marginBottom: "8px",
          fontSize: "12.5px",
          lineHeight: 1.55,
        }}
      >
        若长时间仍未连接，可打开「服务」（services.msc）确认「Scout
        后台索引与检索服务」是否在运行；本步可跳过，不影响后续引导。
      </div>

      <div style={{ display: "flex", gap: "10px", alignItems: "center" }}>
        <button
          onClick={() => void check()}
          disabled={checking}
          style={{
            backgroundColor: "#1c1917",
            color: "white",
            border: "none",
            padding: "7px 16px",
            borderRadius: "7px",
            cursor: checking ? "wait" : "pointer",
            fontSize: "13px",
            fontWeight: 500,
            opacity: checking ? 0.7 : 1,
          }}
        >
          {checking ? "正在检测…" : "重新检测"}
        </button>
        <span style={{ fontSize: "11.5px", color: "var(--subtle)" }}>
          每 3 秒自动检测一次
        </span>
      </div>
    </>
  );
};

export default NativeIndexCheckStep;
