// 桌面自动更新 hook：监听后端 update.rs 的 `update://*` event，驱动左下角提醒 toast。
// 后端每 8 小时轮询一次 GitHub Releases（见 update.rs 顶部注释），本 hook 只负责接
// event + 转发 invoke('install_update')，不做本地状态持久化——刷新/重启后如后端
// 再次检测到同一版本会重新 emit，属预期行为。

import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";

export type AutoUpdateStatus =
  | "idle"
  | "available"
  | "downloading"
  | "installing"
  | "error";

export interface UpdateInfo {
  version: string;
  notes: string;
  asset_name: string;
  asset_url: string;
  asset_size: number;
}

export interface DownloadProgress {
  downloaded: number;
  total: number | null;
  percent: number | null;
}

export interface UseAutoUpdate {
  status: AutoUpdateStatus;
  info: UpdateInfo | null;
  progress: DownloadProgress;
  error: string | null;
  install: () => Promise<void>;
  dismiss: () => void;
}

const INITIAL_PROGRESS: DownloadProgress = {
  downloaded: 0,
  total: null,
  percent: null,
};

export function useAutoUpdate(): UseAutoUpdate {
  const [status, setStatus] = useState<AutoUpdateStatus>("idle");
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [progress, setProgress] = useState<DownloadProgress>(INITIAL_PROGRESS);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    let unlistenAvailable: UnlistenFn | null = null;
    let unlistenProgress: UnlistenFn | null = null;
    let unlistenInstalling: UnlistenFn | null = null;

    (async () => {
      unlistenAvailable = await listen<UpdateInfo>("update://available", (event) => {
        if (!mounted) return;
        setInfo(event.payload);
        setStatus("available");
      });
      if (!mounted) {
        unlistenAvailable();
        return;
      }

      unlistenProgress = await listen<{ downloaded: number; total: number | null }>(
        "update://download-progress",
        (event) => {
          if (!mounted) return;
          const { downloaded, total } = event.payload;
          const percent = total ? Math.min(100, (downloaded / total) * 100) : null;
          setProgress({ downloaded, total, percent });
        },
      );
      if (!mounted) {
        unlistenProgress();
        unlistenAvailable();
        return;
      }

      unlistenInstalling = await listen("update://installing", () => {
        if (!mounted) return;
        setStatus("installing");
      });
    })();

    return () => {
      mounted = false;
      unlistenAvailable?.();
      unlistenProgress?.();
      unlistenInstalling?.();
    };
  }, []);

  const install = useCallback(async () => {
    if (!info) return;
    setStatus("downloading");
    setError(null);
    setProgress(INITIAL_PROGRESS);
    try {
      await invoke("install_update", {
        version: info.version,
        assetName: info.asset_name,
        assetUrl: info.asset_url,
      });
      // 成功路径：进程即将自行退出重启，不需要在此设置任何"完成"状态。
    } catch (e) {
      const msg = typeof e === "string" ? e : JSON.stringify(e);
      setStatus("error");
      setError(msg);
    }
  }, [info]);

  const dismiss = useCallback(() => {
    // 只影响本次会话展示；后端不持久化"已忽略"状态，下一轮 8 小时检查如果仍是
    // 旧版本会再次提醒，属预期行为。
    setStatus("idle");
  }, []);

  return { status, info, progress, error, install, dismiss };
}
