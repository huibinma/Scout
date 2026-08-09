import { AppSettings } from "../../hooks/useAppSettings";
import { EverythingPane } from "./EverythingPane";
import { ShortcutRecorder } from "./ShortcutRecorder";
import { IS_WINDOWS } from "./shared";
import { WindowsPane } from "./WindowsPane";

/**
 * 「常规」面板：全局唤起快捷键 + 多条件检索匹配方式，Windows 平台下追加
 * Everything 加速 / Windows 搜索集成两个子分区。
 *
 * 2026-07-29：原独立的「Everything」「Windows」两个 tab 收进本面板——两者都是
 * "日常怎么用"的系统集成开关，跟常规设置同类，没必要各占一个顶级 tab、让分类树
 * 平白多两项（且只有 Windows 用户能看到，Mac 用户点开全是空 tab 的既往体验也一并
 * 消除）。BETA-47 生成模型 fallback / 模型路径覆盖仍在「语义召回 → 模型管理」。
 *
 * 2026-08-08：「关闭窗口时驻留系统托盘」从 WindowsPane 底部挪到本面板顶部、
 * 与「全局唤起快捷键」合并同一分区——两者本就是同一件事的一体两面：关窗后
 * Scout 若没有驻留托盘（该开关关闭），Windows 上点关闭按钮就是真退出进程，
 * 全局快捷键的监听也随进程一起没了，配置了也唤不起来。之前两者分居面板两端、
 * 互不知情，容易出现「设了快捷键、一关窗口就失效」的困惑。现在关掉托盘驻留时
 * 快捷键录制器直接置灰，逻辑关系体现在可交互状态上，不必靠读文档才知道。
 * macOS 原生就有「关窗不退出、Dock 图标常驻」的心智，没有这个开关，快捷键
 * 恒可用，故仅 Windows 下有这层依赖。
 */
export function GeneralPane({
  settings,
  setSettings,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
}) {
  const shortcutDisabled = IS_WINDOWS && !settings.close_to_tray;

  return (
    <div className="prefs-form">
      {IS_WINDOWS && (
        <div className="prefs-field">
          <label className="prefs-checkbox">
            <input
              type="checkbox"
              checked={settings.close_to_tray}
              onChange={(e) =>
                setSettings({ ...settings, close_to_tray: e.target.checked })
              }
            />
            <span>
              <strong>关闭窗口时驻留系统托盘</strong>
              <br />
              <span className="prefs-hint">
                开启后，点窗口右上角关闭按钮只隐藏窗口——Scout
                继续在后台索引、监听全局快捷键；系统托盘保留一个常驻图标，随时可通过全局快捷键或托盘图标唤起。托盘菜单里的「退出
                Scout」才会真正退出程序。关闭本开关后，关窗恢复为直接退出——此时全局快捷键唤不起一个已经退出的程序，故下方快捷键录制器会一并禁用。
              </span>
            </span>
          </label>
        </div>
      )}
      <ShortcutRecorder
        initialValue={settings.global_shortcut}
        disabled={shortcutDisabled}
        disabledHint="需先开启上方「关闭窗口时驻留系统托盘」——否则关闭窗口即完全退出程序，全局快捷键唤不起一个已经退出的进程。"
      />
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

      <div className="prefs-field">
        <label className="prefs-checkbox">
          <input
            type="checkbox"
            checked={settings.auto_update_enabled}
            onChange={(e) =>
              setSettings({ ...settings, auto_update_enabled: e.target.checked })
            }
          />
          <span>
            <strong>自动检查更新</strong>
            <br />
            <span className="prefs-hint">
              定期检查 GitHub 上是否有新版本，发现新版本会在窗口左下角提醒，点「更新」后台下载并静默安装，安装时保留所有配置、数据与
              MCP token，装完自动重启。
            </span>
          </span>
        </label>
        <label
          className="prefs-label"
          htmlFor="auto-update-interval"
          style={{ marginTop: 10 }}
        >
          检查间隔
        </label>
        <select
          id="auto-update-interval"
          className="prefs-input"
          disabled={!settings.auto_update_enabled}
          value={settings.auto_update_interval_minutes}
          onChange={(e) =>
            setSettings({
              ...settings,
              auto_update_interval_minutes: Number(e.target.value),
            })
          }
        >
          <option value={30}>30 分钟</option>
          <option value={60}>1 小时</option>
          <option value={120}>2 小时</option>
          <option value={240}>4 小时（默认）</option>
          <option value={360}>6 小时</option>
          <option value={480}>8 小时</option>
          <option value={720}>12 小时</option>
          <option value={1440}>24 小时</option>
        </select>
      </div>

      {IS_WINDOWS && (
        <>
          <div className="prefs-section-title">Everything 加速</div>
          <EverythingPane settings={settings} setSettings={setSettings} />

          <div className="prefs-section-title">Windows 搜索集成</div>
          <WindowsPane />
        </>
      )}
    </div>
  );
}
