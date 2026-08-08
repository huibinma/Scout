// 快速入门共用步骤：触发首次索引。
// 用户可以随时点「完成」进主界面，索引在后台继续跑（不阻塞）。
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { phaseProgressText } from "../preferences/shared";

type IndexPhase = "music_discovery" | "music_scan" | "doc" | "image";

interface IndexStatus {
  indexing: boolean;
  last_indexed: string | null;
  last_summary: string | null;
  current_root: string | null;
  fts_progress: [number, number] | null;
  current_phase: IndexPhase | null;
  semantic_indexing: boolean;
  semantic_progress: [number, number] | null;
  semantic_summary: string | null;
  /** 2026-07-28：当前 phase 总数/内计数/速率——有值时算真百分比 + ETA，见 shared.ts phaseProgressText。 */
  phase_total: number | null;
  phase_scanned: number | null;
  phase_rate_per_min: number | null;
}

interface ReindexStats {
  music_added: number;
  music_updated: number;
  doc_added: number;
  doc_updated: number;
  image_added: number;
  image_updated: number;
}

function phaseLabel(phase: IndexPhase | null): string {
  switch (phase) {
    case "music_discovery":
      return "🎵 全盘发现音频";
    case "music_scan":
      return "🎵 扫描音频目录";
    case "doc":
      return "📄 扫描文档（关键词 + 语义）";
    case "image":
      return "🖼 扫描图片（OCR + 语义）";
    default:
      return "准备中…";
  }
}

export interface FirstIndexStepProps {
  onFinish: () => void;
}

