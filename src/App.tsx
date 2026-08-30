import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { confirm as confirmDialog, open } from "@tauri-apps/plugin-dialog";
import {
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
}

interface ManagedStatus {
  managedRoot: string;
  dshPath: string;
  nodeVersion: string;
  dshVersion: string;
}

interface ManagedProgress {
  phase: string;
  message: string;
  percent: number | null;
}

// 与后端 ServiceStatus 默认值一致；展示时经 translateBackendMessage 按当前语言渲染
const emptyStatus: ServiceStatus = { phase: "stopped", pid: null, url: null, message: "服务未运行" };

function errorMessage(error: unknown) {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
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
  const [status, setStatus] = useState<ServiceStatus>(emptyStatus);
  const [embeddedWebviewOpen, setEmbeddedWebviewOpen] = useState(false);
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [managed, setManaged] = useState<ManagedStatus | null>(null);
  const [latestDsh, setLatestDsh] = useState<string | null>(null);
  const [managedProgress, setManagedProgress] = useState<ManagedProgress | null>(null);
  const [managedBusy, setManagedBusy] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const logEnd = useRef<HTMLDivElement>(null);

  const phaseLabels: Record<Phase, string> = {
    stopped: t.phaseStopped,
    starting: t.phaseStarting,
    running: t.phaseRunning,
    stopping: t.phaseStopping,
    failed: t.phaseFailed,
    external: t.phaseExternal,
  };

  useEffect(() => {
    void invoke<Bootstrap>("bootstrap")
      .then((data) => {
        setAppVersion(data.appVersion);
        setConfig({ ...data.config, dshPath: data.config.dshPath || data.detectedDsh || "" });
        setVersion(data.dshVersion);
        setProfiles(data.profiles);
        setStatus(data.status);
        if (data.config.managedRuntimeDir) {
          void invoke<ManagedStatus>("managed_runtime_status", { root: data.config.managedRuntimeDir })
            .then(setManaged)
            .catch(() => undefined);
          void invoke<string>("check_latest_dsh").then(setLatestDsh).catch(() => undefined);
        }
      })
      .catch((reason) => setError(errorMessage(reason)));
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
    const syncRuntimeState = () => {
      void invoke<ServiceStatus>("service_status").then(setStatus).catch(() => undefined);
      void invoke<boolean>("embedded_webview_open").then(setEmbeddedWebviewOpen).catch(() => undefined);
    };
    syncRuntimeState();
    const timer = window.setInterval(syncRuntimeState, 1500);

    return () => {
      clearInterval(timer);
      void statusListener.then((unlisten) => unlisten());
      void logListener.then((unlisten) => unlisten());
      void managedListener.then((unlisten) => unlisten());
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

  async function detectDsh() {
    setError(null);
    try {
      const [path, detectedVersion] = await invoke<[string, string]>("detect_dsh");
      patch("dshPath", path);
      setVersion(detectedVersion);
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
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function installManaged() {
    const selected = await open({ multiple: false, directory: true, title: t.selectInstallDirectory });
    if (!selected || Array.isArray(selected)) return;
    setManagedBusy(true);
    setManagedProgress({ phase: "starting", message: t.preparingInstall, percent: 0 });
    setError(null);
    try {
      const result = await invoke<ManagedStatus>("install_managed_runtime", { root: selected });
      setManaged(result);
      setVersion(result.dshVersion);
      setConfig((current) => current ? {
        ...current,
        managedRuntimeDir: result.managedRoot,
        dshPath: result.dshPath,
      } : current);
      void invoke<string>("check_latest_dsh").then(setLatestDsh).catch(() => undefined);
    } catch (reason) {
      setManagedProgress(null);
      setError(errorMessage(reason));
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

  async function openVersionPage(button: HTMLButtonElement) {
    button.blur();
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

  async function openServiceUrl(button: HTMLButtonElement) {
    button.blur();
    try {
      await invoke("open_service_url");
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function runCommand(command: "start_service" | "stop_service" | "restart_service") {
    if (!config) return;
    setBusy(true);
    setError(null);
    try {
      const payload = command === "stop_service" ? {} : { config };
      setStatus(await invoke<ServiceStatus>(command, payload));
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  if (!config) return <main className="loading">{t.loading}</main>;

  const shouldStart = status.phase === "stopped" || status.phase === "external" || (status.phase === "failed" && !status.pid);
  const webAvailable = status.phase === "running" || status.phase === "external";

  return (
    <main className="launcher-shell">
      <div className="launcher-content">
        <div className="control-row">
        <div className="left-workspace">
        <section className="compact-settings expanded">
          <div className="settings-body">
            <div className="mini-field wide">
              <div className="field-label-row"><label htmlFor="dsh-path">{t.dshCommand}</label><span>{version ? `DSH ${version}` : t.notVerified}</span></div>
              <div className="command-input">
                <input id="dsh-path" value={config.dshPath} disabled={locked} onChange={(event) => patch("dshPath", event.target.value)} />
                <button disabled={locked} onClick={() => void detectDsh()} title={t.autoDetect}><FileSearch size={14} /></button>
                <button disabled={locked} onClick={() => void chooseDsh()} title={t.chooseFile}><FolderOpen size={14} /></button>
              </div>
              <div className="runtime-tools">
                {managed ? (
                  <>
                    <span><PackageCheck size={12} />Node {managed.nodeVersion} · DSH {managed.dshVersion}{latestDsh && latestDsh !== managed.dshVersion ? ` → ${latestDsh}` : ""}</span>
                    <button disabled={managedBusy} onClick={() => void upgradeManagedDsh()}><Download size={12} />{t.upgradeDsh}</button>
                  </>
                ) : (
                  <>
                    <span>{t.managedRuntimeHint}</span>
                    <button disabled={managedBusy || locked} onClick={() => void installManaged()}><Download size={12} />{t.oneClickInstall}</button>
                  </>
                )}
              </div>
              {managedProgress && <div className="managed-progress"><span style={{ width: `${managedProgress.percent ?? 0}%` }} /><small>{managedProgress.message}</small></div>}
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
              <div className={`log-line merged ${line.level}`} key={line.firstIndex}><time>{line.timestamp}</time><span className="source">{line.sources.join("+")}</span><span>{line.message}{line.count > 1 && <em className="log-count">×{line.count}</em>}</span></div>
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
            className={`power-button ${status.phase}`}
            disabled={busy || status.phase === "starting" || status.phase === "stopping"}
            onClick={() => void runCommand(shouldStart ? "start_service" : "stop_service")}
            title={status.phase === "external" ? t.recheckWithConfig : status.phase === "running" ? t.stopService : t.startService}
          >
            <span className="power-ring"><Power size={45} strokeWidth={1.7} /></span>
          </button>

          <div className="launch-copy">
            <h1>{status.phase === "running" ? t.dshRunning : status.phase === "starting" ? t.dshStarting : status.phase === "stopping" ? t.dshStopping : t.dshStart}</h1>
            <p>{status.phase === "external" ? t.launcherNotStarted : translateBackendMessage(status.message, lang)}</p>
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
              onClick={(event) => void openVersionPage(event.currentTarget)}
            >
              v{appVersion}
              {releaseUpdate && <span className="update-dot" aria-hidden="true" />}
            </button>
          )}
        </section>
        </div>
      </div>
    </main>
  );
}