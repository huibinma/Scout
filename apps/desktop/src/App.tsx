import { useEffect, useRef, useState } from "react";
import {
  Compass,
  GearSix,
  Info,
  MagnifyingGlass,
} from "@phosphor-icons/react";
import { Route, Routes, useLocation, useNavigate } from "react-router-dom";
import SearchView from "./SearchView";
import AboutDialog from "./components/AboutDialog";
import PreferencesDialog from "./components/PreferencesDialog";
import ShortcutBanner from "./components/ShortcutBanner";
import StatusIndicator from "./components/StatusIndicator";
import UpdateToast from "./components/UpdateToast";
import { Category } from "./components/preferences/shared";
import { useShouldShowOnboarding } from "./hooks/useShouldShowOnboarding";
import { emitMenuAction, onMenuAction } from "./lib/menu-events";
import OnboardingMac from "./pages/OnboardingMac";
import OnboardingWin from "./pages/OnboardingWin";

type Workspace = "search" | "settings";

function App() {
  const navigate = useNavigate();
  const location = useLocation();
  const onboarding = useShouldShowOnboarding();
  const [settingsCategory, setSettingsCategory] =
    useState<Category>("general");
  const [showAbout, setShowAbout] = useState(false);
  const [settingsCloseRequest, setSettingsCloseRequest] = useState(0);
  // 设置页可能从 / 或 /onboarding/* 打开（快速入门第 5 步「打开索引选项」）；
  // 记录来源路径，关闭时原路返回，而非一律回到 /（否则会把用户从引导流程踢出）。
  const settingsReturnPath = useRef("/");

  const isOnboarding = location.pathname.startsWith("/onboarding");
  const workspace: Workspace = location.pathname.startsWith("/settings")
    ? "settings"
    : "search";

  const openSettings = (category: Category = "general") => {
    if (workspace !== "settings") {
      settingsReturnPath.current = location.pathname;
    }
    setSettingsCategory(category);
    navigate("/settings");
  };

  const closeSettings = () => navigate(settingsReturnPath.current);

  const openSearch = () => {
    if (workspace === "settings") {
      setSettingsCloseRequest((value) => value + 1);
    } else {
      navigate("/");
    }
  };

  useEffect(
    () =>
      onMenuAction((action) => {
        if (action === "open-prefs") openSettings("general");
        else if (action === "open-prefs-indexing") openSettings("indexing");
        else if (action === "open-prefs-misc") openSettings("misc");
        else if (action === "open-prefs-mcp") openSettings("mcp");
      }),
    [navigate],
  );

  // 传统菜单栏已移除，但保留用户已经形成肌肉记忆的高价值快捷键。
  // 引导流程期间不挂这些全局键：Ctrl+; 等会把用户导航到 /settings，
  // 中途打断快速入门且无法自动回到原步骤。
  useEffect(() => {
    if (isOnboarding) return;
    const onKeyDown = (event: KeyboardEvent) => {
      const cmdLike = event.ctrlKey || event.metaKey;
      if (!cmdLike || event.altKey) return;

      const key = event.key.toLowerCase();
      if (!event.shiftKey && (key === ";" || key === ",")) {
        event.preventDefault();
        if (workspace !== "settings") openSettings();
        return;
      }
      if (workspace !== "search") return;
      if (event.shiftKey && key === "c") {
        event.preventDefault();
        emitMenuAction("copy-path");
        return;
      }
      if (event.shiftKey) return;
      const actions: Record<string, Parameters<typeof emitMenuAction>[0]> = {
        n: "new-search",
        f: "focus-search",
        p: "toggle-preview",
        d: "save-search",
      };
      const action = actions[key];
      if (action) {
        event.preventDefault();
        emitMenuAction(action);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate, workspace, isOnboarding]);

  // 首次启动权限引导只自动跳转一次，避免完成后被旧状态拉回。
  const hasAutoRedirected = useRef(false);
  useEffect(() => {
    if (isOnboarding || hasAutoRedirected.current) return;
    if (onboarding === "macos") {
      hasAutoRedirected.current = true;
      navigate("/onboarding/mac");
    } else if (onboarding === "windows") {
      hasAutoRedirected.current = true;
      navigate("/onboarding/win");
    }
  }, [onboarding, navigate, isOnboarding]);

  if (isOnboarding) {
    return (
      <div className="onboarding-root">
        <Routes>
          <Route path="/onboarding/mac" element={<OnboardingMac />} />
          <Route path="/onboarding/win" element={<OnboardingWin />} />
        </Routes>
      </div>
    );
  }

  return (
    <div className="app-shell">
      <aside className="app-sidebar">
        <div className="brand-lockup">
          <img className="brand-icon" src="/scout-icon.png" alt="" />
          <div>
            <div className="brand-name">Scout</div>
            <div className="brand-tagline">Deep Local Search</div>
          </div>
        </div>

        <nav className="primary-nav" aria-label="主要功能">
          <button
            type="button"
            aria-label="找文件"
            className={`nav-item${workspace === "search" ? " active" : ""}`}
            onClick={openSearch}
          >
            <MagnifyingGlass size={20} weight="duotone" />
            <span>
              <strong>找文件</strong>
              <small>按关键字、语义搜索文件</small>
            </span>
          </button>
          <button
            type="button"
            aria-label="设置"
            className={`nav-item${workspace === "settings" ? " active" : ""}`}
            onClick={() => {
              if (workspace !== "settings") openSettings("general");
            }}
          >
            <GearSix size={20} weight="duotone" />
            <span>
              <strong>设置</strong>
              <small>索引、语义与隐私</small>
            </span>
          </button>
        </nav>

        <div className="sidebar-spacer" />

        <div className="sidebar-status">
          <div className="sidebar-section-label">本机服务</div>
          <StatusIndicator />
        </div>

        <div className="secondary-nav">
          <button
            type="button"
            className="secondary-nav-item"
            onClick={() =>
              navigate(
                /Win/i.test(navigator.platform)
                  ? "/onboarding/win"
                  : "/onboarding/mac",
              )
            }
          >
            <Compass size={18} weight="duotone" />
            快速入门
          </button>
          <button
            type="button"
            className="secondary-nav-item"
            onClick={() => setShowAbout(true)}
          >
            <Info size={18} weight="duotone" />
            关于 Scout
          </button>
        </div>
      </aside>

      <section className="workspace">
        <ShortcutBanner />
        <Routes>
          <Route path="/" element={<SearchView />} />
          <Route
            path="/settings"
            element={
              <PreferencesDialog
                key={settingsCategory}
                onClose={closeSettings}
                initialCategory={settingsCategory}
                closeRequestToken={settingsCloseRequest}
              />
            }
          />
        </Routes>
      </section>

      {showAbout && <AboutDialog onClose={() => setShowAbout(false)} />}
      <UpdateToast />
    </div>
  );
}

export default App;
