import type { ReactNode } from "react";

import { describeMonitorStatus } from "@/lib/monitorLog";
import type { StatusTone, TargetState } from "@/lib/types";

/**
 * 活动日志的展示层。
 *
 * 日志文本本身一个字都不改：这里只做「拆成时间戳 + 正文」和「给状态词上色」两件事，
 * 拼接回去还是原来那一行。状态标签 → 色调的反查表也是从当前这批行里现取的，
 * 不另外维护一份「哪些字算无货」的清单 —— 那种清单迟早和引擎里的判定对不上。
 */

export type LogFilter = "all" | "outOfStock" | "comingSoon" | "inStock";

export const LOG_FILTERS: ReadonlyArray<{ value: LogFilter; label: string }> = [
  { value: "all", label: "全部" },
  { value: "outOfStock", label: "仅无货" },
  { value: "comingSoon", label: "仅即将发售" },
  { value: "inStock", label: "仅有货" },
];

/** pushLogs 给每行加的 `[HH:MM:SS] ` 前缀。 */
const STAMP = /^\[[^\]]*\]\s?/;
/** 逐项行：`第 430 轮 · 无货：福建-厦门新生活广场 [R644] …`。 */
const ROW_LINE = /第 \d+ 轮 · ([^：]+)：/;
/** 汇总行：`第 430 轮完成（1.1 秒）：无货 4 项、即将发售 4 项。约 30 秒后查询。` */
const SUMMARY_LINE = /第 (\d+) 轮完成（([^）]*)）：(.*)$/;
const ANY_ROUND = /第 (\d+) 轮/;

const TAG_CLASS: Record<StatusTone, string> = {
  inStock: "text-in-stock",
  outOfStock: "text-out-of-stock",
  presale: "text-presale",
  comingSoon: "text-coming-soon",
  pickupUnsupported: "text-pickup-unsupported",
  notForSale: "text-not-for-sale",
  unknown: "text-unknown",
  pending: "text-muted-foreground",
};

const FILTER_TONES: Record<Exclude<LogFilter, "all">, readonly StatusTone[]> = {
  outOfStock: ["outOfStock"],
  // 「即将发售」和「暂未开售」都是还没开卖，归到同一个筛选项。
  comingSoon: ["comingSoon", "presale"],
  inStock: ["inStock"],
};

export function toneByLabel(rows: readonly TargetState[]): Map<string, StatusTone> {
  const tones = new Map<string, StatusTone>();
  for (const row of rows) {
    const { label, tone } = describeMonitorStatus(row);
    tones.set(label, tone);
  }
  return tones;
}

/** 纯前端字符串筛选：只读 ui.logs，不改、不写回。 */
export function filterLogs(
  logs: readonly string[],
  filter: LogFilter,
  tones: Map<string, StatusTone>,
): readonly string[] {
  if (filter === "all") return logs;
  const wanted = new Set<StatusTone>(FILTER_TONES[filter]);
  const labels = new Set<string>();
  for (const [label, tone] of tones) {
    if (wanted.has(tone)) labels.add(label);
  }
  if (labels.size === 0) return [];
  return logs.filter((line) => matchesFilter(line, labels));
}

function matchesFilter(line: string, labels: Set<string>): boolean {
  const row = ROW_LINE.exec(line);
  if (row !== null) {
    const label = row[1]?.trim();
    return label !== undefined && labels.has(label);
  }
  const summary = SUMMARY_LINE.exec(line);
  if (summary === null) return false;
  // 只在「无货 4 项、即将发售 4 项」这一段里比对：证据文字里也常出现这些词
  // （例如「不能视为无货」），拿整行去 includes 会把它们全算进来。
  const counts = summary[3] ?? "";
  for (const label of labels) {
    if (counts.includes(`${label} `)) return true;
  }
  return false;
}

export interface RoundInfo {
  cycle: number;
  /** `无货 4 项、即将发售 4 项 · 用时 1.1 秒`；本轮还没跑完时为 null。 */
  summary: string | null;
}

/** 最新一轮的轮次号与汇总，全部从日志文本里推出来。 */
export function newestRound(logs: readonly string[]): RoundInfo | null {
  let cycle = 0;
  for (const line of logs) {
    const round = ANY_ROUND.exec(line);
    if (round?.[1] !== undefined) cycle = Math.max(cycle, Number(round[1]));
  }
  if (cycle === 0) return null;
  for (const line of logs) {
    const summary = SUMMARY_LINE.exec(line);
    if (summary === null || Number(summary[1]) !== cycle) continue;
    const counts = (summary[3] ?? "").trim().replace(/。$/, "");
    const elapsed = summary[2] ?? "";
    return { cycle, summary: elapsed === "" ? counts : `${counts} · 用时 ${elapsed}` };
  }
  return { cycle, summary: null };
}

/** 某一轮在给定行序列里的起始下标；-1 表示这一轮没有可见的行。 */
export function roundStartIndex(lines: readonly string[], cycle: number): number {
  const marker = `第 ${cycle} 轮`;
  return lines.findIndex((line) => line.includes(marker));
}

/** 行首的 `[HH:MM:SS]`，没有就返回 null。 */
export function logStamp(line: string): string | null {
  const match = STAMP.exec(line);
  return match === null ? null : match[0].trimEnd();
}

function splitStamp(line: string): { stamp: string | null; body: string } {
  const match = STAMP.exec(line);
  if (match === null) return { stamp: null, body: line };
  return { stamp: match[0].trimEnd(), body: line.slice(match[0].length) };
}

/** 一行原始日志。文字不动，只把时间戳和状态词拆开上色。 */
export function LogLine({ line, tones }: { line: string; tones: Map<string, StatusTone> }) {
  const { stamp, body } = splitStamp(line);
  const row = ROW_LINE.exec(body);
  const label = row?.[1]?.trim();
  const tone = label === undefined ? undefined : tones.get(label);

  let content: ReactNode = body;
  if (row !== null && label !== undefined && tone !== undefined) {
    // row[0] 形如「第 430 轮 · 无货：」，标签之前的部分 = 匹配总长 - 标签长 - 1（冒号）。
    const labelStart = row.index + row[0].length - label.length - 1;
    content = (
      <>
        {body.slice(0, labelStart)}
        <span className={`font-semibold whitespace-nowrap ${TAG_CLASS[tone]}`}>{label}</span>
        {body.slice(labelStart + label.length)}
      </>
    );
  }

  return (
    <div className="flex gap-3.5 border-b border-hair py-2 font-mono text-[10.5px] leading-[1.72] last:border-b-0">
      <span className="shrink-0 text-muted-foreground/70">{stamp ?? ""}</span>
      <span className="min-w-0 flex-1 break-words text-muted-foreground">{content}</span>
    </div>
  );
}

/** 轮次标题行：轮次号、时间戳、本轮汇总。 */
export function RoundHeader({ cycle, stamp, summary }: { cycle: number; stamp: string | null; summary: string | null }) {
  return (
    <div className="flex items-center gap-3 border-b border-hair pt-2 pb-2.5">
      <span className="text-[11px] font-semibold tracking-[0.09em] text-muted-foreground/70 uppercase">
        第 {cycle} 轮
      </span>
      {stamp === null ? null : (
        <span className="font-mono text-[11px] text-muted-foreground/70">{stamp.replace(/^\[|\]$/g, "")}</span>
      )}
      <span className="ml-auto text-[11.5px] text-muted-foreground">{summary ?? "本轮尚未完成"}</span>
    </div>
  );
}
