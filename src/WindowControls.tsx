import { useMemo } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { Minus, Square, X } from "lucide-react";
import type { Dict } from "./i18n";

// Windows/Linux 无边框窗口的自绘最小化/最大化/关闭按钮（实现思路与 Jan 的
// WindowControls 一致：调用 Tauri 窗口 API，由 Rust 侧决定关闭语义）。
// macOS 使用原生红绿灯，不渲染本组件。
export function WindowControls({ t }: { t: Dict }) {
  const appWindow = useMemo(() => getCurrentWebviewWindow(), []);

  return (
    <div className="window-controls">
      <button
        type="button"
        className="wc-btn"
        aria-label={t.minimizeAria}
        title={t.minimizeAria}
        onClick={() => void appWindow.minimize()}
      >
        <Minus size={14} />
      </button>
      <button
        type="button"
        className="wc-btn"
        aria-label={t.maximizeAria}
        title={t.maximizeAria}
        onClick={() => void appWindow.toggleMaximize()}
      >
        <Square size={10} />
      </button>
      <button
        type="button"
        className="wc-btn wc-close"
        aria-label={t.closeAria}
        title={t.closeAria}
        onClick={() => void appWindow.close()}
      >
        <X size={14} />
      </button>
    </div>
  );
}
