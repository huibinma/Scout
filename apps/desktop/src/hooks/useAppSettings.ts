// BETA-33 cycle 9：AppSettings 类型 + 加载/保存/未保存判定的单一信源 hook。
//
// 此前 `AppSettings` 接口与「get_settings 加载 → 本地编辑 → update_settings 保存 →
// 快照重置」整套流在 PreferencesDialog 与 SettingsPage 各复制一份（~120 行漂移面）。
// 本 cycle 删除旧 `/settings` 路由 + SettingsPage（PreferencesDialog 自 cycle 3 起是
// 唯一 UI 入口、旧路由已无任何导航入口），设置流收拢到本 hook——未来任何新表面
// （如 onboarding 步骤）需要读改 settings 一律从这里取，不再复制。
import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** 单条 per-root 排除，patterns 是相对 root 的 path glob 列表（cycle 7-b）。 */
export interface RootExclude {
  root: string;
  patterns: string[];
}

/** 与后端 settings.rs `AppSettings` serde 对应（字段注释见各字段首次引入的 cycle）。 */
export interface AppSettings {
  global_shortcut: string;
  search_scope: string[];
  enable_model_fallback: boolean;
  enable_tracing: boolean;
  model_path: string | null;
  /** BETA-48：embedding 模型路径覆盖（null = 默认数据目录 models/）。
   *  此前接口缺该字段，`update_settings` 全量覆写会把用户手工写进
   *  settings.json 的值经 serde default 静默冲掉——必须透传。 */
  embedding_model_path: string | null;
  semantic_similarity_floor: number | null;
  semantic_weight: number | null;
  index_roots: string[];
  /** 是否纳入系统默认三夹（音频/文档/图片）。默认 false。
   *  2026-07-06 起与 index_roots 空否解耦：不勾 + 无自定义 = 默认零索引。 */
  include_system_defaults: boolean;
  /** BETA-39：图片 OCR 文本参与语义索引 opt-in（默认 false，防乱码 OCR 污染召回）。 */
  enable_image_semantics: boolean;
  /** 重构（原 BETA-47 Everything 集成总开关，默认 true）：内置原生文件索引
   *  （MFT 枚举 + USN Journal）总开关。关闭停用搜索加速（需重启）、索引期
   *  全盘发现与模型本地发现（live 生效）三处调用。 */
  enable_native_file_index: boolean;
  exclude_globs: string[];
  /** cycle 7-b：per-root 子路径排除（相对 root 的 path glob）。 */
  root_excludes: RootExclude[];
  /** 运行期自动增量索引间隔（分钟）。0 = 关闭。 */
  auto_index_interval_minutes: number;
  /** 2026-07-20：多个复合检索条件（关键词组）之间的匹配模式，全局配置。
   *  true（默认）= 全部复合条件命中（严格 AND）；false = 任一条件命中（OR，广召回）。
   *  各检索后端（本地索引 / Windows Search / 内置原生索引 / Spotlight）统一读取。 */
  search_match_all_conditions: boolean;
  /** 2026-07-26：Windows 专属——关闭主窗口时驻留系统托盘而非退出进程，默认 false。
   *  开启后点系统关闭按钮只隐藏窗口（后台索引 / 全局快捷键继续跑），托盘图标常驻、
   *  左键或全局快捷键唤起，托盘菜单「退出 Scout」才真正退出。其他平台忽略此字段
   *  （macOS 已有原生「关窗不退出」心智）。字段缺失该 BETA-48 同款风险：save() 全量
   *  覆写，接口漏字段会把用户已保存的值静默冲回 Rust default，必须透传。 */
  close_to_tray: boolean;
  /** 2026-08-09：自动更新总开关，默认 true。关闭后台后台检查循环每轮 live-read
   *  到 false 即跳过检查（不停循环），运行期切换无需重启。 */
  auto_update_enabled: boolean;
  /** 2026-08-09：自动更新轮询间隔（分钟），默认 240（4 小时）。允许范围
   *  [30, 1440]（半小时 ~ 24 小时），后端 `resolve_auto_update_interval_minutes`
   *  会 clamp 越界值。 */
  auto_update_interval_minutes: number;
}

/**
 * 设置的加载 / 编辑 / 保存流。
 *
 * - 挂载时 `get_settings` 一次，同步存 `initialSettings` 快照（识别 pending 改动 +
 *   未保存关闭前二次确认，cycle 7-a 语义保持不变）。
 * - `save()` 走 `update_settings`；成功后把当前 settings 快照进 initialSettings、
 *   置「设置已保存」3s 自清；失败置错误 message。返回是否成功（「确定」按钮用）。
 * - `message` / `setMessage` 一并暴露：调用方的其他轻提示（如 picker 反馈）复用同一条状态行。
 */
export function useAppSettings() {
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [initialSettings, setInitialSettings] = useState<AppSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState("");

  useEffect(() => {
    invoke<AppSettings>("get_settings")
      .then((s) => {
        setSettings(s);
        setInitialSettings(s);
      })
      .catch(console.error);
  }, []);

  // cycle 7-a：是否有未保存改动（sticky 提示 + 关闭前二次确认用）。
  const hasUnsavedChanges = useMemo(() => {
    if (!settings || !initialSettings) return false;
    return JSON.stringify(settings) !== JSON.stringify(initialSettings);
  }, [settings, initialSettings]);

  const save = async (): Promise<boolean> => {
    if (!settings) return false;
    setSaving(true);
    setMessage("");
    try {
      await invoke("update_settings", { settings });
      // cycle 7-a：应用成功后把当前 settings snapshot 到 initialSettings、清 pending/未保存状态。
      setInitialSettings(settings);
      setMessage("设置已保存");
      setTimeout(() => setMessage(""), 3000);
      return true;
    } catch (err) {
      setMessage(`保存失败: ${err}`);
      return false;
    } finally {
      setSaving(false);
    }
  };

  return {
    settings,
    setSettings,
    initialSettings,
    hasUnsavedChanges,
    save,
    saving,
    message,
    setMessage,
  };
}