export const FirstIndexStep: React.FC<FirstIndexStepProps> = ({
  // 目前「完成」由 shell 底部的 primaryAction 提供，本组件内无独立入口；保留 prop 以便未来扩展。
  onFinish: _onFinish,
}) => {
  const [status, setStatus] = useState<IndexStatus | null>(null);
  const [triggered, setTriggered] = useState(false);
  const [lastStats, setLastStats] = useState<ReindexStats | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const loadStatus = useCallback(async () => {
    try {
      const s = await invoke<IndexStatus>("get_index_status");
      setStatus(s);
    } catch (err) {
      console.error("[FirstIndexStep] get_index_status failed:", err);
    }
  }, []);

  useEffect(() => {
    void loadStatus();
    const t = setInterval(() => void loadStatus(), 1500);
    return () => clearInterval(t);
  }, [loadStatus]);

  const startIndexing = async () => {
    setTriggered(true);
    setErrorMsg(null);
    try {
      const stats = await invoke<ReindexStats>("reindex");
      setLastStats(stats);
    } catch (err) {
      // "正在索引中，请稍候" 不算错误——就是并发状态；轮询会拿到 indexing=true
      const msg = String(err);
      if (msg.includes("正在索引")) return;
      setErrorMsg(msg);
    }
  };

  const isIndexing = status?.indexing === true;
  const hasEverIndexed = status?.last_indexed !== null;

  const [ftsScanned, ftsIndexed] = status?.fts_progress ?? [0, 0];
  // 2026-07-28：真百分比 + ETA（有 phase_total/phase_scanned 时）；否则 null，
  // 下面渲染处退回裸数字展示（此前这里用 ftsIndexed/ftsScanned 算过一个百分比，
  // 但那实际是"扫描到的文件里有多少是新增/变更"，不是"整体进度"，语义不对，本轮改正）。
  const phaseProgress = phaseProgressText(
    status?.phase_total ?? null,
    status?.phase_scanned ?? null,
    status?.phase_rate_per_min ?? null,
  );

  const [semDone, semTotal] = status?.semantic_progress ?? [0, 0];
  const semPct =
    semTotal > 0 ? Math.min(100, (semDone / semTotal) * 100) : null;

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
        点下面按钮启动<strong>首轮索引</strong>：扫描目录 · 抽取文本 / 音频元数据 / 图片
        OCR · 生成语义向量。几分钟到几十分钟不等；
        <strong>你随时可以点「完成」进主界面，索引后台继续跑。</strong>
      </p>

      <div
        style={{
          padding: "10px 12px",
          borderRadius: "10px",
          backgroundColor: "var(--header-bg)",
          marginBottom: "10px",
        }}
      >
        {!isIndexing && !triggered && !hasEverIndexed && (
          <button
            onClick={() => void startIndexing()}
            style={{
              backgroundColor: "#1c1917",
              color: "white",
              border: "none",
              padding: "7px 18px",
              borderRadius: "7px",
              cursor: "pointer",
              fontSize: "13px",
              fontWeight: 500,
            }}
          >
            开始扫描并索引
          </button>
        )}

        {!isIndexing && hasEverIndexed && (
          <div>
            <div
              style={{
                color: "var(--status-ok-fg)",
                fontSize: "13px",
                marginBottom: "3px",
              }}
            >
              ✓ 首轮索引已完成
            </div>
            {status?.last_summary && (
              <div
                style={{
                  fontSize: "12.5px",
                  color: "var(--status-ok-fg)",
                  marginBottom: "4px",
                }}
              >
                {status.last_summary}
              </div>
            )}
            {lastStats && (
              <div style={{ fontSize: "11.5px", color: "var(--muted)" }}>
                本次：音频 +{lastStats.music_added}/~{lastStats.music_updated}，
                文档 +{lastStats.doc_added}/~{lastStats.doc_updated}，
                图片 +{lastStats.image_added}/~{lastStats.image_updated}
              </div>
            )}
            <button
              onClick={() => void startIndexing()}
              style={{
                marginTop: "6px",
                backgroundColor: "transparent",
                color: "#1c1917",
                border: "1px solid #1c1917",
                padding: "3px 12px",
                borderRadius: "5px",
                cursor: "pointer",
                fontSize: "11.5px",
              }}
            >
              重新索引
            </button>
          </div>
        )}

        {isIndexing && (
          <div>
            <div
              style={{
                fontSize: "12.5px",
                fontWeight: 500,
                color: "var(--fg)",
                marginBottom: "4px",
              }}
            >
              {phaseLabel(status?.current_phase ?? null)}
            </div>
            {status?.current_root && (
              <div
                style={{
                  fontSize: "11.5px",
                  color: "var(--muted)",
                  marginBottom: "6px",
                  wordBreak: "break-all",
                }}
              >
                当前目录：{status.current_root}
              </div>
            )}

            <div style={{ marginBottom: "6px" }}>
              <div
                style={{
                  fontSize: "11.5px",
                  color: "var(--fg)",
                  marginBottom: "2px",
                }}
              >
                {phaseProgress
                  ? `关键词索引（FTS）：${phaseProgress.label}`
                  : `关键词索引（FTS）：已扫描 ${ftsScanned} · 已入库 ${ftsIndexed}`}
              </div>
              <div
                style={{
                  height: "5px",
                  backgroundColor: "var(--border)",
                  borderRadius: "3px",
                  overflow: "hidden",
                }}
              >
                <div
                  style={{
                    height: "100%",
                    width: phaseProgress ? `${phaseProgress.pct}%` : "5%",
                    backgroundColor: "var(--accent)",
                    transition: "width 0.3s ease",
                  }}
                />
              </div>
            </div>

            {status?.semantic_indexing && (
              <div>
                <div
                  style={{
                    fontSize: "11.5px",
                    color: "var(--fg)",
                    marginBottom: "2px",
                  }}
                >
                  语义索引（embedding）：{semDone} / {semTotal}
                  {semPct !== null && `（${semPct.toFixed(1)}%）`}
                </div>
                <div
                  style={{
                    height: "5px",
                    backgroundColor: "var(--border)",
                    borderRadius: "3px",
                    overflow: "hidden",
                  }}
                >
                  <div
                    style={{
                      height: "100%",
                      width: semPct !== null ? `${semPct}%` : "5%",
                      backgroundColor: "var(--status-ok-fg)",
                      transition: "width 0.3s ease",
                    }}
                  />
                </div>
              </div>
            )}
          </div>
        )}

        {!isIndexing && triggered && !hasEverIndexed && !errorMsg && (
          <div style={{ fontSize: "12.5px", color: "var(--muted)" }}>正在启动…</div>
        )}

        {errorMsg && (
          <div style={{ color: "var(--status-err-fg)", fontSize: "12.5px" }}>
            索引启动失败：{errorMsg}
          </div>
        )}
      </div>
    </>
  );
};

export default FirstIndexStep;
