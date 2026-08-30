// 前端日志展示合并的纯函数 helper（不依赖 React / Tauri，可独立测试）。
// logs 的原始 state 始终保持不变：合并只产出派生的展示视图，绝不改写输入。
//
// 合并规则（默认展示模式，可经工具栏切回“原始”）：
// 1) 对侧合并：在最近的若干条展示行中，若存在与当前事件归一化消息相同、
//    来源互为 stdout/stderr 对侧、且时间差 <= MERGE_WINDOW_MS 的行，则并入该行；
// 2) 同源聚合：与上一条原始事件同来源、同消息（连续重复）的事件聚合计数；
// 3) 级别取参与事件的最高级别（error > warning > 其他），错误级别不会被降级。

export interface LogLine {
  timestamp: string;
  source: string;
  level: string;
  message: string;
}

/** 合并后的展示行：保留首条事件的时间戳，并附带聚合信息。 */
export interface MergedLogLine {
  timestamp: string;
  sources: string[];
  level: string;
  message: string;
  count: number;
  /** 首条事件在原始 logs 中的下标，用作稳定的 React key。 */
  firstIndex: number;
}

/** 允许合并的时间窗口（毫秒）。 */
const MERGE_WINDOW_MS = 2000;
/** 尝试合并时最多向前检查的展示条数（“近邻”限制）。 */
const MERGE_SCAN_LIMIT = 8;

// 匹配 CSI（颜色/光标控制）、OSC（窗口标题等）与常见两字节转义序列。
const ANSI_PATTERN =
  /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)|\u001b[@-_]/g;

/** 级别权重：数值越大越严重，合并时取最高。 */
function severity(level: string): number {
  const normalized = level.trim().toLowerCase();
  if (normalized === "error" || normalized === "fatal") return 3;
  if (normalized === "warning" || normalized === "warn") return 2;
  return 1;
}

/** 去 ANSI、trim 并压缩连续空白；作为消息等价判断与合并行展示的统一形式。 */
export function normalizeLogMessage(message: string): string {
  return message.replace(ANSI_PATTERN, "").trim().replace(/\s+/g, " ");
}

const TIME_PATTERN = /^(\d{1,2}):(\d{1,2}):(\d{1,2})(?:\.(\d{1,3}))?$/;

/** 解析后端 “%H:%M:%S%.3f” 时间戳为当天毫秒数；格式不规则时返回 null。 */
export function parseLogTime(timestamp: string): number | null {
  const match = TIME_PATTERN.exec(timestamp.trim());
  if (!match) return null;
  const hours = Number(match[1]);
  const minutes = Number(match[2]);
  const seconds = Number(match[3]);
  const millis = Number((match[4] ?? "0").padEnd(3, "0"));
  if (hours > 23 || minutes > 59 || seconds > 59) return null;
  return ((hours * 60 + minutes) * 60 + seconds) * 1000 + millis;
}

/** 两个事件的时间差（毫秒），自动处理跨零点回绕；任一端无法解析时按 0 处理。 */
function timeDelta(a: number | null, b: number | null): number {
  if (a === null || b === null) return 0;
  let delta = a - b;
  if (delta <= -43200000) delta += 86400000;
  else if (delta > 43200000) delta -= 86400000;
  return Math.abs(delta);
}

/** stdout → stderr、stderr → stdout 的对侧映射；其他来源（如 launcher）无对侧。 */
function oppositeSource(source: string): string | null {
  if (source === "stdout") return "stderr";
  if (source === "stderr") return "stdout";
  if (source === "npm-out") return "npm-err";
  if (source === "npm-err") return "npm-out";
  return null;
}

/** 内部累积中的展示行：比 MergedLogLine 多携带比较用的辅助字段。 */
interface OpenEntry extends MergedLogLine {
  norm: string;
  lastSource: string;
  lastTime: number | null;
}

/** 把一条事件并入已有展示行：计数 +1、级别取最高、来源去重追加、推进最近时间/来源。 */
function appendEvent(entry: OpenEntry, log: LogLine): void {
  entry.count += 1;
  if (severity(log.level) > severity(entry.level)) entry.level = log.level;
  if (!entry.sources.includes(log.source)) entry.sources.push(log.source);
  entry.lastSource = log.source;
  const parsed = parseLogTime(log.timestamp);
  if (parsed !== null) entry.lastTime = parsed;
}

/**
 * 把原始日志流折叠为合并展示行：
 * - 对侧合并只回看最近 MERGE_SCAN_LIMIT 条展示行（“近邻”限制；
 *   时间窗以该行最近一次并入事件的时间为基准）；
 * - 同源聚合仅当上一条原始事件归属的展示行同来源、同消息（连续重复）时生效；
 * - 返回的新数组与输入无共享可变状态，logs原始 state 不受影响。
 */
export function mergeLogs(logs: readonly LogLine[], windowMs: number = MERGE_WINDOW_MS): MergedLogLine[] {
  const entries: OpenEntry[] = [];
  let previous: OpenEntry | null = null;
  for (let index = 0; index < logs.length; index += 1) {
    const log = logs[index];
    const norm = normalizeLogMessage(log.message);
    const parsed = parseLogTime(log.timestamp);

    // 规则一：对侧来源合并，只检查最近若干条展示行
    let matched = false;
    const scanStart = Math.max(0, entries.length - MERGE_SCAN_LIMIT);
    for (let cursor = scanStart; cursor < entries.length && !matched; cursor += 1) {
      const candidate = entries[cursor];
      if (candidate.norm !== norm || candidate.sources.length !== 1) continue;
      const opposite = oppositeSource(candidate.sources[0]);
      if (opposite === null || log.source !== opposite) continue;
      if (timeDelta(parsed, candidate.lastTime) > windowMs) continue;
      appendEvent(candidate, log);
      previous = candidate;
      matched = true;
    }
    if (matched) continue;

    // 规则二：与上一条原始事件同来源且同消息（连续重复）→ 聚合计数
    if (previous && previous.norm === norm && previous.lastSource === log.source && timeDelta(parsed, previous.lastTime) <= windowMs) {
      appendEvent(previous, log);
      continue;
    }

    // 否则新起一行
    const entry: OpenEntry = {
      timestamp: log.timestamp,
      sources: [log.source],
      level: log.level,
      message: norm,
      count: 1,
      firstIndex: index,
      norm,
      lastSource: log.source,
      lastTime: parsed,
    };
    entries.push(entry);
    previous = entry;
  }
  return entries;
}