// Windows 版快速入门（6 步）：
// 1. 优化 Windows 搜索索引
// 2. 检测内置原生文件索引（可选加速，需管理员权限）
// 3. 下载嵌入模型（必需，语义召回底座）
// 4. 下载生成模型 Qwen3-0.6B（可选，复杂 NL 查询解析）
// 5. 配置索引目录
// 6. 首次索引
//
// 每步的核心内容由独立组件承担；本文件只做 step router + shell 组织 + 完成态记录。
// 每步都提供 skipAction（"跳过此步"）保证用户不会被困在任何一步。
import React, { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useNavigate } from 'react-router-dom';
import { ModelDownloadStep } from '../components/ModelDownloadStep';
import OnboardingShell from '../components/onboarding/OnboardingShell';
import NativeIndexCheckStep from '../components/onboarding/NativeIndexCheckStep';
import WindowsSearchCheckStep from '../components/onboarding/WindowsSearchCheckStep';
import PdftoppmCheckStep from '../components/onboarding/PdftoppmCheckStep';
import IndexRootsStep from '../components/onboarding/IndexRootsStep';
import FirstIndexStep from '../components/onboarding/FirstIndexStep';

type Step = 1 | 2 | 3 | 4 | 5 | 6;
const TOTAL_STEPS = 6;

// 设置页现在是独立路由（而非弹层），从「打开索引选项」跳过去会卸载本组件；
// 用 sessionStorage 记步数，回到 /onboarding/win 时才能接着上次的步骤，
// 而不是被打回第 1 步。
const STEP_STORAGE_KEY = 'scout-onboarding-win-step';
function readSavedStep(): Step {
  const raw = Number(sessionStorage.getItem(STEP_STORAGE_KEY));
  return raw >= 1 && raw <= TOTAL_STEPS ? (raw as Step) : 1;
}

