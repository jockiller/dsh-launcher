// 界面中英文文案与后端已知消息翻译。
// 语言来源：localStorage 持久化值优先；未持久化时按 navigator 语言判断，zh 开头用中文，其余英文。
// 后端（Rust）返回的 status.message / 错误串为中文静态文案：中文界面原样展示，
// 英文界面按"已知消息映射"翻译，动态片段用模板占位符还原，未知消息原文返回。

export type Lang = "zh" | "en";

export const LANG_STORAGE_KEY = "dsh-launcher-lang";

const zhDict = {
  loading: "正在加载...",
  langToggleTitle: "切换语言 (English)",
  // 设置表单
  dshCommand: "DSH 命令",
  notVerified: "尚未验证",
  autoDetect: "自动检测",
  chooseFile: "选择文件",
  profile: "Profile",
  host: "主机",
  port: "端口",
  afterLaunch: "启动后",
  launchNone: "不执行",
  launchDefaultBrowser: "默认浏览器",
  launchEmbeddedWebview: "内置 WebView",
  dshArgs: "DSH 参数",
  customArgsPlaceholder: "例如 --no-open",
  autoStart: "自动启动",
  selectDshDialogTitle: "选择 dsh 可执行文件",
  selectInstallDirectory: "选择空目录安装 Node 与 DSH",
  managedRuntimeHint: "使用独立 Node LTS 安装 DSH",
  oneClickInstall: "一键安装",
  installDialogTitle: "安装 Node 与 DSH",
  installDirectory: "安装文件夹",
  installDirectoryPlaceholder: "请选择一个空文件夹",
  chooseDirectory: "选择文件夹",
  useMirror: "使用国内镜像源",
  useMirrorDescription: "加速 Node 与 DSH 下载，不修改系统 npm 配置；后续升级将沿用此选择。",
  installNow: "开始安装",
  upgradeDsh: "升级 DSH",
  preparingInstall: "正在准备安装...",
  upgradingDsh: "正在停止服务并升级 DSH...",
  upgradeConfirmTitle: "升级 DSH",
  upgradeConfirmMessage: "升级将停止当前 DSH 服务，完成后不会自动重启。是否继续？",
  upgradeConfirmAction: "停止并升级",
  cancel: "取消",
  // 日志窗口
  serviceLogs: "服务日志",
  logLineCount: "{0} 行",
  autoScroll: "自动滚动",
  clearLogs: "清空日志",
  logEmpty: "等待服务输出...",
  // 阶段与状态
  phaseStopped: "已停止",
  phaseStarting: "启动中",
  phaseRunning: "运行中",
  phaseStopping: "停止中",
  phaseFailed: "异常",
  phaseExternal: "外部服务",
  switchToDark: "切换到暗色",
  switchToLight: "切换到亮色",
  recheckWithConfig: "按当前配置重新检测",
  startService: "启动服务",
  stopService: "停止服务",
  dshRunning: "DSH 正在运行",
  dshStarting: "正在启动 DSH",
  dshStopping: "正在停止 DSH",
  dshStart: "启动 DSH",
  launcherNotStarted: "启动器服务尚未启动",
  windowCloseHint: "本窗口可以关闭",
  restartService: "重启服务",
  openWebGui: "打开 Web GUI",
  viewOnGitHub: "在 GitHub 上查看项目",
  launcherUpdateAvailable: "发现 Launcher v{0}，点击查看更新日志",
  appUpdateTitle: "发现新版本 v{0}",
  appUpdateNotesEmpty: "本次更新没有说明文本。",
  appUpdateInstallAction: "应用内更新并自动重启",
  appUpdateLinuxHint: "Linux 暂不支持应用内更新，请前往 GitHub 下载对应平台的安装包。",
  appUpdateInstallRunning: "正在下载并安装更新（含签名校验）...",
  appUpdateInstalled: "更新已安装完成！",
  appUpdateRestartNow: "立即重启",
  appUpdateFailed: "应用内更新失败，可改用 GitHub 手动下载。",
  appUpdateLoading: "正在获取更新信息...",
} as const;

export type Dict = Record<keyof typeof zhDict, string>;

