import { AppSettings } from "../../hooks/useAppSettings";
import { EverythingPane } from "./EverythingPane";
import { IS_WINDOWS } from "./shared";
import { WindowsPane } from "./WindowsPane";

/**
 * 「常规」面板：全局唤起快捷键 + 多条件检索匹配方式，Windows 平台下追加
 * Everything 加速 / Windows 搜索集成与托盘两个子分区。
 *
 * 2026-07-29：原独立的「Everything」「Windows」两个 tab 收进本面板——两者都是
 * "日常怎么用"的系统集成开关，跟常规设置同类，没必要各占一个顶级 tab、让分类树
 * 平白多两项（且只有 Windows 用户能看到，Mac 用户点开全是空 tab 的既往体验也一并
 * 消除）。BETA-47 生成模型 fallback / 模型路径覆盖仍在「语义召回 → 模型管理」。
 */
export function GeneralPane({
  settings,
  setSettings,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
}) {
  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">全局唤起快捷键</label>
        <input
          type="text"
          className="prefs-input"
          value={settings.global_shortcut}
          onChange={(e) =>
            setSettings({ ...settings, global_shortcut: e.target.value })
          }
          disabled
        />
        <p className="prefs-hint">当前版本暂不支持修改快捷键。</p>
      </div>
      <div className="prefs-field">
        <label className="prefs-label">多条件检索匹配方式</label>
        <select
          className="prefs-input"
          value={settings.search_match_all_conditions ? "all" : "any"}
          onChange={(e) =>
            setSettings({
              ...settings,
              search_match_all_conditions: e.target.value === "all",
            })
          }
        >
          <option value="all">全部条件都命中（推荐，更精确）</option>
          <option value="any">任一条件命中即可（更宽泛，结果更多）</option>
        </select>
        <p className="prefs-hint">
          搜索词被拆成多个条件（如同义词组）时，「全部命中」要求每个条件都满足，避免只符合部分条件的结果混入；
          「任一命中」放宽为只要满足一个条件就返回，召回更广但可能包含较多不相关结果。
        </p>
      </div>

      {IS_WINDOWS && (
        <>
          <div className="prefs-section-title">Everything 加速</div>
          <EverythingPane settings={settings} setSettings={setSettings} />

          <div className="prefs-section-title">Windows 搜索集成与托盘</div>
          <WindowsPane settings={settings} setSettings={setSettings} />
        </>
      )}
    </div>
  );
}
