import React, { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
// BETA-33 cycle 9：EmbedStatus 类型 + 徽标文案改从单一信源引入（原三处复制 + 本组件
// tooltip 直接拼 raw expected_path / Rust 错误串，详见 lib/model-status.ts 顶注）。
import { EmbedStatus, embedStatusBadge } from "../lib/model-status";

interface BackendSummary {
  id: string;
  name: string;
  backend_kind: string | null;
  is_available: boolean;
  implementation_status: "real" | "stub";
}

/**
 * StatusIndicator 组件
 *
 * 顶部状态灯。每 backend 一个圆点：
 * - 绿色：可用 (Real)
 * - 灰色：不可用
 * - 红色：Fallback (Stub)
 *
 * BETA-33 cycle 3 v4：语义召回灯口径修正——不再只判 backend.is_available（那只表示
 * embedder handle 存在），额外查 `embedding_model_status`：state=ready 才绿；loading
 * 蓝；not_found/failed/unavailable 灰或黄。这样灯颜色与选项对话框「语义召回」pane 的
 * 状态源统一、不误导用户「灯绿但搜索报 embedding 模型不可用」（本次真机截图现场）。
 */
export const StatusIndicator: React.FC = () => {
  const [backends, setBackends] = useState<BackendSummary[]>([]);
  const [embedStatus, setEmbedStatus] = useState<EmbedStatus | null>(null);
  const [loading, setLoading] = useState(true);

  const fetchStatus = async () => {
    try {
      const [data, es] = await Promise.all([
        invoke<BackendSummary[]>("get_backend_status"),
        invoke<EmbedStatus>("embedding_model_status").catch(() => null),
      ]);
      setBackends(data);
      if (es) setEmbedStatus(es);
    } catch (err) {
      console.error("Failed to fetch backend status:", err);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchStatus();
    // 每 10 秒轮询一次（v0.9.4：从 30s 缩到 10s，让 embed 状态变化更快反映到顶栏灯上）。
    const interval = setInterval(fetchStatus, 10000);
    return () => clearInterval(interval);
  }, []);

  if (loading && backends.length === 0) {
    return <div className="backend-loading">正在检查…</div>;
  }

  // 顶栏「本机服务」只做最扼要的总览，不需要把每个内部 backend 都摆出来——
  // `search.native_file_index`（内置原生索引）是"本地索引"/"语义召回"背后
  // 共享的同一个 scoutd 连接，且它是可关闭的加速项而非独立能力，跟其余三个
  // 摆在一起会显得冗余；其可用性检测已经有专门位置（设置页「内置原生索引」
  // 面板 + 快速入门第 2 步），此处过滤掉，不重复展示。
  const visibleBackends = backends.filter(
    (backend) => backend.id !== "search.native_file_index",
  );

  return (
    <div className="backend-status-list">
      {visibleBackends.map((backend) => {
        let dotColor = "var(--status-neutral)"; // Default gray (unavailable)
        let statusText = "不可用";

        if (backend.is_available) {
          if (backend.implementation_status === "real") {
            dotColor = "var(--status-ok)"; // Green (available & real)
            statusText = "就绪";
          } else {
            dotColor = "var(--status-warn)"; // Amber (available but stub/fallback)
            statusText = "降级";
          }
        }

        // BETA-33 cycle 3 v4：语义召回专项覆写——用真实 EmbedStatus 而非 backend.is_available
        // （后者曾只判 embedder handle 存在；cycle 9 起 is_available 也走 is_ready() live 探测，
        // 但灯的五态粒度仍需 EmbedStatus）。cycle 9：文案改走 embedStatusBadge 单一信源，
        // tooltip 不再拼 raw 路径 / Rust 错误串，细节引导到「选项 → 语义召回」。
        if (backend.backend_kind === 'semantic_index' && embedStatus) {
          const badge = embedStatusBadge(embedStatus);
          dotColor = badge.color;
          statusText = badge.text;
        }

        return (
          <div
            key={backend.id}
            className="backend-status-row"
            title={`${backend.name}: ${statusText}`}
          >
            <span
              className="backend-status-dot"
              style={{ backgroundColor: dotColor }}
            />
            <span className="backend-status-name">{backend.name}</span>
            <span className="backend-status-text">{statusText}</span>
          </div>
        );
      })}
    </div>
  );
};

export default StatusIndicator;
