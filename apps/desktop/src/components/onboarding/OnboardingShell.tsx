// 快速入门共用外壳：stepper + 页面容器 + 底部三槽动作按钮。
// Win/Mac Onboarding 复用；两版差异只在步骤数、内容组件与 step 数组。
//
// 底部按钮布局（从左到右）：
//   [secondaryAction: "上一步"]                      [skipAction: "跳过此步"] [primaryAction: "下一步"]
import React from "react";

interface Action {
  label: string;
  onClick: () => void | Promise<void>;
  disabled?: boolean;
  variant?: "primary" | "secondary" | "ghost";
}

interface OnboardingShellProps {
  totalSteps: number;
  /** 1-based 当前步。 */
  currentStep: number;
  title: string;
  /** 副标题一行，写"为什么这一步"或"可选/必需"等元信息。 */
  subtitle?: string;
  children: React.ReactNode;
  /** 主动作（右下、黑色）。缺省时表示当前步的推进由 body 内组件回调触发。 */
  primaryAction?: Action;
  /** 次动作（左下、幽灵）。通常是"上一步"；第 1 步隐藏。 */
  secondaryAction?: Action;
  /** 跳过动作（右下、primary 左侧、幽灵）。每一步都建议提供。 */
  skipAction?: Action;
  /** 底部小字：提示信息、privacy 注脚等。 */
  footerNote?: string;
}

const stepperDot = (active: boolean): React.CSSProperties => ({
  width: "20px",
  height: "20px",
  borderRadius: "50%",
  backgroundColor: active ? "var(--accent)" : "var(--border)",
  color: active ? "white" : "var(--muted)",
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
  fontSize: "11px",
  fontWeight: 600,
  flexShrink: 0,
});

const buttonStyle = (variant: Action["variant"] = "primary"): React.CSSProperties => {
  const base: React.CSSProperties = {
    border: "none",
    padding: "9px 22px",
    borderRadius: "7px",
    cursor: "pointer",
    fontSize: "14px",
    fontWeight: 500,
  };
  if (variant === "primary") {
    return { ...base, backgroundColor: "#1c1917", color: "white" };
  }
  if (variant === "secondary") {
    return {
      ...base,
      backgroundColor: "var(--header-bg)",
      color: "var(--fg)",
    };
  }
  return {
    ...base,
    background: "none",
    color: "var(--muted)",
    padding: "9px 10px",
  };
};

const renderButton = (action: Action) => (
  <button
    onClick={() => void action.onClick()}
    disabled={action.disabled}
    style={{
      ...buttonStyle(action.variant ?? "primary"),
      opacity: action.disabled ? 0.5 : 1,
      cursor: action.disabled ? "not-allowed" : "pointer",
    }}
  >
    {action.label}
  </button>
);

export const OnboardingShell: React.FC<OnboardingShellProps> = ({
  totalSteps,
  currentStep,
  title,
  subtitle,
  children,
  primaryAction,
  secondaryAction,
  skipAction,
  footerNote,
}) => {
  const steps = Array.from({ length: totalSteps }, (_, i) => i + 1);
  return (
    <div
      style={{
        padding: "18px 36px 20px",
        maxWidth: "720px",
        margin: "0 auto",
        color: "var(--fg)",
      }}
    >
      {/* Stepper：N 步动态渲染，之间用短横线分隔。 */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: "5px",
          marginBottom: "14px",
          justifyContent: "center",
          flexWrap: "wrap",
        }}
      >
        {steps.map((s, i) => (
          <React.Fragment key={s}>
            <span style={stepperDot(currentStep >= s)}>{s}</span>
            {i < steps.length - 1 && (
              <span
                style={{
                  width: "22px",
                  height: "2px",
                  backgroundColor: currentStep >= s + 1 ? "var(--accent)" : "var(--border)",
                }}
              />
            )}
          </React.Fragment>
        ))}
      </div>

      <h1
        style={{
          fontSize: "20px",
          margin: 0,
          marginBottom: subtitle ? "3px" : "10px",
        }}
      >
        {title}
      </h1>
      {subtitle && (
        <p
          style={{
            color: "var(--muted)",
            margin: 0,
            marginBottom: "12px",
            fontSize: "12.5px",
            lineHeight: 1.5,
          }}
        >
          {subtitle}
        </p>
      )}

      <div style={{ marginBottom: "16px" }}>{children}</div>

      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          gap: "10px",
        }}
      >
        <div>{secondaryAction && renderButton({ variant: "ghost", ...secondaryAction })}</div>
        <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
          {skipAction && renderButton({ variant: "ghost", ...skipAction })}
          {primaryAction && renderButton({ variant: "primary", ...primaryAction })}
        </div>
      </div>

      {footerNote && (
        <p
          style={{
            marginTop: "12px",
            marginBottom: 0,
            fontSize: "11.5px",
            color: "var(--subtle)",
            textAlign: "center",
            lineHeight: 1.5,
          }}
        >
          {footerNote}
        </p>
      )}
    </div>
  );
};

export default OnboardingShell;
