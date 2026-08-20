// BETA-31 / BETA-33 cycle 3 v4：模型下载共用组件（embedding + generation）。
// 用于 Onboarding Step 2、PreferencesDialog NotFound 行下方（旧 SettingsPage 已随 cycle 9 删除）。
// 2026-07-06（cycle 9 真机反馈）：下载 UI 前先做本地发现——默认路径已有 → 直接就绪；
// 否则经内置原生索引精确文件名全盘发现候选，「使用此文件」复制进默认目录（免重下 ~700MB）。
import React, { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import {
  useModelDownload,
  EMBEDDING_MODEL_FALLBACK_URL,
  GENERATION_MODEL_FALLBACK_URL,
  type ModelKind,
} from '../hooks/useModelDownload';

interface LocalModelCandidate {
  path: string;
  size_bytes: number;
}

interface DiscoverResult {
  present: boolean;
  expected_path: string;
  candidates: LocalModelCandidate[];
  native_index_available: boolean;
}

export interface ModelDownloadStepProps {
  onComplete: () => void;
  onSkip?: () => void;
  /// 紧凑模式：用于设置页 inline（无标题 / 无描述、仅按钮 + 进度条）。
  compact?: boolean;
  /// 模型种类。默认 embedding（保持 <=v0.9.3 调用点无参兼容）。
  kind?: ModelKind;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

interface CopyByKind {
  title: string;
  description: string;
  fallbackUrl: string;
  skipLabel: string;
  buttonLabel: string;
}

function copyFor(kind: ModelKind): CopyByKind {
  if (kind === 'embedding') {
    return {
      title: '下载嵌入模型（313 MB）',
      description:
        'Scout 用本地小模型把「按意思找到」做成现实：你输入中文，能召回英文文档；' +
        '记不清文件名，按主题描述也能命中。这一步把模型下载到本地，之后搜索全程不用网络。',
      fallbackUrl: EMBEDDING_MODEL_FALLBACK_URL,
      skipLabel: '稍后下载，先体验关键词搜索',
      buttonLabel: '下载模型',
    };
  }
  return {
    title: '下载生成模型 Qwen3-0.6B（~400 MB，可选）',
    description:
      '仅在解析复杂多条件自然语言查询（如「上周从张三收到的关于 Q3 报表的 PDF」）时才会触发；' +
      '日常关键词与语义召回不需要它。装了之后 parser 覆盖率从 88% 提升到 ~95%+。',
    fallbackUrl: GENERATION_MODEL_FALLBACK_URL,
    skipLabel: '暂不下载（当前搜索已可用）',
    buttonLabel: '下载 Qwen3-0.6B',
  };
}

export const ModelDownloadStep: React.FC<ModelDownloadStepProps> = ({
  onComplete,
  onSkip,
  compact = false,
  kind = 'embedding',
}) => {
  const { status, progress, error, start, cancel } = useModelDownload(kind);
  const copy = copyFor(kind);

  // 本地发现：mount 时查默认路径 + 内置原生索引候选。失败静默降级为原下载 UI。
  const [discover, setDiscover] = useState<DiscoverResult | null>(null);
  const [importing, setImporting] = useState<string | null>(null);
  const [importError, setImportError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    invoke<DiscoverResult>('discover_local_model', { kind })
      .then((r) => {
        if (alive) setDiscover(r);
      })
      .catch((e) => {
        console.error('[ModelDownloadStep] discover_local_model failed:', e);
      });
    return () => {
      alive = false;
    };
  }, [kind]);

  useEffect(() => {
    if (status === 'done') {
      const t = setTimeout(() => onComplete(), 500);
      return () => clearTimeout(t);
    }
  }, [status, onComplete]);

  // 2026-07-30：原先「默认路径已有完整模型」也会 500ms 后静默调用 onComplete()
  // 自动跳到下一步——onboarding 里这是唯一不需要用户任何操作就推进的环节（其余
  // 步骤即便后台检测早就绪，也都停在原地等用户点「下一步」），真机反馈体验成
  // "第 3/4 步莫名被跳过"。去掉这条纯后台触发的自动推进，只保留「✓ 已就绪」
  // 状态展示；推进统一交给 OnboardingWin/Mac 里显式的「下一步」按钮（step 3
  // 原本没有这个按钮，本次一并补上，见 OnboardingWin.tsx/OnboardingMac.tsx）。
  // 上面「下载完成」那条 effect 予以保留——那是用户主动点了下载、下载真正跑完
  // 后的收尾，不是无操作静默跳过，且 SemanticPane 设置页内嵌用它触发状态刷新。

  const importFrom = async (path: string) => {
    setImporting(path);
    setImportError(null);
    try {
      // 成功后 Rust 侧 emit 与下载一致的 done event → status 变 'done' → 既有流程收尾。
      await invoke<string>('import_local_model', { kind, source: path });
    } catch (e) {
      setImportError(String(e));
    } finally {
      setImporting(null);
    }
  };

  // 手动指定本地已下载的模型文件：不依赖内置原生索引自动发现（macOS 上恒不可用，
  // Windows 上也可能没装/没扫到）。走同一条 import_local_model 命令与 importFrom，
  // 后端仍按 acceptable_source_names 精确文件名校验（防误选其它模型触发 abort，
  // 见 model_download.rs 注释），选错文件名会走既有 importError 提示。
  const browseLocal = async () => {
    try {
      const { open } = await import('@tauri-apps/plugin-dialog');
      const picked = await open({
        multiple: false,
        filters: [{ name: 'GGUF 模型', extensions: ['gguf'] }],
      });
      if (typeof picked === 'string') {
        void importFrom(picked);
      }
    } catch (e) {
      console.error('[ModelDownloadStep] browse dialog failed:', e);
    }
  };

  const expectedFileName = discover?.expected_path.split(/[\\/]/).pop();

  const percentText =
    progress.percent !== null
      ? `${progress.percent.toFixed(1)}%`
      : progress.downloaded > 0
        ? formatBytes(progress.downloaded)
        : '准备中…';

  const containerStyle: React.CSSProperties = compact
    ? { padding: '12px 0' }
    : { padding: '20px', backgroundColor: 'var(--header-bg)', borderRadius: '12px', color: 'var(--fg)' };

  return (
    <div style={containerStyle}>
      {!compact && (
        <>
          <h2 style={{ fontSize: '18px', marginBottom: '8px' }}>{copy.title}</h2>
          <p style={{ color: 'var(--muted)', marginBottom: '16px', lineHeight: 1.6 }}>
            {copy.description}
          </p>
        </>
      )}

      {status === 'idle' && discover?.present && (
        <div style={{ color: 'var(--status-ok-fg)', fontSize: '14px' }}>
          ✓ 已在本机检测到模型（{discover.expected_path}），无需下载。
          {!compact && '点下方「下一步」继续。'}
        </div>
      )}

      {status === 'idle' && !discover?.present && (
        <div>
          {/* 本地发现候选：内置原生索引按精确文件名找到的同款模型，复制即用免重下。 */}
          {discover && discover.candidates.length > 0 && (
            <div
              style={{
                marginBottom: '14px',
                padding: '10px 12px',
                border: '1px solid rgba(240, 93, 50, 0.3)',
                backgroundColor: 'var(--accent-soft)',
                borderRadius: '8px',
              }}
            >
              <div style={{ fontSize: '13.5px', fontWeight: 600, marginBottom: '6px' }}>
                在本机找到已有的模型文件，可直接使用（复制进数据目录，免下载）：
              </div>
              {discover.candidates.map((c) => (
                <div
                  key={c.path}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: '10px',
                    padding: '4px 0',
                    fontSize: '12.5px',
                  }}
                >
                  <span style={{ flex: 1, wordBreak: 'break-all' }} title={c.path}>
                    📦 {c.path}{' '}
                    <span style={{ color: 'var(--subtle)' }}>({formatBytes(c.size_bytes)})</span>
                  </span>
                  <button
                    onClick={() => void importFrom(c.path)}
                    disabled={importing !== null}
                    style={{
                      backgroundColor: importing === c.path ? 'rgba(28, 25, 23, 0.5)' : '#1c1917',
                      color: 'white',
                      border: 'none',
                      padding: '5px 14px',
                      borderRadius: '6px',
                      cursor: importing !== null ? 'default' : 'pointer',
                      fontSize: '12.5px',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {importing === c.path ? '复制中…' : '使用此文件'}
                  </button>
                </div>
              ))}
            </div>
          )}
          {discover && !discover.native_index_available && !compact && (
            <p style={{ fontSize: '12px', color: 'var(--subtle)', margin: '0 0 10px' }}>
              内置原生索引不可用，无法自动发现本机已有模型——可在下方手动选择文件。
            </p>
          )}
          <div style={{ display: 'flex', gap: '12px', flexWrap: 'wrap', alignItems: 'center' }}>
            <button
              onClick={start}
              style={{
                backgroundColor: '#1c1917',
                color: 'white',
                border: 'none',
                padding: '10px 24px',
                borderRadius: '8px',
                cursor: 'pointer',
                fontSize: '14px',
                fontWeight: 500,
              }}
            >
              {copy.buttonLabel}
            </button>
            <button
              onClick={() => void browseLocal()}
              disabled={importing !== null}
              style={{
                backgroundColor: 'transparent',
                color: 'var(--fg)',
                border: '1px solid var(--border)',
                padding: '10px 24px',
                borderRadius: '8px',
                cursor: importing !== null ? 'default' : 'pointer',
                fontSize: '14px',
              }}
            >
              {importing !== null ? '导入中…' : '选择本地已下载的文件…'}
            </button>
            {onSkip && (
              <button
                onClick={onSkip}
                style={{
                  backgroundColor: 'transparent',
                  color: 'var(--muted)',
                  border: '1px solid var(--border)',
                  padding: '10px 24px',
                  borderRadius: '8px',
                  cursor: 'pointer',
                  fontSize: '14px',
                }}
              >
                {copy.skipLabel}
              </button>
            )}
          </div>
          {!compact && expectedFileName && (
            <p style={{ fontSize: '11.5px', color: 'var(--subtle)', margin: '8px 0 0' }}>
              手动选择的文件名需与 <code>{expectedFileName}</code> 完全一致（大小写不敏感），
              避免误选到其他模型。
            </p>
          )}
          {importError && (
            <div style={{ color: 'var(--status-err-fg)', fontSize: '12.5px', marginTop: '8px' }}>
              导入失败：{importError}
            </div>
          )}
        </div>
      )}

      {status === 'downloading' && (
        <div>
          <div style={{ marginBottom: '8px', fontSize: '14px', color: 'var(--fg)' }}>
            {percentText} · {formatBytes(progress.downloaded)}
            {progress.total ? ` / ${formatBytes(progress.total)}` : ''}
          </div>
          <div
            style={{
              height: '8px',
              backgroundColor: 'var(--border)',
              borderRadius: '4px',
              overflow: 'hidden',
              marginBottom: '12px',
            }}
          >
            <div
              style={{
                height: '100%',
                width: progress.percent !== null ? `${progress.percent}%` : '5%',
                backgroundColor: 'var(--accent)',
                transition: 'width 0.3s ease',
              }}
            />
          </div>
          <button
            onClick={cancel}
            style={{
              backgroundColor: 'transparent',
              color: 'var(--status-err-fg)',
              border: '1px solid var(--status-err-fg)',
              padding: '6px 16px',
              borderRadius: '6px',
              cursor: 'pointer',
              fontSize: '13px',
            }}
          >
            取消
          </button>
        </div>
      )}

      {status === 'done' && (
        <div style={{ color: 'var(--status-ok-fg)', fontSize: '14px' }}>
          ✓ 模型已就绪。{!compact && '即将进入下一步。'}
        </div>
      )}

      {status === 'error' && (
        <div>
          <div style={{ color: 'var(--status-err-fg)', marginBottom: '12px', fontSize: '14px' }}>
            下载失败：{error || '未知错误'}
          </div>
          <p style={{ fontSize: '13px', color: 'var(--muted)', lineHeight: 1.6 }}>
            网络问题？可手动下载 GGUF 文件并放到 app 数据目录的 <code>models/</code> 子目录：
          </p>
          <a
            href={copy.fallbackUrl}
            target="_blank"
            rel="noreferrer"
            style={{ color: 'var(--accent)', fontSize: '13px', wordBreak: 'break-all' }}
          >
            {copy.fallbackUrl}
          </a>
          <div style={{ marginTop: '12px', display: 'flex', gap: '12px' }}>
            <button
              onClick={start}
              style={{
                backgroundColor: '#1c1917',
                color: 'white',
                border: 'none',
                padding: '8px 20px',
                borderRadius: '6px',
                cursor: 'pointer',
                fontSize: '13px',
              }}
            >
              重试
            </button>
            {onSkip && (
              <button
                onClick={onSkip}
                style={{
                  backgroundColor: 'transparent',
                  color: 'var(--muted)',
                  border: '1px solid var(--border)',
                  padding: '8px 20px',
                  borderRadius: '6px',
                  cursor: 'pointer',
                  fontSize: '13px',
                }}
              >
                稍后下载
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
};

export default ModelDownloadStep;
