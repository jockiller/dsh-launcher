import { useEffect, useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { confirm as confirmDialog, open } from "@tauri-apps/plugin-dialog";
import {
  AppWindow,
  Ban,
  Copy,
  Download,
  ExternalLink,
  FileSearch,
  FolderOpen,
  PackageCheck,
  Power,
  RefreshCw,
  RotateCw,
  Settings,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import {
  detectLanguage,
  persistLanguage,
  translateBackendMessage,
  translations,
  type Dict,
  type Lang,
} from "./i18n";
import { mergeLogs, type LogLine } from "./logMerge";
import { WindowControls } from "./WindowControls";
import changelogMarkdown from "../CHANGELOG.md?raw";

type Phase = "stopped" | "starting" | "running" | "stopping" | "restarting" | "failed" | "external";
type Theme = "light" | "dark";

interface Config {
  dshPath: string;
  profile: string;
  host: string;
  port: number;
  customArgs: string;
  autoStart: boolean;
  showTrayIcon: boolean;
  autoScrollLogs: boolean;
  managedRuntimeDir: string;
}

interface ServiceStatus {
  phase: Phase;
  pid: number | null;
  url: string | null;
  message: string;
}

interface Bootstrap {
  appVersion: string;
  platform: string;
  config: Config;
  detectedDsh: string | null;
  dshVersion: string | null;
  profiles: string[];
  status: ServiceStatus;
}

interface ReleaseUpdate {
  latestVersion: string | null;
  releaseUrl: string | null;
  updateAvailable: boolean;
  notes: string | null;
}

interface AppUpdateProgress {
  phase: "progress" | "finished";
  received: number;
  total: number | null;
}

interface ManagedStatus {
  managedRoot: string;
  dshPath: string;
  nodeVersion: string;
  dshVersion: string;
}

interface DshVersionInfo {
  currentVersion: string;
  currentNotes: string | null;
  latestVersion: string;
  latestNotes: string | null;
  updateAvailable: boolean;
}

interface ManagedProgress {
  phase: string;
  message: string;
  percent: number | null;
}

interface ProfilePlugin {
  name: string;
  version: string;
}

// 与后端 ServiceStatus 默认值一致；展示时经 translateBackendMessage 按当前语言渲染
const emptyStatus: ServiceStatus = { phase: "stopped", pid: null, url: null, message: "服务未运行" };
const STARTUP_OVERLAY_MIN_MS = 700;
const STARTUP_OVERLAY_MAX_MS = 12_000;

function errorMessage(error: unknown) {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}

function escapeRegExp(value: string) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** 从 CHANGELOG.md 取出指定版本的更新说明；找不到或为空时返回 null。 */
function changelogSection(changelog: string, version: string): string | null {
  const heading = new RegExp(`^##\\s+v?${escapeRegExp(version)}(?:\\s|$)`, "m");
  const match = heading.exec(changelog);
  if (!match) return null;
  const rest = changelog.slice(match.index + match[0].length);
  const nextHeading = rest.search(/^##\s+/m);
  const body = (nextHeading === -1 ? rest : rest.slice(0, nextHeading)).trim();
  return body || null;
}

function systemLanguageIsChinese(): boolean {
  return (navigator.languages.length ? navigator.languages : [navigator.language])
    .some((language) => language.toLowerCase().startsWith("zh"));
}

function focusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(
    container.querySelectorAll<HTMLElement>(
      'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    ),
  ).filter((element) => !element.hidden && element.getAttribute("aria-hidden") !== "true");
}

function managedProgressLabel(
  progress: ManagedProgress,
  operation: "install" | "upgrade" | null,
  t: Dict,
): string {
  switch (progress.phase) {
    case "starting":
      return operation === "upgrade" ? t.upgradingDsh : t.preparingInstall;
    case "metadata":
      return t.managedProgressMetadata;
    case "download":
      return t.managedProgressDownload;
    case "verify":
      return t.managedProgressVerify;
    case "extract":
      return t.managedProgressExtract;
    case "install":
      return t.managedProgressInstall;
    case "upgrade":
      return t.managedProgressUpgrade;
    case "complete":
      return operation === "upgrade" ? t.managedProgressUpgradeComplete : t.managedProgressComplete;
    default:
      return t.managedProgressUnknown;
  }
}

export default function App() {
  const [lang, setLang] = useState<Lang>(detectLanguage);
  const t = translations[lang];
  const [systemTheme, setSystemTheme] = useState<Theme>(() =>
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light",
  );
  const [themeOverride, setThemeOverride] = useState<Theme | null>(() => {
    const saved = localStorage.getItem("dsh-launcher-theme-preference");
    return saved === "light" || saved === "dark" ? saved : null;
  });
  const theme = themeOverride ?? systemTheme;
  const [platform, setPlatform] = useState<string | null>(null);
  const [config, setConfig] = useState<Config | null>(null);
  const [appVersion, setAppVersion] = useState("");
  const [releaseUpdate, setReleaseUpdate] = useState<ReleaseUpdate | null>(null);
  const [profiles, setProfiles] = useState<string[]>(["web"]);
  const [version, setVersion] = useState<string | null>(null);
  const [dshVersionInfo, setDshVersionInfo] = useState<DshVersionInfo | null>(null);
  const [dshVersionDialogOpen, setDshVersionDialogOpen] = useState(false);
  const [dshVersionChecking, setDshVersionChecking] = useState(false);
  const [dshVersionCheckError, setDshVersionCheckError] = useState<string | null>(null);
  const [externalDshCopyState, setExternalDshCopyState] = useState<"idle" | "copied" | "failed">("idle");
  const [status, setStatus] = useState<ServiceStatus>(emptyStatus);
  const [embeddedWebviewOpen, setEmbeddedWebviewOpen] = useState(false);
  // 内容页是否完成加载：未就绪时保持内容隐藏，避免启动/切换时的白屏闪烁
  const [contentPageReady, setContentPageReady] = useState(false);
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [managed, setManaged] = useState<ManagedStatus | null>(null);
  const [latestDsh, setLatestDsh] = useState<string | null>(null);
  const [managedProgress, setManagedProgress] = useState<ManagedProgress | null>(null);
  const [managedOperation, setManagedOperation] = useState<"install" | "upgrade" | null>(null);
  const [managedBusy, setManagedBusy] = useState(false);
  const [installDialogOpen, setInstallDialogOpen] = useState(false);
  const [installDirectory, setInstallDirectory] = useState("");
  const [installDialogError, setInstallDialogError] = useState<string | null>(null);
  const [useMirror, setUseMirror] = useState(systemLanguageIsChinese);
  const [busy, setBusy] = useState(false);
  const [pendingAction, setPendingAction] = useState<"start" | "stop" | "restart" | "force_stop" | null>(null);
  const [startupReady, setStartupReady] = useState(false);
  const [minStartupElapsed, setMinStartupElapsed] = useState(false);
  const [externalStopOffered, setExternalStopOffered] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [appUpdateDialogOpen, setAppUpdateDialogOpen] = useState(false);
  const [appUpdateBusy, setAppUpdateBusy] = useState(false);
  const [appUpdateInstalled, setAppUpdateInstalled] = useState(false);
  const [appUpdateProgress, setAppUpdateProgress] = useState<AppUpdateProgress | null>(null);
  const [appUpdateError, setAppUpdateError] = useState<string | null>(null);
  const [appVersionChecking, setAppVersionChecking] = useState(false);
  const [appVersionCheckError, setAppVersionCheckError] = useState<string | null>(null);
  const [contentTitle, setContentTitle] = useState<string>("");
  // 标题栏弹层：错误详情 / 设置面板
  const [errorPanelOpen, setErrorPanelOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 插件管理
  const [pluginsDialogOpen, setPluginsDialogOpen] = useState(false);
  const [plugins, setPlugins] = useState<ProfilePlugin[]>([]);
  const [pluginsLoading, setPluginsLoading] = useState(false);
  const [pluginsError, setPluginsError] = useState<string | null>(null);
  const [uninstallingName, setUninstallingName] = useState<string | null>(null);
  const [cleanBusy, setCleanBusy] = useState(false);
  const logEnd = useRef<HTMLDivElement>(null);
  const pageContentRef = useRef<HTMLDivElement>(null);
  const installDialogRef = useRef<HTMLElement>(null);
  const appUpdateDialogRef = useRef<HTMLElement>(null);
  const dshVersionDialogRef = useRef<HTMLElement>(null);
  const pluginsDialogRef = useRef<HTMLElement>(null);
  const installDialogTriggerRef = useRef<HTMLElement | null>(null);
  const appUpdateDialogTriggerRef = useRef<HTMLElement | null>(null);
  const dshVersionTriggerRef = useRef<HTMLElement | null>(null);
  const pluginsTriggerRef = useRef<HTMLElement | null>(null);
  const previousDialogRef = useRef<"install" | "update" | "dsh-version" | "plugins" | null>(null);
  const versionCheckSeq = useRef(0);
  const appVersionCheckSeq = useRef(0);

  const isMac = platform === "macos";

  const phaseLabels: Record<Phase, string> = {
    stopped: t.phaseStopped,
    starting: t.phaseStarting,
    running: t.phaseRunning,
    stopping: t.phaseStopping,
    restarting: t.phaseRestarting,
    failed: t.phaseFailed,
    external: t.phaseExternal,
  };

  useEffect(() => {
    const minimumTimer = window.setTimeout(() => setMinStartupElapsed(true), STARTUP_OVERLAY_MIN_MS);
    void invoke<Bootstrap>("bootstrap")
      .then((data) => {
        setPlatform(data.platform);
        setAppVersion(data.appVersion);
        setConfig({ ...data.config, dshPath: data.config.dshPath || data.detectedDsh || "" });
        setVersion(data.dshVersion);
        setDshVersionInfo(null);
        setProfiles(data.profiles);
        setStatus(data.status);
        if (data.status.phase === "external") {
          setExternalStopOffered(true);
        }
        if (data.config.managedRuntimeDir) {
          // Managed runtimes keep their existing update check; external runtimes use the separate async check below.
          void invoke<ManagedStatus>("managed_runtime_status", { root: data.config.managedRuntimeDir })
            .then(setManaged)
            .catch(() => undefined);
          void invoke<string>("check_latest_dsh", { root: data.config.managedRuntimeDir })
            .then(setLatestDsh)
            .catch(() => undefined);
        }
      })
      .catch((reason) => {
        setError(errorMessage(reason));
        setStartupReady(true);
      });
    void invoke<ReleaseUpdate | null>("check_launcher_update")
      .then((update) => {
        if (update) setReleaseUpdate(update);
      })
      .catch(() => undefined);

    const statusListener = listen<ServiceStatus>("service-status", ({ payload }) => {
      setStatus(payload);
      if (payload.phase === "external") {
        setExternalStopOffered(true);
      }
    });
    const logListener = listen<LogLine>("service-log", ({ payload }) => {
      setLogs((current) => [...current.slice(-9998), payload]);
    });
    const managedListener = listen<ManagedProgress>("managed-progress", ({ payload }) => {
      setManagedProgress(payload);
    });
    const appUpdateListener = listen<AppUpdateProgress>("app-update-progress", ({ payload }) => {
      setAppUpdateProgress(payload);
    });
    const contentViewListener = listen<boolean>("content-webview-changed", ({ payload }) => {
      setEmbeddedWebviewOpen(payload);
    });
    // DSH 内容页的主题侦测上报：以 DSH 实际主题为准，标题栏跟随（持久化覆盖值）
    const contentThemeListener = listen<string>("content-theme-changed", ({ payload }) => {
      setThemeOverride(payload === "dark" ? "dark" : "light");
    });
    // 内容页加载进度：Started/Finished
    const contentPageLoadListener = listen<boolean>("content-page-load", ({ payload }) => {
      setContentPageReady(payload);
    });
    // DSH 内容页真实 document.title 上报
    const contentTitleListener = listen<string>("content-title-changed", ({ payload }) => {
      setContentTitle(payload);
    });
    const syncRuntimeState = () => {
      void invoke<ServiceStatus>("service_status").then(setStatus).catch(() => undefined);
      void invoke<boolean>("embedded_webview_open").then(setEmbeddedWebviewOpen).catch(() => undefined);
    };
    syncRuntimeState();
    const timer = window.setInterval(syncRuntimeState, 1500);

    return () => {
      clearTimeout(minimumTimer);
      clearInterval(timer);
      void statusListener.then((unlisten) => unlisten());
      void logListener.then((unlisten) => unlisten());
      void managedListener.then((unlisten) => unlisten());
      void appUpdateListener.then((unlisten) => unlisten());
      void contentViewListener.then((unlisten) => unlisten());
      void contentThemeListener.then((unlisten) => unlisten());
      void contentPageLoadListener.then((unlisten) => unlisten());
      void contentTitleListener.then((unlisten) => unlisten());
    };
  }, []);

  useEffect(() => {
    if (["running", "stopped", "failed", "external"].includes(status.phase)) {
      setPendingAction(null);
    }
  }, [status.phase]);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const syncSystemTheme = () => setSystemTheme(media.matches ? "dark" : "light");
    syncSystemTheme();
    media.addEventListener("change", syncSystemTheme);
    return () => media.removeEventListener("change", syncSystemTheme);
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
  }, [theme]);

  useEffect(() => {
    if (themeOverride) localStorage.setItem("dsh-launcher-theme-preference", themeOverride);
    else localStorage.removeItem("dsh-launcher-theme-preference");
    localStorage.removeItem("dsh-launcher-theme");
  }, [themeOverride]);

  useEffect(() => {
    document.documentElement.lang = lang;
    persistLanguage(lang);
  }, [lang]);

  useEffect(() => {
    if (!config) return;
    const timer = window.setTimeout(() => {
      void invoke("save_config", { config }).catch(() => undefined);
    }, 300);
    return () => clearTimeout(timer);
  }, [config]);

  // 标题栏层的任何交互（顶栏快速菜单/错误详情/设置面板/日志抽屉/模态框/过渡遮罩）
  // 期间隐藏 DSH 内容 WebView（隐藏而非销毁，页面状态保留），保证弹层在应用窗口上
  // 真实可见；全部关闭后恢复。此外内容页未加载完成（Started→Finished 之间）也保持
  // 隐藏，配合过渡遮罩消除启动/切换时的两次闪烁。
  const isTransitioning = busy || pendingAction !== null || ["starting", "stopping", "restarting"].includes(status.phase);
  // 服务已运行但内容页尚未加载完成：保持遮罩直到揭示瞬间，避免中间空档
  const contentPendingReveal = status.phase === "running" && embeddedWebviewOpen && !contentPageReady;
  const launcherLayerActive = errorPanelOpen || settingsOpen
    || installDialogOpen || dshVersionDialogOpen || appUpdateDialogOpen || pluginsDialogOpen || isTransitioning
    || contentPendingReveal;
  useEffect(() => {
    void invoke("set_content_hidden", { hidden: launcherLayerActive }).catch(() => undefined);
  }, [launcherLayerActive, embeddedWebviewOpen]);

  // 内容关闭时重置就绪状态；加载事件异常时 8 秒兜底揭示
  useEffect(() => {
    if (!embeddedWebviewOpen) setContentPageReady(false);
  }, [embeddedWebviewOpen]);
  useEffect(() => {
    if (!embeddedWebviewOpen || contentPageReady) return;
    const timer = window.setTimeout(() => setContentPageReady(true), 8000);
    return () => clearTimeout(timer);
  }, [embeddedWebviewOpen, contentPageReady]);

  // 主题与 DSH 内容保持一致：把当前生效主题（用户切换或跟随系统）推送给内容页
  useEffect(() => {
    if (!embeddedWebviewOpen) return;
    void invoke("set_content_theme", { theme }).catch(() => undefined);
  }, [theme, embeddedWebviewOpen]);

  useEffect(() => {
    if (config?.autoScrollLogs) logEnd.current?.scrollIntoView({ block: "end" });
  }, [logs, config?.autoScrollLogs]);

  useEffect(() => {
    const activeDialog = installDialogOpen
      ? "install"
      : appUpdateDialogOpen
        ? "update"
        : dshVersionDialogOpen
          ? "dsh-version"
          : pluginsDialogOpen
            ? "plugins"
            : null;
    const dialogRef = activeDialog === "install"
      ? installDialogRef
      : activeDialog === "update"
        ? appUpdateDialogRef
        : activeDialog === "dsh-version"
          ? dshVersionDialogRef
          : pluginsDialogRef;
    const dialog = dialogRef.current;
    const pageContent = pageContentRef.current;

    if (!activeDialog || !dialog) {
      const trigger = previousDialogRef.current === "install"
        ? installDialogTriggerRef.current
        : previousDialogRef.current === "update"
          ? appUpdateDialogTriggerRef.current
          : previousDialogRef.current === "dsh-version"
            ? dshVersionTriggerRef.current
            : previousDialogRef.current === "plugins"
              ? pluginsTriggerRef.current
              : null;
      previousDialogRef.current = null;
      trigger?.focus();
      return;
    }

    previousDialogRef.current = activeDialog;
    pageContent?.setAttribute("inert", "");
    pageContent?.setAttribute("aria-hidden", "true");

    const focusDialog = () => {
      const first = focusableElements(dialog)[0];
      (first ?? dialog).focus();
    };
    const frame = window.requestAnimationFrame(focusDialog);
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        if (activeDialog === "install") setInstallDialogOpen(false);
        else if (activeDialog === "update") setAppUpdateDialogOpen(false);
        else if (activeDialog === "dsh-version") setDshVersionDialogOpen(false);
        else setPluginsDialogOpen(false);
        return;
      }
      if (event.key !== "Tab") return;

      const focusable = focusableElements(dialog);
      if (focusable.length === 0) {
        event.preventDefault();
        dialog.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const current = document.activeElement;
      if (event.shiftKey && (current === first || !dialog.contains(current))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (current === last || !dialog.contains(current))) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown, true);
    return () => {
      window.cancelAnimationFrame(frame);
      document.removeEventListener("keydown", handleKeyDown, true);
      pageContent?.removeAttribute("inert");
      pageContent?.removeAttribute("aria-hidden");
    };
  }, [installDialogOpen, appUpdateDialogOpen, dshVersionDialogOpen, pluginsDialogOpen]);

  // 弹层（错误/设置）的 Esc 关闭；对话框打开时交给上面对话框逻辑处理
  useEffect(() => {
    const anyDialogOpen = installDialogOpen || appUpdateDialogOpen || dshVersionDialogOpen || pluginsDialogOpen;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || anyDialogOpen) return;
      if (errorPanelOpen) setErrorPanelOpen(false);
      else if (settingsOpen) setSettingsOpen(false);
    };
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [errorPanelOpen, settingsOpen, installDialogOpen, appUpdateDialogOpen, dshVersionDialogOpen, pluginsDialogOpen]);

  useEffect(() => {
    if (status.phase !== "external") setExternalStopOffered(false);
  }, [status.phase]);

  useEffect(() => {
    if (!config || !minStartupElapsed) return;
    const waitingForAutoStart = config.autoStart && status.phase === "starting";
    if (!waitingForAutoStart) {
      setStartupReady(true);
      return;
    }
    const timeout = window.setTimeout(() => setStartupReady(true), STARTUP_OVERLAY_MAX_MS);
    return () => clearTimeout(timeout);
  }, [config, minStartupElapsed, status.phase]);

  // 强制关闭入口提供恢复路径：Esc 退回普通 external 状态（再次点击启动即为重新检测）
  useEffect(() => {
    if (!externalStopOffered) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setExternalStopOffered(false);
    };
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [externalStopOffered]);

  const locked = ["running", "starting", "stopping", "restarting"].includes(status.phase);
  // 合并展示是派生视图：仅在日志列表变化时重算，logs 原始 state 不变
  const mergedLogs = useMemo(() => mergeLogs(logs), [logs]);
  const currentAppNotes = useMemo(
    () => (appVersion ? changelogSection(changelogMarkdown, appVersion) : null),
    [appVersion],
  );

  function patch<K extends keyof Config>(key: K, value: Config[K]) {
    setConfig((current) => (current ? { ...current, [key]: value } : current));
  }

  function toggleLang() {
    setLang((current) => (current === "zh" ? "en" : "zh"));
  }

  // 点击 DSH 版本号：打开版本弹窗并调用 check_dsh_version 检查最新版本与更新说明
  async function checkDshVersion(button: HTMLButtonElement) {
    if (!version) return;
    button.blur();
    dshVersionTriggerRef.current = button;
    setExternalDshCopyState("idle");
    setDshVersionCheckError(null);
    setDshVersionInfo(null);
    setDshVersionChecking(true);
    setDshVersionDialogOpen(true);
    const requestSeq = ++versionCheckSeq.current;
    try {
      const info = await invoke<DshVersionInfo>("check_dsh_version", { currentVersion: version });
      if (versionCheckSeq.current === requestSeq) setDshVersionInfo(info);
    } catch (reason) {
      if (versionCheckSeq.current === requestSeq) setDshVersionCheckError(errorMessage(reason));
    } finally {
      if (versionCheckSeq.current === requestSeq) setDshVersionChecking(false);
    }
  }

  async function detectDsh() {
    setError(null);
    try {
      const [path, detectedVersion] = await invoke<[string, string]>("detect_dsh");
      patch("dshPath", path);
      setVersion(detectedVersion);
      setDshVersionInfo(null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function chooseDsh() {
    const selected = await open({ multiple: false, directory: false, title: t.selectDshDialogTitle });
    if (!selected) return;
    try {
      const detectedVersion = await invoke<string>("validate_dsh", { path: selected });
      patch("dshPath", selected);
      setVersion(detectedVersion);
      setDshVersionInfo(null);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function openProfileDirectory() {
    try {
      await invoke("open_profile_dir", { profile: config?.profile || "web" });
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function chooseInstallDirectory() {
    try {
      const selected = await open({ multiple: false, directory: true, title: t.selectInstallDirectory });
      if (selected && !Array.isArray(selected)) {
        setInstallDirectory(selected);
        setInstallDialogError(null);
      }
    } catch (reason) {
      setInstallDialogError(errorMessage(reason));
    }
  }

  async function installManaged() {
    if (!installDirectory) return;
    setInstallDialogError(null);
    setInstallDialogOpen(false);
    setManagedBusy(true);
    setManagedOperation("install");
    setManagedProgress({ phase: "starting", message: t.preparingInstall, percent: 0 });
    setError(null);
    try {
      const result = await invoke<ManagedStatus>("install_managed_runtime", {
        root: installDirectory,
        useMirror,
      });
      setManaged(result);
      setVersion(result.dshVersion);
      setDshVersionInfo(null);
      setConfig((current) => current ? {
        ...current,
        managedRuntimeDir: result.managedRoot,
        dshPath: result.dshPath,
      } : current);
      void invoke<string>("check_latest_dsh", { root: result.managedRoot }).then(setLatestDsh).catch(() => undefined);
    } catch (reason) {
      const message = errorMessage(reason);
      setManagedProgress(null);
      setError(message);
      setInstallDialogError(message);
      setInstallDialogOpen(true);
    } finally {
      setManagedBusy(false);
    }
  }

  async function upgradeManagedDsh() {
    if (!managed) return;
    const confirmed = await confirmDialog(t.upgradeConfirmMessage, {
      title: t.upgradeConfirmTitle,
      kind: "warning",
      okLabel: t.upgradeConfirmAction,
      cancelLabel: t.cancel,
    });
    if (!confirmed) return;
    setManagedBusy(true);
    setManagedOperation("upgrade");
    setManagedProgress({ phase: "upgrade", message: t.upgradingDsh, percent: 0 });
    setError(null);
    try {
      const result = await invoke<ManagedStatus>("upgrade_managed_dsh", { root: managed.managedRoot });
      setManaged(result);
      setVersion(result.dshVersion);
      setLatestDsh(result.dshVersion);
    } catch (reason) {
      setManagedProgress(null);
      setError(errorMessage(reason));
    } finally {
      setManagedBusy(false);
    }
  }

  async function openReleaseUrl() {
    try {
      if (releaseUpdate?.releaseUrl) {
        await invoke("open_release_page", { url: releaseUpdate.releaseUrl });
      } else {
        await invoke("open_project_page");
      }
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function checkLauncherVersion(force: boolean) {
    const requestSeq = ++appVersionCheckSeq.current;
    setAppVersionChecking(true);
    setAppVersionCheckError(null);
    try {
      const update = await invoke<ReleaseUpdate | null>("check_launcher_update", { force });
      if (appVersionCheckSeq.current !== requestSeq) return;
      if (update) {
        setReleaseUpdate(update);
      } else if (force) {
        setAppVersionCheckError(t.appVersionCheckFailed);
      }
    } catch (reason) {
      if (appVersionCheckSeq.current !== requestSeq) return;
      setAppVersionCheckError(errorMessage(reason));
    } finally {
      if (appVersionCheckSeq.current === requestSeq) setAppVersionChecking(false);
    }
  }

  // 点击版本号始终打开版本弹窗：展示当前版本更新内容，并可跳转 GitHub / 检查更新。
  function handleAppVersionClick(button: HTMLButtonElement) {
    button.blur();
    appUpdateDialogTriggerRef.current = button;
    setAppUpdateError(null);
    setAppUpdateInstalled(false);
    setAppUpdateProgress(null);
    setAppVersionCheckError(null);
    setAppUpdateDialogOpen(true);
  }

  // 应用内更新：调用 Rust 侧安装（内部会先停托管 DSH、校验签名并汇报进度）。
  // 任何失败都在对话框内展示，并提供 GitHub 手动下载的兜底路径。
  async function runAppUpdate() {
    if (!releaseUpdate) return;
    setAppUpdateBusy(true);
    setAppUpdateError(null);
    setAppUpdateProgress(null);
    try {
      await invoke("app_update_install");
      setAppUpdateInstalled(true);
      // 立即重启：确认由对话框按钮完成，这里只负责把安装状态交给 restart 流程
      setAppUpdateDialogOpen(true);
    } catch (reason) {
      setAppUpdateError(errorMessage(reason));
    } finally {
      setAppUpdateBusy(false);
    }
  }

  async function restartApp() {
    setAppUpdateBusy(true);
    try {
      await invoke("app_update_restart");
    } catch (reason) {
      setAppUpdateError(errorMessage(reason));
      setAppUpdateBusy(false);
    }
  }

  async function openServiceUrl(button: HTMLButtonElement) {
    button.blur();
    try {
      await invoke("open_service_url");
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function openEmbeddedView(button: HTMLButtonElement) {
    button.blur();
    // 不论何时点击“打开”：彻底收起设置面板和所有弹层，切换到 webview
    setSettingsOpen(false);
    setPluginsDialogOpen(false);
    setErrorPanelOpen(false);
    if (!appUpdateBusy) setAppUpdateDialogOpen(false);
    try {
      await invoke("set_content_hidden", { hidden: false });
      await invoke("open_embedded_view");
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function openDshGitHub() {
    try {
      await invoke("open_dsh_github_page");
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  // ---------- 插件管理与 Profile 清理 ----------

  async function refreshPlugins() {
    setPluginsLoading(true);
    try {
      const list = await invoke<ProfilePlugin[]>("list_profile_plugins", { profile: config?.profile || "web" });
      setPlugins(list);
    } catch (reason) {
      setPluginsError(errorMessage(reason));
    } finally {
      setPluginsLoading(false);
    }
  }

  async function openPluginsDialog(button: HTMLButtonElement) {
    button.blur();
    pluginsTriggerRef.current = button;
    setPluginsError(null);
    setPluginsDialogOpen(true);
    await refreshPlugins();
  }

  // 卸载前原生确认：防止误删；卸载过程按插件粒度展示忙碌状态
  async function uninstallPlugin(name: string) {
    const confirmed = await confirmDialog(t.uninstallConfirmMessage.replace("{0}", name), {
      title: t.uninstallConfirmTitle,
      kind: "warning",
      okLabel: t.uninstallAction,
      cancelLabel: t.cancel,
    });
    if (!confirmed) return;
    setUninstallingName(name);
    try {
      await invoke("uninstall_profile_plugin", { profile: config?.profile || "web", name });
      setPlugins((list) => list.filter((plugin) => plugin.name !== name));
    } catch (reason) {
      setPluginsError(errorMessage(reason));
    } finally {
      setUninstallingName(null);
    }
  }

  async function runCleanLockfile() {
    setCleanBusy(true);
    // 自动打开设置面板，让命令输出在其中的日志区即时可见
    setSettingsOpen(true);
    try {
      await invoke("run_profile_clean", { profile: config?.profile || "web" });
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setCleanBusy(false);
    }
  }

  async function copyExternalDshCommand() {
    if (!dshVersionInfo?.updateAvailable) return;
    const command = `npm install -g @deepseek-ai/dsh@${dshVersionInfo.latestVersion}`;
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable");
      await navigator.clipboard.writeText(command);
      setExternalDshCopyState("copied");
    } catch {
      setExternalDshCopyState("failed");
    }
  }

  // 强制停止占用当前端口的外部 DSH 进程（Rust 侧会做 DSH 身份校验后再终止）
  async function forceStopExternal(button: HTMLButtonElement) {
    button.blur();
    if (!config) return;
    const confirmed = await confirmDialog(t.forceStopConfirmMessage, {
      title: t.forceStopConfirmTitle,
      kind: "warning",
      okLabel: t.forceStopAction,
      cancelLabel: t.cancel,
    });
    if (!confirmed) return;
    flushSync(() => {
      setBusy(true);
      setPendingAction("force_stop");
      setStatus((prev) => ({ ...prev, phase: "stopping", message: "正在停止外部服务..." }));
      setError(null);
    });
    await new Promise((resolve) => requestAnimationFrame(() => setTimeout(resolve, 50)));
    try {
      const [nextStatus] = await Promise.all([
        invoke<ServiceStatus>("force_stop_external_service", { config }),
        new Promise((resolve) => setTimeout(resolve, 500)),
      ]);
      setStatus(nextStatus);
      setExternalStopOffered(false);
    } catch (reason) {
      setError(errorMessage(reason));
      setPendingAction(null);
    } finally {
      setBusy(false);
      setPendingAction(null);
    }
  }

  async function runCommand(command: "start_service" | "stop_service" | "restart_service") {
    if (!config) return;
    const action = command === "start_service" ? "start" : command === "stop_service" ? "stop" : "restart";
    flushSync(() => {
      setBusy(true);
      setPendingAction(action);
      if (command === "start_service") {
        setStatus((prev) => ({ ...prev, phase: "starting", message: "正在启动 DSH..." }));
      } else if (command === "stop_service") {
        setStatus((prev) => ({ ...prev, phase: "stopping", message: "正在停止 DSH..." }));
      } else if (command === "restart_service") {
        setStatus((prev) => ({ ...prev, phase: "restarting", message: "正在重启 DSH..." }));
      }
      setError(null);
    });
    // 确保浏览器在发起后端 IPC 前，已经将遮罩绘制到屏幕上
    await new Promise((resolve) => requestAnimationFrame(() => setTimeout(resolve, 50)));
    try {
      const payload = command === "stop_service" ? {} : { config };
      const minDelay = command === "stop_service" ? 500 : 0;
      const [nextStatus] = await Promise.all([
        invoke<ServiceStatus>(command, payload),
        minDelay ? new Promise((resolve) => setTimeout(resolve, minDelay)) : Promise.resolve(),
      ]);
      setStatus(nextStatus);
      setExternalStopOffered(command === "start_service" && nextStatus.phase === "external");
    } catch (reason) {
      setError(errorMessage(reason));
      setPendingAction(null);
    } finally {
      setBusy(false);
      if (command === "stop_service") {
        setPendingAction(null);
      }
    }
  }

  if (!config || !startupReady) {
    const message = config?.autoStart ? t.autoStarting : t.preparingEnvironment;
    return (
      <main className="loading" aria-busy="true" aria-live="polite">
        <div className="loading-copy">
          <span className="loading-spinner" aria-hidden="true" />
          <span>{message}</span>
        </div>
      </main>
    );
  }

  const shouldStart = status.phase === "stopped" || status.phase === "external" || (status.phase === "failed" && !status.pid);
  const webAvailable = status.phase === "running" || status.phase === "external";

  let transitionTitle = t.dshStarting;
  if (contentPendingReveal) {
    transitionTitle = t.loadingWebview;
  } else if (pendingAction === "restart" || status.phase === "restarting") {
    transitionTitle = pendingAction === "restart" ? t.dshRestartAction : t.dshRestarting;
  } else if (pendingAction === "stop" || status.phase === "stopping") {
    transitionTitle = t.dshStopping;
  } else if (pendingAction === "force_stop") {
    transitionTitle = t.forceStoppingAction;
  } else if (pendingAction === "start" || status.phase === "starting") {
    transitionTitle = t.dshStarting;
  } else if (busy && !shouldStart) {
    transitionTitle = t.dshStopping;
  }

  const transitionDetail = translateBackendMessage(status.message, lang);
  const powerTitle = externalStopOffered
    ? t.forceStopConfirmTitle
    : status.phase === "external"
      ? t.recheckWithConfig
      : ["running", "starting", "restarting"].includes(status.phase)
        ? t.stopService
        : t.startService;

  return (
    <main className="app-shell">
      <div className="shell-root" ref={pageContentRef}>
        <header className={`titlebar ${isMac ? "is-mac" : ""}`} data-tauri-drag-region="deep">
          {isMac && <div className="traffic-light-spacer" aria-hidden="true" />}
          <button
            type="button"
            className={`tb-status ${status.phase}`}
            onClick={() => {
              if (status.phase === "running") {
                // 与右上角设置按钮保持一致：在主窗控制台与 webview 之间来回切换
                setSettingsOpen((prev) => !prev);
                setErrorPanelOpen(false);
              } else {
                setSettingsOpen(true);
                setErrorPanelOpen(false);
              }
            }}
            title={status.message ? translateBackendMessage(status.message, lang) : undefined}
          >
            <span className="tb-dot" aria-hidden="true" />
            {phaseLabels[status.phase]}
            {status.pid != null && <small>PID {status.pid}</small>}
          </button>
          {error && (
            <button
              type="button"
              className={`tb-btn tb-error-btn ${errorPanelOpen ? "active" : ""}`}
              onClick={() => {
                setErrorPanelOpen((openState) => !openState);
              }}
              title={t.errorBellTitle}
            >
              <TriangleAlert size={15} />
            </button>
          )}
          <div className="tb-title" data-tauri-drag-region="deep">
            {contentTitle || "DSH Launcher"}
          </div>
          <div className="tb-spacer" />
          <div className="tb-actions">
            <button
              type="button"
              className={`tb-btn tb-power ${externalStopOffered ? "force-stop" : status.phase}`}
              disabled={busy || status.phase === "stopping"}
              onClick={(event) => {
                if (externalStopOffered) void forceStopExternal(event.currentTarget);
                else void runCommand(shouldStart ? "start_service" : "stop_service");
              }}
              title={powerTitle}
            >
              {externalStopOffered ? <Ban size={15} /> : <Power size={15} />}
            </button>
            <button
              type="button"
              className={`tb-btn ${settingsOpen ? "active" : ""}`}
              onClick={() => {
                setSettingsOpen((openState) => !openState);
              }}
              title={t.settingsTitle}
            >
              <Settings size={14} />
            </button>
            {appVersion && (
              <button
                type="button"
                className={`tb-btn tb-version${releaseUpdate?.updateAvailable ? " has-update" : ""}`}
                title={releaseUpdate?.updateAvailable && releaseUpdate.latestVersion ? t.launcherUpdateAvailable.replace("{0}", releaseUpdate.latestVersion) : t.appVersionClickTitle}
                onClick={(event) => handleAppVersionClick(event.currentTarget)}
              >
                v{appVersion}
                {releaseUpdate?.updateAvailable && <span className="update-dot" aria-hidden="true" />}
              </button>
            )}
            {!isMac && <WindowControls t={t} />}
          </div>
        </header>

        {errorPanelOpen && error && (
          <div className="top-strip error-strip" role="alert" data-tauri-drag-region="false">
            <p>{translateBackendMessage(error, lang)}</p>
            <div className="strip-actions">
              <button
                type="button"
                onClick={() => {
                  setError(null);
                  setErrorPanelOpen(false);
                }}
              >
                {t.errorDismiss}
              </button>
            </div>
          </div>
        )}

        <div className="launcher-shell">
          <div className="launcher-content">
            <div className="console-top">
              <section className="console-settings">
                {error && <div className="error-banner">{translateBackendMessage(error, lang)}</div>}
                    <div className="mini-field wide">
                      <div className="field-label-row">
                        <label htmlFor="dsh-path">{t.dshCommand}</label>
                        {version ? (
                          <button
                            type="button"
                            className="dsh-version-update"
                            title={dshVersionInfo?.updateAvailable ? t.dshVersionUpdateAvailable.replace("{0}", dshVersionInfo.latestVersion) : t.dshVersionCheckTitle}
                            disabled={dshVersionChecking}
                            onClick={(event) => void checkDshVersion(event.currentTarget)}
                          >
                            DSH {version}{dshVersionInfo?.updateAvailable && <span className="update-dot" aria-hidden="true" />}
                          </button>
                        ) : <span>{t.notVerified}</span>}
                      </div>
                      <div className="command-input">
                        <input id="dsh-path" value={config.dshPath} disabled={locked} onChange={(event) => patch("dshPath", event.target.value)} />
                        <button disabled={locked} onClick={() => void detectDsh()} title={t.autoDetect}><FileSearch size={14} /></button>
                        <button disabled={locked} onClick={() => void chooseDsh()} title={t.chooseFile}><FolderOpen size={14} /></button>
                      </div>
                      <div className="runtime-tools">
                        {managed ? (
                          <>
                            <span><PackageCheck size={12} />Node {managed.nodeVersion} · DSH {managed.dshVersion}{latestDsh && latestDsh !== managed.dshVersion ? ` → ${latestDsh}` : ""}</span>
                            {(!latestDsh || latestDsh !== managed.dshVersion) && (
                              <button disabled={managedBusy} onClick={() => void upgradeManagedDsh()}><Download size={12} />{t.upgradeDsh}</button>
                            )}
                          </>
                        ) : (
                          <>
                            <span>{t.managedRuntimeHint}</span>
                            <button disabled={managedBusy || locked} onClick={(event) => { installDialogTriggerRef.current = event.currentTarget; setInstallDialogOpen(true); }}><Download size={12} />{t.oneClickInstall}</button>
                          </>
                        )}
                      </div>
                      {managedProgress && <div className="managed-progress" aria-live="polite"><span style={{ width: `${managedProgress.percent ?? 0}%` }} /><small>{managedProgressLabel(managedProgress, managedOperation, t)}</small></div>}
                    </div>

                    <div className="connection-row">
                      <div className="mini-field">
                        <div className="field-label-row">
                          <label htmlFor="profile">{t.profile}</label>
                          <button
                            type="button"
                            className="profile-dir-btn"
                            onClick={() => void openProfileDirectory()}
                            title={t.openProfileDir}
                          >
                            <FolderOpen size={10} />
                          </button>
                        </div>
                        <input id="profile" list="profile-list" value={config.profile} disabled={locked} onChange={(event) => patch("profile", event.target.value)} />
                        <datalist id="profile-list">{profiles.map((profile) => <option key={profile} value={profile} />)}</datalist>
                      </div>
                      <div className="mini-field"><label htmlFor="host">{t.host}</label><input id="host" value={config.host} disabled={locked} onChange={(event) => patch("host", event.target.value)} /></div>
                      <div className="mini-field port"><label htmlFor="port">{t.port}</label><input id="port" type="text" inputMode="numeric" pattern="[0-9]*" value={config.port} disabled={locked} onChange={(event) => patch("port", Number(event.target.value.replace(/\D/g, "").slice(0, 5)))} /></div>
                    </div>

                    <div className="post-launch-row">
                      <div className="mini-field custom-args"><label htmlFor="custom-args">{t.dshArgs}</label><input id="custom-args" value={config.customArgs} disabled={locked} onChange={(event) => patch("customArgs", event.target.value)} placeholder={t.customArgsPlaceholder} /></div>
                      <label className="mini-toggle"><input type="checkbox" checked={config.autoStart} disabled={locked} onChange={(event) => patch("autoStart", event.target.checked)} /><span>{t.autoStart}</span></label>
                      <label className="mini-toggle"><input type="checkbox" checked={config.showTrayIcon} onChange={(event) => patch("showTrayIcon", event.target.checked)} /><span>{t.showTrayIcon}</span></label>
                    </div>

                    <div className="settings-tools-row">
                      <button type="button" onClick={(event) => void openPluginsDialog(event.currentTarget)}>
                        <PackageCheck size={12} />{t.managePlugins}
                      </button>
                      <button type="button" disabled={cleanBusy} onClick={() => void runCleanLockfile()} title="pnpm clean --lockfile && pnpm install">
                        <Trash2 size={12} />{t.runCleanLockfile}
                      </button>
                    </div>

                    <div className="settings-lang-row">
                      <span>{t.languageLabel}</span>
                      <button type="button" className="lang-switch" onClick={toggleLang} title={t.langToggleTitle}>
                        {lang === "zh" ? "English" : "中文"}
                      </button>
                    </div>
              </section>

              <section className={`console-launch ${status.phase}`}>
                <div className={`compact-status ${status.phase}`}>
                  <span aria-hidden="true" />
                  {phaseLabels[status.phase]}
                  {status.pid && <small>PID {status.pid}</small>}
                </div>
                <button
                  className={`power-button ${externalStopOffered ? "force-stop" : status.phase}`}
                  disabled={busy || status.phase === "stopping"}
                  onClick={(event) => externalStopOffered
                    ? void forceStopExternal(event.currentTarget)
                    : void runCommand(shouldStart ? "start_service" : "stop_service")}
                  title={powerTitle}
                >
                  <span className="power-ring">{externalStopOffered ? <Ban size={45} strokeWidth={1.7} /> : <Power size={45} strokeWidth={1.7} />}</span>
                </button>

                <div className="launch-copy">
                  <h1>{externalStopOffered ? t.forceStopConfirmTitle : status.phase === "running" ? t.dshRunning : status.phase === "starting" ? t.dshStarting : status.phase === "stopping" ? t.dshStopping : status.phase === "restarting" ? t.dshRestarting : t.dshStart}</h1>
                  <p>{translateBackendMessage(status.message, lang)}</p>
                  {status.phase === "running" && status.url && <p className="stage-url">{status.url}</p>}
                </div>

                <div className="quick-actions">
                  <button disabled={status.phase !== "running" || busy} onClick={() => void runCommand("restart_service")}><RotateCw size={14} />{t.restartService}</button>
                  <button disabled={!webAvailable} onClick={(event) => void openServiceUrl(event.currentTarget)}><ExternalLink size={14} />{t.openInBrowser}</button>
                  <button disabled={!webAvailable} onClick={(event) => void openEmbeddedView(event.currentTarget)}><AppWindow size={14} />{t.openEmbedded}</button>
                </div>
              </section>
            </div>

            <section className="console-logs" aria-label={t.serviceLogs}>
                  <header>
                    <div><strong>{t.serviceLogs}</strong><span>{t.logLineCount.replace("{0}", String(mergedLogs.length))}</span></div>
                    <div className="log-tools">
                      <label className="log-autoscroll">
                        <input type="checkbox" checked={config.autoScrollLogs} onChange={(event) => patch("autoScrollLogs", event.target.checked)} />
                        {t.autoScroll}
                      </label>
                      <button onClick={() => setLogs([])} title={t.clearLogs}><Trash2 size={13} /></button>
                    </div>
                  </header>
                  <div className="mini-console">
                    {logs.length === 0 ? <div className="console-empty">{t.logEmpty}</div> : mergedLogs.map((line) => (
                      <div className={`log-line merged ${line.level}`} key={line.firstIndex}><time>{line.timestamp}</time><span className="source">{line.sources.join("+")}</span><span>{line.sources.includes("installer") ? translateBackendMessage(line.message, lang) : line.message}{line.count > 1 && <em className="log-count">×{line.count}</em>}</span></div>
                    ))}
                    <div ref={logEnd} />
                  </div>
                </section>
          </div>
        </div>
      </div>
      {installDialogOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setInstallDialogOpen(false)}>
          <section className="install-dialog" role="dialog" aria-modal="true" aria-labelledby="install-dialog-title" tabIndex={-1} ref={installDialogRef} onMouseDown={(event) => event.stopPropagation()}>
            <header><h2 id="install-dialog-title">{t.installDialogTitle}</h2></header>
            <div className="install-dialog-body">
              <label htmlFor="install-directory">{t.installDirectory}</label>
              <div className="command-input">
                <input id="install-directory" value={installDirectory} readOnly placeholder={t.installDirectoryPlaceholder} />
                <button type="button" onClick={() => void chooseInstallDirectory()} title={t.chooseDirectory}><FolderOpen size={14} /></button>
              </div>
              {installDialogError && <p className="install-dialog-error" role="alert">{translateBackendMessage(installDialogError, lang)}</p>}
              <label className="mirror-option">
                <input type="checkbox" checked={useMirror} onChange={(event) => setUseMirror(event.target.checked)} />
                <span><strong>{t.useMirror}</strong><small>{t.useMirrorDescription}</small></span>
              </label>
            </div>
            <footer>
              <button type="button" onClick={() => setInstallDialogOpen(false)}>{t.cancel}</button>
              <button type="button" className="primary" disabled={!installDirectory} onClick={() => void installManaged()}><Download size={13} />{t.installNow}</button>
            </footer>
          </section>
        </div>
      )}
      {dshVersionDialogOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setDshVersionDialogOpen(false)}>
          <section className="install-dialog" role="dialog" aria-modal="true" aria-labelledby="dsh-version-title" tabIndex={-1} ref={dshVersionDialogRef} onMouseDown={(event) => event.stopPropagation()}>
            <header><h2 id="dsh-version-title">{t.dshVersionDialogTitle}</h2></header>
            <div className="install-dialog-body">
              {dshVersionChecking && <p className="dsh-update-warning" role="status">{t.dshVersionChecking}</p>}
              {!dshVersionChecking && dshVersionCheckError && (
                <p className="install-dialog-error" role="alert">{translateBackendMessage(dshVersionCheckError, lang)}</p>
              )}
              {!dshVersionChecking && !dshVersionCheckError && dshVersionInfo && (
                <>
                  <p className="dsh-version-line"><strong>{t.dshVersionCurrent}</strong> v{dshVersionInfo.currentVersion}</p>
                  <p className="dsh-version-line"><strong>{t.dshVersionLatest}</strong> v{dshVersionInfo.latestVersion}</p>
                  <label>{t.dshCurrentNotesLabel}</label>
                  <div className="app-update-notes">{dshVersionInfo.currentNotes || t.dshUpdateNotesEmpty}</div>
                  {dshVersionInfo.updateAvailable ? (
                    <>
                      <label>{t.dshLatestNotesLabel}</label>
                      <div className="app-update-notes">{dshVersionInfo.latestNotes || t.dshUpdateNotesEmpty}</div>
                      <label htmlFor="dsh-update-command">{t.dshUpdateCommandLabel}</label>
                      <div className="command-input update-command-input">
                        <input id="dsh-update-command" readOnly value={`npm install -g @deepseek-ai/dsh@${dshVersionInfo.latestVersion}`} />
                        <button type="button" onClick={() => void copyExternalDshCommand()} title={t.dshUpdateCopyCommand} aria-label={t.dshUpdateCopyCommand}>
                          <Copy size={14} />
                        </button>
                      </div>
                      <p className="dsh-update-warning" role="note">{t.dshUpdateWarning}</p>
                      {externalDshCopyState === "copied" && <p className="app-update-success" role="status">{t.dshUpdateCopied}</p>}
                      {externalDshCopyState === "failed" && <p className="install-dialog-error" role="alert">{t.dshUpdateCopyFailed}</p>}
                    </>
                  ) : (
                    <p className="app-update-success" role="status">{t.dshVersionUpToDate}</p>
                  )}
                </>
              )}
            </div>
            <footer>
              <button type="button" onClick={() => void openDshGitHub()}><ExternalLink size={13} />{t.openDshGitHub}</button>
              <button type="button" onClick={() => setDshVersionDialogOpen(false)}>{t.cancel}</button>
            </footer>
          </section>
        </div>
      )}
      {appUpdateDialogOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setAppUpdateDialogOpen(false)}>
          <section className="install-dialog" role="dialog" aria-modal="true" aria-labelledby="app-update-title" tabIndex={-1} ref={appUpdateDialogRef} onMouseDown={(event) => event.stopPropagation()}>
            <header>
              <h2 id="app-update-title">
                {releaseUpdate?.updateAvailable
                  ? t.appUpdateTitle.replace("{0}", releaseUpdate.latestVersion ?? "")
                  : t.appVersionDialogTitle}
              </h2>
            </header>
            <div className="install-dialog-body">
              <p className="dsh-version-line"><strong>{t.dshVersionCurrent}</strong> v{appVersion}</p>
              {releaseUpdate?.latestVersion && (
                <p className="dsh-version-line"><strong>{t.dshVersionLatest}</strong> v{releaseUpdate.latestVersion}</p>
              )}
              <label>{t.appVersionCurrentNotesLabel}</label>
              <div className="app-update-notes">{currentAppNotes || t.appVersionChangelogEmpty}</div>
              {releaseUpdate?.updateAvailable && (
                <>
                  <label>{t.appVersionLatestNotesLabel}</label>
                  <div className="app-update-notes">
                    {navigator.platform.startsWith("Linux") && <p className="window-close-hint">{t.appUpdateLinuxHint}</p>}
                    {releaseUpdate.notes || t.appUpdateNotesEmpty}
                  </div>
                </>
              )}
              {appVersionChecking && <p className="dsh-update-warning" role="status">{t.appVersionChecking}</p>}
              {!appVersionChecking && appVersionCheckError && (
                <p className="install-dialog-error" role="alert">{translateBackendMessage(appVersionCheckError, lang)}</p>
              )}
              {!appVersionChecking && !appVersionCheckError && releaseUpdate && !releaseUpdate.updateAvailable && (
                <p className="app-update-success" role="status">{t.appVersionUpToDate}</p>
              )}
              {appUpdateProgress && !appUpdateInstalled && (
                <div className="managed-progress" aria-live="polite">
                  <span style={{ width: appUpdateProgress.total ? `${Math.min(100, Math.round((appUpdateProgress.received / appUpdateProgress.total) * 100))}%` : "50%" }} />
                  <small>
                    {appUpdateProgress.total
                      ? `${t.appUpdateInstallRunning} ${Math.round((appUpdateProgress.received / appUpdateProgress.total) * 100)}%`
                      : t.appUpdateInstallRunning}
                  </small>
                </div>
              )}
              {appUpdateInstalled && <p className="app-update-success" role="status">{t.appUpdateInstalled}</p>}
              {appUpdateError && (
                <>
                  <p className="install-dialog-error" role="alert">{translateBackendMessage(appUpdateError, lang)}</p>
                  <p className="window-close-hint">{t.appUpdateFailed}</p>
                </>
              )}
            </div>
            <footer>
              <button type="button" disabled={appUpdateBusy} onClick={() => void openReleaseUrl()}>
                <ExternalLink size={13} />{t.openGitHub}
              </button>
              <button
                type="button"
                disabled={appVersionChecking || appUpdateBusy}
                onClick={() => void checkLauncherVersion(true)}
              >
                <RefreshCw size={13} />{appVersionChecking ? t.appVersionChecking : t.checkForUpdates}
              </button>
              {appUpdateInstalled ? (
                <button type="button" className="primary" disabled={appUpdateBusy} onClick={() => void restartApp()}><RotateCw size={13} />{t.appUpdateRestartNow}</button>
              ) : releaseUpdate?.updateAvailable && !navigator.platform.startsWith("Linux") ? (
                <button type="button" className="primary" disabled={appUpdateBusy} onClick={() => void runAppUpdate()}>
                  <Download size={13} />{appUpdateBusy && !appUpdateProgress ? t.appUpdateLoading : appUpdateProgress ? t.appUpdateInstallRunning : t.appUpdateInstallAction}
                </button>
              ) : null}
            </footer>
          </section>
        </div>
      )}
      {pluginsDialogOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setPluginsDialogOpen(false)}>
          <section className="install-dialog plugins-dialog" role="dialog" aria-modal="true" aria-labelledby="plugins-dialog-title" tabIndex={-1} ref={pluginsDialogRef} onMouseDown={(event) => event.stopPropagation()}>
            <header><h2 id="plugins-dialog-title">{t.pluginsDialogTitle}</h2></header>
            <div className="install-dialog-body">
              <p className="plugins-hint">{t.pluginsDialogHint}</p>
              {pluginsLoading && <p className="dsh-update-warning" role="status">{t.pluginsLoading}</p>}
              {pluginsError && <p className="install-dialog-error" role="alert">{translateBackendMessage(pluginsError, lang)}</p>}
              {!pluginsLoading && !pluginsError && plugins.length === 0 && (
                <p className="app-update-success" role="status">{t.pluginsEmpty}</p>
              )}
              <div className="plugins-list">
                {plugins.map((plugin) => (
                  <div className="plugin-row" key={plugin.name}>
                    <div className="plugin-meta">
                      <strong>{plugin.name}</strong>
                      <small>{plugin.version}</small>
                    </div>
                    <button
                      type="button"
                      className="plugin-uninstall"
                      disabled={uninstallingName !== null}
                      onClick={() => void uninstallPlugin(plugin.name)}
                    >
                      {uninstallingName === plugin.name ? t.uninstalling : t.uninstall}
                    </button>
                  </div>
                ))}
              </div>
            </div>
            <footer>
              <button type="button" disabled={pluginsLoading} onClick={() => void refreshPlugins()}>
                <RefreshCw size={13} />{t.pluginsRefresh}
              </button>
              <button type="button" onClick={() => setPluginsDialogOpen(false)}>{t.cancel}</button>
            </footer>
          </section>
        </div>
      )}
      {(isTransitioning || contentPendingReveal) && (
        <div className="action-overlay-backdrop" role="alert" aria-busy="true" aria-live="polite">
          <section className="action-overlay-card" role="dialog" aria-modal="true" aria-label={transitionTitle}>
            <span className="loading-spinner" aria-hidden="true" />
            <div className="action-overlay-text">
              <strong>{transitionTitle}</strong>
              {transitionDetail && <p>{transitionDetail}</p>}
            </div>
            {(status.phase === "starting" || pendingAction === "start" || status.phase === "restarting" || pendingAction === "restart") && (
              <button
                type="button"
                className="action-overlay-cancel"
                disabled={busy && pendingAction === "stop"}
                onClick={() => void runCommand("stop_service")}
              >
                {t.stopService}
              </button>
            )}
          </section>
        </div>
      )}
    </main>
  );
}