export const OnboardingWin: React.FC = () => {
  const [step, setStep] = useState<Step>(readSavedStep);
  const navigate = useNavigate();

  useEffect(() => {
    sessionStorage.setItem(STEP_STORAGE_KEY, String(step));
  }, [step]);

  const goTo = (next: Step) => setStep(next);

  const handleOpenIndexingOptions = async () => {
    try {
      await invoke('open_windows_indexing_options');
    } catch (err) {
      alert(`无法打开索引选项: ${err}`);
    }
  };

  const markStep1Done = async () => {
    try {
      await invoke('complete_onboarding', { feature: 'windows_indexing' });
    } catch (err) {
      console.error('[OnboardingWin] complete_onboarding windows_indexing:', err);
    }
    goTo(2);
  };

  const markEmbeddingDone = async () => {
    try {
      await invoke('complete_onboarding', { feature: 'model_download' });
    } catch (err) {
      console.error('[OnboardingWin] complete_onboarding model_download:', err);
    }
    goTo(4);
  };

  const handleFinishOnboarding = () => {
    sessionStorage.removeItem(STEP_STORAGE_KEY);
    navigate('/');
  };

  return (
    <>
      {step === 1 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={1}
          title="第 1 步：优化 Windows 搜索索引"
          subtitle="必需 · 决定 Scout 能「看见」哪些目录里的文件"
          primaryAction={{
            label: '我已设置好，下一步',
            onClick: markStep1Done,
          }}
          skipAction={{
            label: '跳过此步',
            onClick: () => goTo(2),
          }}
          footerNote="提示：Windows 索引构建可能需要一些时间，可以让它在后台跑，你继续下一步。"
        >
          <p
            style={{
              color: 'var(--muted)',
              margin: 0,
              marginBottom: '10px',
              lineHeight: 1.55,
              fontSize: '13px',
            }}
          >
            Scout 借助 Windows 自带搜索索引来快速定位内容。请确认常用工作目录
            （桌面、文档、下载、你自己的项目文件夹）已被 Windows 索引服务收录。
          </p>

          {/* BETA-33 cycle 9：真实服务状态条（check_windows_search_indexed 真做后的消费点）。 */}
          <WindowsSearchCheckStep />

          <div
            style={{
              backgroundColor: 'var(--header-bg)',
              padding: '10px 14px',
              borderRadius: '10px',
              marginBottom: '10px',
              color: 'var(--fg)',
              fontSize: '12.5px',
              lineHeight: 1.6,
            }}
          >
            <div style={{ fontWeight: 600, marginBottom: '4px' }}>操作步骤</div>
            <ol style={{ paddingLeft: '18px', margin: 0 }}>
              <li>点下方「打开索引选项…」</li>
              <li>在打开的窗口里点「修改」</li>
              <li>勾选希望被搜索到的文件夹（默认已包含"用户文件夹"，通常已覆盖桌面 / 文档 / 下载）</li>
              <li>点「确定」，回本页点右下角「我已设置好，下一步」</li>
            </ol>
          </div>

          <button
            onClick={handleOpenIndexingOptions}
            style={{
              backgroundColor: '#1c1917',
              color: 'white',
              border: 'none',
              padding: '7px 18px',
              borderRadius: '7px',
              cursor: 'pointer',
              fontSize: '13px',
              fontWeight: 500,
            }}
          >
            打开索引选项…
          </button>
        </OnboardingShell>
      )}

      {step === 2 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={2}
          title="第 2 步：可选加速组件"
          subtitle="可选 · 内置文件名搜索加速（管理员权限）+ 扫描版 PDF OCR（poppler）"
          primaryAction={{
            label: '下一步',
            onClick: () => goTo(3),
          }}
          skipAction={{
            label: '跳过此步',
            onClick: () => goTo(3),
          }}
          secondaryAction={{
            label: '上一步',
            onClick: () => goTo(1),
          }}
        >
          <div style={{ display: 'flex', flexDirection: 'column', gap: '18px' }}>
            <div>
              <div
                style={{
                  fontSize: '13px',
                  fontWeight: 600,
                  color: 'var(--fg)',
                  marginBottom: '8px',
                }}
              >
                内置原生索引 — 文件名搜索加速
              </div>
              <NativeIndexCheckStep onReady={() => { /* 状态由组件自展示 */ }} />
            </div>
            <div>
              <div
                style={{
                  fontSize: '13px',
                  fontWeight: 600,
                  color: 'var(--fg)',
                  marginBottom: '8px',
                }}
              >
                Poppler (pdftoppm) — 扫描版 PDF OCR（BETA-35）
              </div>
              <PdftoppmCheckStep />
            </div>
          </div>
        </OnboardingShell>
      )}

      {step === 3 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={3}
          title="第 3 步：下载嵌入模型"
          subtitle="必需 · 让 Scout 能按「意思」检索，而不仅是关键词匹配"
          primaryAction={{
            label: '下一步',
            onClick: markEmbeddingDone,
          }}
          skipAction={{
            label: '跳过此步（稍后下载）',
            onClick: markEmbeddingDone,
          }}
          secondaryAction={{
            label: '上一步',
            onClick: () => goTo(2),
          }}
          footerNote="嵌入模型只在本地运行；下载后全程无需联网。"
        >
          <ModelDownloadStep
            kind="embedding"
            onComplete={markEmbeddingDone}
            // 隐藏组件内部的「稍后下载」按钮，改由 shell 底部统一提供 skip。
          />
        </OnboardingShell>
      )}

      {step === 4 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={4}
          title="第 4 步：下载生成模型（可选）"
          subtitle="可选 · 用于解析复杂多条件的自然语言查询"
          primaryAction={{
            label: '下一步',
            onClick: () => goTo(5),
          }}
          skipAction={{
            label: '跳过此步（不下载）',
            onClick: () => goTo(5),
          }}
          secondaryAction={{
            label: '上一步',
            onClick: () => goTo(3),
          }}
          footerNote="不装也不影响关键词与语义搜索；装了后类似「上周从张三收到的 Q3 PDF」这类复杂 query 解析成功率更高。"
        >
          <ModelDownloadStep
            kind="generation"
            onComplete={() => goTo(5)}
          />
        </OnboardingShell>
      )}

      {step === 5 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={5}
          title="第 5 步：配置索引目录"
          subtitle="必需 · 决定 Scout 会在哪些文件夹里搜索"
          primaryAction={{
            label: '下一步',
            onClick: () => goTo(6),
          }}
          skipAction={{
            label: '跳过此步（用默认）',
            onClick: () => goTo(6),
          }}
          secondaryAction={{
            label: '上一步',
            onClick: () => goTo(4),
          }}
        >
          <IndexRootsStep />
        </OnboardingShell>
      )}

      {step === 6 && (
        <OnboardingShell
          totalSteps={TOTAL_STEPS}
          currentStep={6}
          title="第 6 步：首次索引"
          subtitle="点「开始扫描并索引」启动首轮；索引跑起来后你可以随时进主界面"
          primaryAction={{
            label: '完成，进入主界面',
            onClick: handleFinishOnboarding,
          }}
          skipAction={{
            label: '跳过此步（不启动首轮）',
            onClick: handleFinishOnboarding,
          }}
          secondaryAction={{
            label: '上一步',
            onClick: () => goTo(5),
          }}
        >
          <FirstIndexStep onFinish={handleFinishOnboarding} />
        </OnboardingShell>
      )}
    </>
  );
};

export default OnboardingWin;
