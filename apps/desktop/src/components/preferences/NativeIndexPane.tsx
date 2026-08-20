import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings } from "../../hooks/useAppSettings";

/**
 * 重构（原 BETA-47 「Everything」面板）：「内置原生索引」面板（仅 Windows tab 树中出现）。
 * 检测（与开关独立——关了也要能告知「能不能用」）+ 集成总开关。
 * 不再有安装引导——内置服务无需安装，唯一前置条件是 Scout 需以管理员权限运行
 * （Win32 打开 NTFS 卷句柄的硬性要求，不是本实现的选择）。
 * 轮询节奏与 WindowsPane 对齐（3s→15s，纯 UI 状态刷新，不影响搜索行为）。
 */
export function NativeIndexPane({
  settings,
  setSettings,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
}) {
  // null = 首次检测中；true = 可用；false = 不可用（多半是未以管理员权限运行）。
  const [available, setAvailable] = useState<boolean | null>(null);

  const check = useCallback(async () => {
    try {
      setAvailable(await invoke<boolean>("check_native_file_index_available"));
    } catch (err) {
      console.error(
        "[NativeIndexPane] check_native_file_index_available failed:",
        err,
      );
      setAvailable(false);
    }
  }, []);

  useEffect(() => {
    void check();
    // 用户可能切去以管理员身份重启 Scout，定时轮询自动感知（与快速入门步骤同款，
    // 15s 只是 UI 状态刷新节奏，不影响搜索）。
    const t = setInterval(() => void check(), 15000);
    return () => clearInterval(t);
  }, [check]);

  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">检测</label>
        {available === null ? (
          <p className="prefs-status">正在检测内置原生索引…</p>
        ) : available ? (
          <p className="prefs-status status-text-ok">
            ✓ 内置原生索引可用（MFT 枚举 + USN Journal）
          </p>
        ) : (
          <p className="prefs-status status-text-warn">
            ⚠ 当前不可用——最常见原因是 Scout 未以管理员权限运行
          </p>
        )}
        <p className="prefs-hint">每 15 秒自动检测一次，无需手动重新检测。</p>
      </div>

      <div className="prefs-field">
        <label className="prefs-checkbox prefs-checkbox-strong">
          <input
            type="checkbox"
            checked={settings.enable_native_file_index}
            onChange={(e) =>
              setSettings({
                ...settings,
                enable_native_file_index: e.target.checked,
              })
            }
          />
          <strong>使用内置原生索引加速（推荐，无需安装任何软件）</strong>
        </label>
        <p className="prefs-hint">
          开启时 Scout 直接读取 NTFS 文件系统的 MFT（主文件表）与 USN Journal
          变更日志，在进程内维护一份极速内存索引，用于三处场景：① 按文件名搜索
          加速与 Windows 索引盲区兜底（如 <code>%TEMP%</code>、外接盘）；②
          建索引时的全盘快速发现（结果仅限所选索引目录）；③ 模型下载前的本机
          已有模型发现。无第三方进程、无需安装——唯一前置条件是 Scout 需以
          管理员权限运行；未满足时自动降级、不影响使用。
        </p>
        <p className="prefs-hint">
          关闭后 Scout 完全不使用内置原生索引：搜索加速部分
          <strong>需重启应用生效</strong>，全盘发现与模型发现保存后即生效
          （改为只扫描索引目录）。
        </p>
      </div>

      {available === false && (
        <div className="prefs-field">
          <label className="prefs-label">如何启用</label>
          <p className="prefs-hint">
            右键 Scout 快捷方式 →「以管理员身份运行」。这是 Windows
            打开卷句柄读取文件系统元数据的系统要求，不是 Scout 的额外权限索取——
            不开启也完全不影响语义/关键词搜索。
          </p>
        </div>
      )}
    </div>
  );
}
