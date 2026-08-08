// BETA-31 / BETA-33 cycle 3 v4：模型 GUI 下载 hook（embedding + generation 双模型共用）。
// 封装 invoke('download_<kind>_model') + Tauri event listen + 状态机。
// 与 backend model_download.rs 配对。

import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, UnlistenFn } from '@tauri-apps/api/event';

export type DownloadStatus = 'idle' | 'downloading' | 'done' | 'error';
export type ModelKind = 'embedding' | 'generation';

export interface DownloadProgress {
  downloaded: number;
  total: number | null;
  percent: number | null;
}

export interface UseModelDownload {
  status: DownloadStatus;
  progress: DownloadProgress;
  error: string | null;
  start: () => Promise<void>;
  cancel: () => Promise<void>;
}

/// HF 公开仓回退链接（下载失败时让用户手动下载 .gguf）。
export const EMBEDDING_MODEL_FALLBACK_URL =
  'https://huggingface.co/ggml-org/embeddinggemma-300M-qat-q8_0-gguf';
export const GENERATION_MODEL_FALLBACK_URL =
  'https://huggingface.co/unsloth/Qwen3-0.6B-GGUF';

/// 兼容旧引用：默认指 embedding 仓（v0.9.3 前只支持 embedding）。
export const MODEL_DOWNLOAD_FALLBACK_URL = EMBEDDING_MODEL_FALLBACK_URL;

const INITIAL_PROGRESS: DownloadProgress = {
  downloaded: 0,
  total: null,
  percent: null,
};

interface WiringByKind {
  invokeStart: string;
  invokeCancel: string;
  eventProgress: string;
  eventDone: string;
  eventError: string;
}

function wiringFor(kind: ModelKind): WiringByKind {
  if (kind === 'embedding') {
    return {
      invokeStart: 'download_embedding_model',
      invokeCancel: 'cancel_embedding_download',
      eventProgress: 'model-download://embedding/progress',
      eventDone: 'model-download://embedding/done',
      eventError: 'model-download://embedding/error',
    };
  }
  return {
    invokeStart: 'download_generation_model',
    invokeCancel: 'cancel_generation_download',
    eventProgress: 'model-download://generation/progress',
    eventDone: 'model-download://generation/done',
    eventError: 'model-download://generation/error',
  };
}

export function useModelDownload(kind: ModelKind = 'embedding'): UseModelDownload {
  const [status, setStatus] = useState<DownloadStatus>('idle');
  const [progress, setProgress] = useState<DownloadProgress>(INITIAL_PROGRESS);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const w = wiringFor(kind);
    let unlistenProgress: UnlistenFn | null = null;
    let unlistenDone: UnlistenFn | null = null;
    let unlistenError: UnlistenFn | null = null;
    let mounted = true;

    (async () => {
      unlistenProgress = await listen<{ downloaded: number; total: number | null }>(
        w.eventProgress,
        (event) => {
          if (!mounted) return;
          const { downloaded, total } = event.payload;
          const percent = total ? Math.min(100, (downloaded / total) * 100) : null;
          setProgress({ downloaded, total, percent });
        }
      );
      // listener leak 兜底：若组件在 await 期间已 unmount、立即 unlisten 防泄漏
      if (!mounted) { unlistenProgress(); return; }

      unlistenDone = await listen(w.eventDone, () => {
        if (!mounted) return;
        setStatus('done');
      });
      if (!mounted) { unlistenDone(); unlistenProgress(); return; }

      unlistenError = await listen<{ reason: string }>(
        w.eventError,
        (event) => {
          if (!mounted) return;
          // 用户主动取消时、cancel() 已设 status='idle'、不应让随后到达的
          // error event（reason="用户取消下载"）覆盖 UI 显示"下载失败"。
          if (event.payload.reason === '用户取消下载') return;
          setStatus('error');
          setError(event.payload.reason);
        }
      );
      if (!mounted) { unlistenError(); unlistenDone(); unlistenProgress(); return; }

      // v0.9.16 真机踩坑：切步重挂后前端回 idle、但后端下载还在进行（守卫持有）——
      // 用户看不到「取消」也无法导入。mount 时查后端 in-flight 恢复「下载中」态。
      try {
        const inFlight = await invoke<boolean>('model_download_in_flight', { kind });
        if (mounted && inFlight) setStatus('downloading');
      } catch {
        // 命令不可用（旧后端）时静默降级为原行为。
      }
    })();

    return () => {
      mounted = false;
      unlistenProgress?.();
      unlistenDone?.();
      unlistenError?.();
    };
  }, [kind]);

  const start = useCallback(async () => {
    const w = wiringFor(kind);
    setStatus('downloading');
    setError(null);
    setProgress(INITIAL_PROGRESS);
    try {
      await invoke(w.invokeStart);
      // 成功路径靠 event setStatus('done')
    } catch (e) {
      const msg = typeof e === 'string' ? e : JSON.stringify(e);
      // v0.9.16 真机踩坑：用户点「取消」后 invoke promise 以 Err("用户取消下载")
      // 拒绝——事件路径已过滤该 reason，但这里的 catch 没过滤，会把 cancel() 刚设的
      // idle 覆盖成「下载失败：用户取消下载」。主动取消不是失败，静默保持 idle。
      if (msg === '用户取消下载') return;
      // backend 已 emit error event、setStatus 也会 setError；这里兜底（防 event 丢失）
      setStatus('error');
      setError(msg);
    }
  }, [kind]);

  const cancel = useCallback(async () => {
    const w = wiringFor(kind);
    try {
      await invoke(w.invokeCancel);
      setStatus('idle');
      setProgress(INITIAL_PROGRESS);
    } catch (e) {
      console.error('cancel failed', e);
    }
  }, [kind]);

  return { status, progress, error, start, cancel };
}