const enDict: Dict = {
  loading: "Loading...",
  langToggleTitle: "Switch language (中文)",
  dshCommand: "DSH Command",
  notVerified: "Not verified",
  autoDetect: "Auto-detect",
  chooseFile: "Browse",
  profile: "Profile",
  host: "Host",
  port: "Port",
  afterLaunch: "After launch",
  launchNone: "Do nothing",
  launchDefaultBrowser: "Default browser",
  launchEmbeddedWebview: "Embedded WebView",
  dshArgs: "DSH arguments",
  customArgsPlaceholder: "e.g. --no-open",
  autoStart: "Auto start",
  selectDshDialogTitle: "Select dsh executable",
  selectInstallDirectory: "Select an empty directory for Node and DSH",
  managedRuntimeHint: "Install DSH with a dedicated Node LTS runtime",
  oneClickInstall: "Install",
  installDialogTitle: "Install Node and DSH",
  installDirectory: "Installation folder",
  installDirectoryPlaceholder: "Select an empty folder",
  chooseDirectory: "Choose folder",
  useMirror: "Use China mirror",
  useMirrorDescription: "Speeds up Node and DSH downloads without changing system npm settings. Future upgrades reuse this choice.",
  installNow: "Install now",
  upgradeDsh: "Upgrade DSH",
  preparingInstall: "Preparing installation...",
  upgradingDsh: "Stopping the service and upgrading DSH...",
  upgradeConfirmTitle: "Upgrade DSH",
  upgradeConfirmMessage: "The current DSH service will stop and will not restart automatically after the upgrade. Continue?",
  upgradeConfirmAction: "Stop and upgrade",
  cancel: "Cancel",
  serviceLogs: "Service Logs",
  logLineCount: "{0} lines",
  autoScroll: "Auto-scroll",
  clearLogs: "Clear logs",
  logEmpty: "Waiting for service output...",
  phaseStopped: "Stopped",
  phaseStarting: "Starting",
  phaseRunning: "Running",
  phaseStopping: "Stopping",
  phaseFailed: "Failed",
  phaseExternal: "External",
  switchToDark: "Switch to dark theme",
  switchToLight: "Switch to light theme",
  recheckWithConfig: "Re-detect with the current configuration",
  startService: "Start service",
  stopService: "Stop service",
  dshRunning: "DSH is running",
  dshStarting: "Starting DSH",
  dshStopping: "Stopping DSH",
  dshStart: "Start DSH",
  launcherNotStarted: "The launcher service is not started yet",
  windowCloseHint: "This window can be closed",
  restartService: "Restart",
  openWebGui: "Open Web GUI",
  viewOnGitHub: "View project on GitHub",
  launcherUpdateAvailable: "Launcher v{0} is available; click to view the update log",
  appUpdateTitle: "New version v{0} available",
  appUpdateNotesEmpty: "No release notes for this update.",
  appUpdateInstallAction: "Update in place and restart",
  appUpdateLinuxHint: "In-place updates are not available on Linux yet. Please download the installer for your platform from GitHub.",
  appUpdateInstallRunning: "Downloading and installing (with signature verification)...",
  appUpdateInstalled: "Update installed!",
  appUpdateRestartNow: "Restart now",
  appUpdateFailed: "In-place update failed; you can download manually from GitHub instead.",
  appUpdateLoading: "Fetching update info...",
};

export const translations: Record<Lang, Dict> = { zh: zhDict, en: enDict };

/** 把模板中的 "{0}" 占位符替换为按序参数。 */
export function format(template: string, ...params: Array<string | number>): string {
  return template.replace(/\{(\d+)\}/g, (match, index: string) => {
    const value = params[Number(index)];
    return value === undefined ? match : String(value);
  });
}

/** 推断初始语言：localStorage 优先，否则 navigator 语言 zh 开头用中文、其余英文。 */
export function detectLanguage(): Lang {
  let saved: string | null = null;
  try {
    saved = localStorage.getItem(LANG_STORAGE_KEY);
  } catch {
    // localStorage 不可用（隐私模式等）时退回浏览器语言判断
    saved = null;
  }
  if (saved === "zh" || saved === "en") return saved;
  const candidates =
    typeof navigator !== "undefined" && navigator.languages && navigator.languages.length > 0
      ? navigator.languages
      : [navigator.language];
  for (const candidate of candidates) {
    if (candidate && candidate.toLowerCase().startsWith("zh")) return "zh";
  }
  return "en";
}

/** 持久化语言偏好；失败时静默忽略（只影响下次启动的默认语言）。 */
export function persistLanguage(lang: Lang): void {
  try {
    localStorage.setItem(LANG_STORAGE_KEY, lang);
  } catch {
    // 忽略持久化失败
  }
}

// ---------- 后端已知消息映射（service.rs / lib.rs / config.rs 的中文静态文案） ----------

/** 精确匹配：后端完整中文原文 → 英文。 */
const backendExact: Record<string, string> = {
  服务未运行: "Service not running",
  "检测到端口上已有 Web 服务，启动器不会接管":
    "A web service is already running on this port; the launcher will not take it over",
  "正在启动 DSH...": "Starting DSH...",
  "正在等待健康检查...": "Waiting for health check...",
  "DSH 服务运行中": "DSH service is running",
  "正在停止 DSH...": "Stopping DSH...",
  "DSH 启动超时，请停止服务后检查日志":
    "DSH startup timed out; stop the service and check the logs",
  健康检查通过: "Health check passed",
  "30 秒内未通过健康检查": "Health check did not pass within 30 seconds",
  "DSH 服务已由启动器运行": "DSH is already running under the launcher",
  "未找到可执行的 dsh，请手动指定路径": "No executable dsh found; please set the path manually",
  "未找到 dsh，请手动指定可执行文件": "dsh not found; please specify the executable manually",
  "执行 dsh --version 超时": "dsh --version timed out",
  "主机或端口格式无效": "Invalid host or port format",
  "Profile 不能为空": "Profile cannot be empty",
  "主机不能为空": "Host cannot be empty",
  "端口必须在 1 到 65535 之间": "Port must be between 1 and 65535",
  "DSH 服务尚未运行": "DSH service is not running yet",
  打开浏览器失败: "Failed to open the browser",
  替换配置文件失败: "Failed to replace the config file",
};

