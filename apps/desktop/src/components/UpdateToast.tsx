import { DownloadSimple, Info, Warning, X } from "@phosphor-icons/react";
import { useAutoUpdate } from "../hooks/useAutoUpdate";
import { formatBytes } from "./preferences/shared";

// 自动更新提醒：后端每 8 小时轮询 GitHub Releases，发现新版本经 update://available
// event 通知（见 update.rs）。窗口左下角常驻 toast，不打断当前操作；点「更新」后台
// 下载 + 静默安装，装完进程自行重启，不需要额外的"完成"UI 态。
export function UpdateToast() {
  const { status, info, progress, error, install, dismiss } = useAutoUpdate();

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
