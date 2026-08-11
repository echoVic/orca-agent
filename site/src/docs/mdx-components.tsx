import { useState, type ReactNode } from "react";

type CalloutType = "info" | "warning" | "danger" | "success";

const calloutIcons: Record<CalloutType, string> = {
  info: "ⓘ",
  warning: "⚠",
  danger: "⛔",
  success: "✓",
};

const calloutColors: Record<CalloutType, { border: string; bg: string; icon: string }> = {
  info: { border: "var(--accent)", bg: "var(--surface)", icon: "var(--accent)" },
  warning: { border: "#d97706", bg: "rgba(217,119,6,0.08)", icon: "#d97706" },
  danger: { border: "#dc2626", bg: "rgba(220,38,38,0.08)", icon: "#dc2626" },
  success: { border: "#16a34a", bg: "rgba(22,163,74,0.08)", icon: "#16a34a" },
};

export function Callout({
  type = "info",
  title,
  children,
}: {
  type?: CalloutType;
  title?: string;
  children: ReactNode;
}) {
  const c = calloutColors[type];
  return (
    <div
      className="mdx-callout"
      style={{ borderColor: c.border, background: c.bg }}
    >
      <span className="mdx-callout-icon" style={{ color: c.icon }}>
        {calloutIcons[type]}
      </span>
      <div className="mdx-callout-body">
        {title ? <strong className="mdx-callout-title">{title}</strong> : null}
        <div className="mdx-callout-content">{children}</div>
      </div>
    </div>
  );
}

export function Steps({ children }: { children: ReactNode }) {
  return <ol className="mdx-steps">{children}</ol>;
}

export function Step({ title, children }: { title?: string; children: ReactNode }) {
  return (
    <li className="mdx-step">
      {title ? <div className="mdx-step-title">{title}</div> : null}
      <div className="mdx-step-content">{children}</div>
    </li>
  );
}

type TabDef = { label: string; children: ReactNode };

export function Tabs({ children }: { children: ReactNode }) {
  const tabs = (Array.isArray(children) ? children : [children]) as React.ReactElement[];
  const [active, setActive] = useState(0);

  const labels = tabs.map((t, i) => {
    const props = (t.props || {}) as { label?: string; value?: string };
    return props.label || props.value || `Tab ${i + 1}`;
  });

  return (
    <div className="mdx-tabs">
      <div className="mdx-tabs-list" role="tablist">
        {labels.map((label, i) => (
          <button
            key={i}
            role="tab"
            aria-selected={active === i}
            className={"mdx-tabs-tab" + (active === i ? " is-active" : "")}
            onClick={() => setActive(i)}
          >
            {label}
          </button>
        ))}
      </div>
      <div className="mdx-tabs-panel" role="tabpanel">{tabs[active]}</div>
    </div>
  );
}

export function Tab({ label, children }: { label: string; children: ReactNode }) {
  return <>{children}</>;
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="mdx-kbd">{children}</kbd>;
}

export const mdxComponents = {
  Callout,
  Steps,
  Step,
  Tabs,
  Tab,
  Kbd,
};

export type MdxComponents = typeof mdxComponents;
