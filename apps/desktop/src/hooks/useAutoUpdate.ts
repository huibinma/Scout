// 桌面自动更新 hook：监听后端 update.rs 的 `update://*` event，驱动左下角提醒 toast。
// 后端每 4 小时轮询一次 GitHub Releases（见 update.rs 顶部注释），本 hook 只负责接
// event + 转发 invoke('install_update')，不做本地状态持久化——刷新/重启后如后端
// 再次检测到同一版本会重新 emit，属预期行为。
//
// **2026-08-10 真机反馈修复**：后台轮询的 `update://available` 是 fire-and-forget
// 广播，若这里的 `listen()` 还没注册完（webview JS 运行时冷启动有窗口期）事件会
// 直接丢失、下一次要等一整个轮询周期才会再提醒——真机测试"打开等一会儿"完全可能
// 落在这个窗口里，看起来就是"自动更新没反应"。挂载时额外主动调一次
// `check_for_updates`（request/response，不会丢）兜底，不再只靠后台 emit。

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
  /** 手动/即时检查一次（request/response，不经过事件、不会丢）。命中新版本时
   *  同步把 status 置 available（左下角 toast 自然出现）并原样返回该版本信息；
   *  没有更新则返回 null；请求失败原样 throw，交由调用方决定如何展示——
   *  「关于」弹窗的手动按钮要展示错误，后台静默检查则选择吞掉不打扰用户。 */
  checkNow: () => Promise<UpdateInfo | null>;
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

  const checkNow = useCallback(async (): Promise<UpdateInfo | null> => {
    const result = await invoke<UpdateInfo | null>("check_for_updates");
    if (result) {
      setInfo(result);
      setStatus("available");
    }
    return result;
  }, []);

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

      // 监听器就绪后立即主动查一次（request/response，见文件顶部注释）——不再
      // 只靠后台轮询 30 秒后的 emit，也不受它的事件丢失窗口影响。静默失败：
      // 网络抖动/限流不该在每次启动时弹一个用户没主动要求的错误提示，后台
      // 轮询循环稍后仍会按周期重试。
      checkNow().catch((e) => {
        console.error("启动即时检查更新失败（后台轮询稍后仍会重试）", e);
      });
    })();

    return () => {
      mounted = false;
      unlistenAvailable?.();
      unlistenProgress?.();
      unlistenInstalling?.();
    };
  }, [checkNow]);

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
    // 只影响本次会话展示；后端不持久化"已忽略"状态，下一轮轮询（默认 4 小时，
    // 设置里可调）如果仍是旧版本会再次提醒，属预期行为。
    setStatus("idle");
  }, []);

  return { status, info, progress, error, install, dismiss, checkNow };
}
