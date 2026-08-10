import { DownloadSimple, Info, Warning, X } from "@phosphor-icons/react";
import { UseAutoUpdate } from "../hooks/useAutoUpdate";
import { formatBytes } from "./preferences/shared";

// 自动更新提醒：后端每 4 小时（默认，设置里可调）轮询 GitHub Releases，发现新版本经
// update://available event 通知，前端挂载时也会主动即时查一次兜底（见 update.rs /
// useAutoUpdate.ts 顶部注释）。窗口左下角常驻 toast，不打断当前操作；点「更新」后台
// 下载 + 静默安装，装完进程自行重启，不需要额外的"完成"UI 态。
//
// **状态来自 props、不在本组件内部调 useAutoUpdate()**：「关于 Scout」弹窗的手动
// 检查按钮也要驱动同一份更新状态（命中新版本时两处应该是同一个 toast，而不是各自
// 独立的两份状态），所以由 App.tsx 统一持有单个 hook 实例、经 props 分发。
export function UpdateToast({
  state,
}: {
  state: Pick<
    UseAutoUpdate,
    "status" | "info" | "progress" | "error" | "install" | "dismiss"
  >;
}) {
  const { status, info, progress, error, install, dismiss } = state;

  if (status === "idle" || !info) return null;

  return (
    <div className="update-toast" role="status">
      {status === "available" && (
        <>
          <Info size={18} weight="fill" />
          <div className="update-toast-body">
            <strong>发现新版本 v{info.version}</strong>
            {info.notes && <p className="update-toast-notes">{info.notes}</p>}
          </div>
          <button type="button" className="update-toast-primary" onClick={install}>
            更新
          </button>
          <button type="button" onClick={dismiss} aria-label="关闭更新提醒">
            <X size={15} />
          </button>
        </>
      )}

      {status === "downloading" && (
        <>
          <DownloadSimple size={18} className="update-toast-spin" />
          <div className="update-toast-body">
            <strong>正在下载更新 v{info.version}</strong>
            <div className="update-toast-progress">
              <div
                className="update-toast-progress-fill"
                style={{
                  width: progress.percent !== null ? `${progress.percent}%` : "8%",
                }}
              />
            </div>
            <p className="update-toast-notes">
              {formatBytes(progress.downloaded)}
              {progress.total ? ` / ${formatBytes(progress.total)}` : ""}
            </p>
          </div>
        </>
      )}

      {status === "installing" && (
        <>
          <DownloadSimple size={18} weight="fill" />
          <div className="update-toast-body">
            <strong>正在安装，即将自动重启…</strong>
          </div>
        </>
      )}

      {status === "error" && (
        <>
          <Warning size={18} weight="fill" />
          <div className="update-toast-body">
            <strong>更新失败</strong>
            <p className="update-toast-notes">
              {error ?? "未知错误"} · 可到 GitHub Releases 页手动下载安装。
            </p>
          </div>
          <button type="button" onClick={dismiss} aria-label="关闭更新提醒">
            <X size={15} />
          </button>
        </>
      )}
    </div>
  );
}

export default UpdateToast;