/**
 * 动态模板：segments 按顺序给出中文原文里的字面量片段（首尾 + 中间分隔），
 * 中间夹住的部分按序作为 {0} {1} ... 填入英文模板。
 * 数组顺序即匹配顺序，越具体的模板要排在越前面（如 "启动 dsh 失败：" 先于 "启动 "）。
 */
interface BackendTemplate {
  readonly segments: readonly string[];
  readonly en: string;
}

const backendTemplates: readonly BackendTemplate[] = [
  {
    // 优先于下方 "打开浏览器失败：" 前缀模板，避免被更短的模板抢先匹配
    segments: ["打开浏览器失败：", " 执行超时（", "），子进程已被终止；状态不明确，不再回退其他程序"],
    en: "Failed to open the browser: {0} timed out ({1}); the child process was terminated; the state is unknown, no further fallback will be attempted",
  },
  {
    // 优先于下方 "启动 " 前缀模板
    segments: ["启动 dsh 失败：", ""],
    en: "Failed to start dsh: {0}",
  },
  { segments: ["启动失败：", ""], en: "Start failed: {0}" },
  { segments: ["端口 ", " 已被其他程序占用"], en: "Port {0} is already in use by another program" },
  { segments: ["端口 ", " 已有外部 Web 服务"], en: "An external web service is already running on port {0}" },
  { segments: ["启动 ", ""], en: "Starting {0}" },
  { segments: ["检查 DSH 退出状态失败：", ""], en: "Failed to check DSH exit status: {0}" },
  { segments: ["DSH 在启动期间退出：", ""], en: "DSH exited during startup: {0}" },
  { segments: ["DSH 已退出：", ""], en: "DSH has exited: {0}" },
  { segments: ["DSH 进程已退出：", ""], en: "DSH process has exited: {0}" },
  { segments: ["无法执行 dsh：", ""], en: "Failed to execute dsh: {0}" },
  { segments: ["检查 dsh --version 状态失败：", ""], en: "Failed to check dsh --version status: {0}" },
  { segments: ["DSH 参数格式无效：", ""], en: "Invalid DSH arguments: {0}" },
  { segments: ["无效的 DSH URL：", ""], en: "Invalid DSH URL: {0}" },
  { segments: ["打开内置 WebView 失败：", ""], en: "Failed to open the embedded WebView: {0}" },
  { segments: ["恢复内置 WebView 大小失败：", ""], en: "Failed to restore the embedded WebView size: {0}" },
  { segments: ["恢复内置 WebView 位置失败：", ""], en: "Failed to restore the embedded WebView position: {0}" },
  { segments: ["居中内置 WebView 失败：", ""], en: "Failed to center the embedded WebView: {0}" },
  { segments: ["显示内置 WebView 失败：", ""], en: "Failed to show the embedded WebView: {0}" },
  { segments: ["打开浏览器失败：", ""], en: "Failed to open the browser: {0}" },
  { segments: ["替换配置文件失败：", ""], en: "Failed to replace the config file: {0}" },
];

/** 按字面量片段切分消息，命中返回占位参数，否则返回 null。 */
function matchBackendTemplate(message: string, segments: readonly string[]): string[] | null {
  const first = segments[0] ?? "";
  const last = segments[segments.length - 1] ?? "";
  if (segments.length < 2 || !message.startsWith(first) || message.length < first.length + last.length) {
    return null;
  }
  if (!message.endsWith(last)) return null;
  let rest = message.slice(first.length, message.length - last.length);
  const params: string[] = [];
  for (let index = 1; index < segments.length - 1; index += 1) {
    const middle = segments[index] ?? "";
    const at = rest.indexOf(middle);
    if (at < 0) return null;
    params.push(rest.slice(0, at));
    rest = rest.slice(at + middle.length);
  }
  params.push(rest);
  return params;
}

/**
 * 翻译后端返回的 status.message / 错误串。
 * 中文界面原样返回；英文界面先查精确映射，再按模板匹配动态消息，未知消息原文返回。
 */
export function translateBackendMessage(message: string, lang: Lang): string {
  if (!message || lang === "zh") return message;
  const exact = backendExact[message];
  if (exact) return exact;
  for (const template of backendTemplates) {
    const params = matchBackendTemplate(message, template.segments);
    if (params) return format(template.en, ...params);
  }
  return message;
}