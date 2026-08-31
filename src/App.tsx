import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { confirm as confirmDialog, open } from "@tauri-apps/plugin-dialog";
import {
  Ban,
  Copy,
  Download,
  ExternalLink,
  FileSearch,
  FolderOpen,
  Moon,
  PackageCheck,
  Power,
  RotateCw,
  Sun,
  Trash2,
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

type Phase = "stopped" | "starting" | "running" | "stopping" | "failed" | "external";
type LaunchAction = "none" | "default_browser" | "embedded_webview";
type Theme = "light" | "dark";

interface Config {
  dshPath: string;
  profile: string;
  host: string;
  port: number;
  launchAction: LaunchAction;
  customArgs: string;
  autoStart: boolean;
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

// 与后端 ServiceStatus 默认值一致；展示时经 translateBackendMessage 按当前语言渲染
const emptyStatus: ServiceStatus = { phase: "stopped", pid: null, url: null, message: "服务未运行" };
const STARTUP_OVERLAY_MIN_MS = 700;
const STARTUP_OVERLAY_MAX_MS = 12_000;

function errorMessage(error: unknown) {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
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
  const [startupReady, setStartupReady] = useState(false);
  const [minStartupElapsed, setMinStartupElapsed] = useState(false);
  const [externalStopOffered, setExternalStopOffered] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [appUpdateDialogOpen, setAppUpdateDialogOpen] = useState(false);
  const [appUpdateBusy, setAppUpdateBusy] = useState(false);
  const [appUpdateInstalled, setAppUpdateInstalled] = useState(false);
  const [appUpdateProgress, setAppUpdateProgress] = useState<AppUpdateProgress | null>(null);
  const [appUpdateError, setAppUpdateError] = useState<string | null>(null);
  const logEnd = useRef<HTMLDivElement>(null);
  const pageContentRef = useRef<HTMLDivElement>(null);
  const installDialogRef = useRef<HTMLElement>(null);
  const appUpdateDialogRef = useRef<HTMLElement>(null);
  const dshVersionDialogRef = useRef<HTMLElement>(null);
  const installDialogTriggerRef = useRef<HTMLElement | null>(null);
  const appUpdateDialogTriggerRef = useRef<HTMLElement | null>(null);
  const dshVersionTriggerRef = useRef<HTMLElement | null>(null);
  const previousDialogRef = useRef<"install" | "update" | "dsh-version" | null>(null);
  const versionCheckSeq = useRef(0);

  const phaseLabels: Record<Phase, string> = {
    stopped: t.phaseStopped,
    starting: t.phaseStarting,
    running: t.phaseRunning,
    stopping: t.phaseStopping,
    failed: t.phaseFailed,
    external: t.phaseExternal,
  };

  useEffect(() => {
    const minimumTimer = window.setTimeout(() => setMinStartupElapsed(true), STARTUP_OVERLAY_MIN_MS);
    void invoke<Bootstrap>("bootstrap")
      .then((data) => {
        setAppVersion(data.appVersion);
        setConfig({ ...data.config, dshPath: data.config.dshPath || data.detectedDsh || "" });
        setVersion(data.dshVersion);
        setDshVersionInfo(null);
        setProfiles(data.profiles);
        setStatus(data.status);
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
      .then((update) => setReleaseUpdate(update?.updateAvailable ? update : null))
      .catch(() => undefined);

    const statusListener = listen<ServiceStatus>("service-status", ({ payload }) => setStatus(payload));
    const logListener = listen<LogLine>("service-log", ({ payload }) => {
      setLogs((current) => [...current.slice(-9998), payload]);
    });
    const managedListener = listen<ManagedProgress>("managed-progress", ({ payload }) => {
      setManagedProgress(payload);
    });
    const appUpdateListener = listen<AppUpdateProgress>("app-update-progress", ({ payload }) => {
      setAppUpdateProgress(payload);
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
    };
  }, []);

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
          : null;
    const dialogRef = activeDialog === "install"
      ? installDialogRef
      : activeDialog === "update"
        ? appUpdateDialogRef
        : dshVersionDialogRef;
    const dialog = dialogRef.current;
    const pageContent = pageContentRef.current;

    if (!activeDialog || !dialog) {
      const trigger = previousDialogRef.current === "install"
        ? installDialogTriggerRef.current
        : previousDialogRef.current === "update"
          ? appUpdateDialogTriggerRef.current
          : previousDialogRef.current === "dsh-version"
            ? dshVersionTriggerRef.current
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
        else setDshVersionDialogOpen(false);
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
  }, [installDialogOpen, appUpdateDialogOpen, dshVersionDialogOpen]);

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

  const locked = ["running", "starting", "stopping"].includes(status.phase);
  const serviceUrl = useMemo(
    () => status.url || (config ? `http://${config.host}:${config.port}` : ""),
    [config, status.url],
  );
  // 合并展示是派生视图：仅在日志列表变化时重算，logs 原始 state 不变
  const mergedLogs = useMemo(() => mergeLogs(logs), [logs]);

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

  async function openVersionPage(button: HTMLButtonElement) {
    button.blur();
    await openReleaseUrl();
  }

  // 版本按钮点击：有新版本时打开"更新日志"对话框，由用户决定是否应用内更新；
  // 无新版本时保持原有行为（打开 Release 列表页）。
  function handleAppVersionClick(button: HTMLButtonElement) {
    button.blur();
    if (releaseUpdate?.updateAvailable) {
      appUpdateDialogTriggerRef.current = button;
      setAppUpdateError(null);
      setAppUpdateInstalled(false);
      setAppUpdateProgress(null);
      setAppUpdateDialogOpen(true);
      return;
    }
    void openVersionPage(button);
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

  async function openDshGitHub() {
    try {
      await invoke("open_dsh_github_page");
    } catch (reason) {
      setError(errorMessage(reason));
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
    setBusy(true);
    setError(null);
    try {
      setStatus(await invoke<ServiceStatus>("force_stop_external_service", { config }));
      setExternalStopOffered(false);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function runCommand(command: "start_service" | "stop_service" | "restart_service") {
    if (!config) return;
    setBusy(true);
    setError(null);
    try {
      const payload = command === "stop_service" ? {} : { config };
      const nextStatus = await invoke<ServiceStatus>(command, payload);
      setStatus(nextStatus);
      setExternalStopOffered(command === "start_service" && nextStatus.phase === "external");
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
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

  return (
    <main className="launcher-shell">
      <div className="launcher-content" ref={pageContentRef}>
        <div className="control-row">
        <div className="left-workspace">
        <section className="compact-settings expanded">
          <div className="settings-body">
            <div className="mini-field wide">
              <div className="field-label-row"><label htmlFor="dsh-path">{t.dshCommand}</label>{version ? (
                 <button
                   type="button"
                   className="dsh-version-update"
                   title={dshVersionInfo?.updateAvailable ? t.dshVersionUpdateAvailable.replace("{0}", dshVersionInfo.latestVersion) : t.dshVersionCheckTitle}
                   disabled={dshVersionChecking}
                   onClick={(event) => void checkDshVersion(event.currentTarget)}
                 >
                   DSH {version}{dshVersionInfo?.updateAvailable && <span className="update-dot" aria-hidden="true" />}
                 </button>
               ) : <span>{t.notVerified}</span>}</div>
              <div className="command-input">
                <input id="dsh-path" value={config.dshPath} disabled={locked} onChange={(event) => patch("dshPath", event.target.value)} />
                <button disabled={locked} onClick={() => void detectDsh()} title={t.autoDetect}><FileSearch size={14} /></button>
                <button disabled={locked} onClick={() => void chooseDsh()} title={t.chooseFile}><FolderOpen size={14} /></button>
              </div>
              <div className="runtime-tools">
                {managed ? (
                  <>
                    <span><PackageCheck size={12} />Node {managed.nodeVersion} · DSH {managed.dshVersion}{latestDsh && latestDsh !== managed.dshVersion ? ` → ${latestDsh}` : ""}</span>
                    {/* 仅在“确认有新版”（或版本未知）时显示升级按钮；已确认最新则隐藏 */}
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
              <div className="mini-field"><label htmlFor="profile">{t.profile}</label><input id="profile" list="profile-list" value={config.profile} disabled={locked} onChange={(event) => patch("profile", event.target.value)} /><datalist id="profile-list">{profiles.map((profile) => <option key={profile} value={profile} />)}</datalist></div>
              <div className="mini-field"><label htmlFor="host">{t.host}</label><input id="host" value={config.host} disabled={locked} onChange={(event) => patch("host", event.target.value)} /></div>
              <div className="mini-field port"><label htmlFor="port">{t.port}</label><input id="port" type="text" inputMode="numeric" pattern="[0-9]*" value={config.port} disabled={locked} onChange={(event) => patch("port", Number(event.target.value.replace(/\D/g, "").slice(0, 5)))} /></div>
            </div>

            <div className="post-launch-row">
              <div className="mini-field action"><label htmlFor="launch-action">{t.afterLaunch}</label><select id="launch-action" value={config.launchAction} disabled={locked} onChange={(event) => patch("launchAction", event.target.value as LaunchAction)}><option value="none">{t.launchNone}</option><option value="default_browser">{t.launchDefaultBrowser}</option><option value="embedded_webview">{t.launchEmbeddedWebview}</option></select></div>
              <div className="mini-field custom-args"><label htmlFor="custom-args">{t.dshArgs}</label><input id="custom-args" value={config.customArgs} disabled={locked} onChange={(event) => patch("customArgs", event.target.value)} placeholder={t.customArgsPlaceholder} /></div>
              <label className="mini-toggle"><input type="checkbox" checked={config.autoStart} disabled={locked} onChange={(event) => patch("autoStart", event.target.checked)} /><span>{t.autoStart}</span></label>
            </div>

          </div>
        </section>

        {error && <div className="error-banner launcher-error">{translateBackendMessage(error, lang)}</div>}

        <section className="log-window">
          <header>
            <div><strong>{t.serviceLogs}</strong><span>{t.logLineCount.replace("{0}", String(mergedLogs.length))}</span></div>
            <div className="log-tools">
              <label><input type="checkbox" checked={config.autoScrollLogs} onChange={(event) => patch("autoScrollLogs", event.target.checked)} />{t.autoScroll}</label>
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

        <section className={`launch-stage ${status.phase}`}>
          <button className="lang-toggle" onClick={toggleLang} title={t.langToggleTitle}>
            {lang === "zh" ? "EN" : "中"}
          </button>
          <button className="theme-toggle" onClick={() => setThemeOverride(theme === "light" ? "dark" : "light")} title={theme === "light" ? t.switchToDark : t.switchToLight}>
            {theme === "light" ? <Moon size={15} /> : <Sun size={15} />}
          </button>
          <div className={`compact-status ${status.phase}`}><span />{phaseLabels[status.phase]}{status.pid && <small>PID {status.pid}</small>}</div>
          <button
            className={`power-button ${externalStopOffered ? "force-stop" : status.phase}`}
            disabled={busy || status.phase === "starting" || status.phase === "stopping"}
            onClick={(event) => externalStopOffered
              ? void forceStopExternal(event.currentTarget)
              : void runCommand(shouldStart ? "start_service" : "stop_service")}
            title={externalStopOffered ? t.forceStopConfirmTitle : status.phase === "external" ? t.recheckWithConfig : status.phase === "running" ? t.stopService : t.startService}
          >
            <span className="power-ring">{externalStopOffered ? <Ban size={45} strokeWidth={1.7} /> : <Power size={45} strokeWidth={1.7} />}</span>
          </button>

          <div className="launch-copy">
            <h1>{externalStopOffered ? t.forceStopConfirmTitle : status.phase === "running" ? t.dshRunning : status.phase === "starting" ? t.dshStarting : status.phase === "stopping" ? t.dshStopping : t.dshStart}</h1>
            <p>{translateBackendMessage(status.message, lang)}</p>
            {status.phase === "running" && embeddedWebviewOpen && <p className="window-close-hint">{t.windowCloseHint}</p>}
          </div>

          <button className="launch-url" disabled={!webAvailable} onClick={(event) => void openServiceUrl(event.currentTarget)}>
            {serviceUrl}<ExternalLink size={13} />
          </button>

          <div className="quick-actions">
            <button disabled={status.phase !== "running" || busy} onClick={() => void runCommand("restart_service")}><RotateCw size={14} />{t.restartService}</button>
            <button disabled={!webAvailable} onClick={(event) => void openServiceUrl(event.currentTarget)}><ExternalLink size={14} />{t.openWebGui}</button>
          </div>

          {appVersion && (
            <button
              className={`app-version${releaseUpdate ? " has-update" : ""}`}
              title={releaseUpdate?.latestVersion ? t.launcherUpdateAvailable.replace("{0}", releaseUpdate.latestVersion) : t.viewOnGitHub}
              onClick={(event) => handleAppVersionClick(event.currentTarget)}
            >
              v{appVersion}
              {releaseUpdate && <span className="update-dot" aria-hidden="true" />}
            </button>
          )}
        </section>
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
      {appUpdateDialogOpen && releaseUpdate && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setAppUpdateDialogOpen(false)}>
          <section className="install-dialog" role="dialog" aria-modal="true" aria-labelledby="app-update-title" tabIndex={-1} ref={appUpdateDialogRef} onMouseDown={(event) => event.stopPropagation()}>
            <header><h2 id="app-update-title">{t.appUpdateTitle.replace("{0}", releaseUpdate.latestVersion ?? "")}</h2></header>
            <div className="install-dialog-body">
              <div className="app-update-notes">
                {/* Linux 不参与应用内热更新，仅提示手动下载 */}
                {navigator.platform.startsWith("Linux") && <p className="window-close-hint">{t.appUpdateLinuxHint}</p>}
                <div className="app-update-notes-content">
                  {releaseUpdate.notes ? releaseUpdate.notes : t.appUpdateNotesEmpty}
                </div>
              </div>
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
              <button type="button" onClick={() => setAppUpdateDialogOpen(false)}>{t.cancel}</button>
              {/* Linux 上"在 GitHub 查看"是唯一行动出口，升级为 primary 主按钮；隐藏热更按钮后保持行动指引清晰 */}
              {releaseUpdate.releaseUrl && !appUpdateInstalled && (
                <button
                  type="button"
                  className={`github-download${navigator.platform.startsWith("Linux") ? " primary" : ""}`}
                  disabled={appUpdateBusy}
                  onClick={() => void openReleaseUrl()}
                >
                  <ExternalLink size={13} />{t.viewOnGitHub}
                </button>
              )}
              {appUpdateInstalled ? (
                <button type="button" className="primary" disabled={appUpdateBusy} onClick={() => void restartApp()}><RotateCw size={13} />{t.appUpdateRestartNow}</button>
              ) : !navigator.platform.startsWith("Linux") && (
                <button type="button" className="primary" disabled={appUpdateBusy} onClick={() => void runAppUpdate()}>
                  <Download size={13} />{appUpdateBusy && !appUpdateProgress ? t.appUpdateLoading : appUpdateProgress ? t.appUpdateInstallRunning : t.appUpdateInstallAction}
                </button>
              )}
            </footer>
          </section>
        </div>
      )}
    </main>
  );
}
