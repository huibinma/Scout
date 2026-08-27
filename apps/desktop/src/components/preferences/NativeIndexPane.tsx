import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings } from "../../hooks/useAppSettings";

/**
 * 重构（原 BETA-47 「Everything」面板）：「内置原生索引」面板（仅 Windows tab 树中出现）。
 * 检测（与开关独立——关了也要能告知「能不能用」）+ 集成总开关。
 * 不再有安装引导——内置服务无需安装。BETA-78 后内置原生索引跑在后台 `scoutd`
 * 服务（`LocalSystem` 常驻，能读 NTFS MFT），桌面本身是非管理员的瘦客户端，
 * 「可不可用」= 是否连上 scoutd（`service_connection_status`，与
 * search.local/search.semantic 共享同一份连接，跟 `enable_native_file_index`
 * 开关无关——即使暂时关着集成，这个检测反映的仍是服务本身的连接态）。
 * 修复：此前直接调用桌面进程内的 `check_native_file_index_available`（探测桌面
 * 进程自己能不能打开 NTFS 卷句柄）——这是 BETA-78 之前的判据，重构后桌面进程
 * 恒无管理员权限，会一直误报「不可用」并指导用户做一个不解决问题的操作。
 * 轮询节奏与 WindowsPane 对齐（3s→15s，纯 UI 状态刷新，不影响搜索行为）。
 */
export function NativeIndexPane({
  settings,
  setSettings,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
}) {
  // null = 首次检测中；true = 已连接 scoutd；false = 未连接（服务未装/未起/还在启动）。
  const [available, setAvailable] = useState<boolean | null>(null);

  const check = useCallback(async () => {
    try {
      setAvailable(await invoke<boolean>("service_connection_status"));
    } catch (err) {
      console.error(
        "[NativeIndexPane] service_connection_status failed:",
        err,
      );
      setAvailable(false);
    }
  }, []);

  useEffect(() => {
    void check();
    // 服务可能刚开机还在启动、或用户刚修完问题重启了它，定时轮询自动感知
    // （与快速入门步骤同款，15s 只是 UI 状态刷新节奏，不影响搜索）。
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
            ⚠ 当前不可用——后台服务 Scoutd 尚未连接（可能还在启动，或未成功安装）
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
          已有模型发现。无第三方进程、无需安装——由后台服务 Scoutd 常驻维护，
          桌面本身无需任何额外权限；服务未连接时自动降级、不影响使用。
        </p>
        <p className="prefs-hint">
          关闭后 Scout 完全不使用内置原生索引：搜索加速部分
          <strong>需重启应用生效</strong>，全盘发现与模型发现保存后即生效
          （改为只扫描索引目录）。
        </p>
      </div>

      {available === false && (
        <div className="prefs-field">
          <label className="prefs-label">如何排查</label>
          <p className="prefs-hint">
            打开「服务」（<code>services.msc</code>）确认「Scout 后台索引与检索服务」
            （Scoutd）是否已注册且处于「正在运行」；若刚装好或刚开机，服务可能还在
            启动中，稍候会自动重连。仍不行可查看
            <code>%ProgramData%\Scout\scoutd\scoutd.log</code>
            排查具体原因，或重新安装 Scout。不开启也完全不影响语义/关键词搜索。
          </p>
        </div>
      )}
    </div>
  );
}
