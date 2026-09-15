import { Fragment, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import packageInfo from "../package.json";
import {
  AlertTriangle,
  BellRing,
  Clock3,
  Download,
  LogIn,
  Pause,
  Play,
  Plus,
  RefreshCw,
  ShoppingBag,
  ShoppingCart,
  ShieldPlus,
  Trash2,
  UserRound,
  Volume2,
  X,
} from "lucide-react";

import {
  LOG_FILTERS,
  LogLine,
  RoundHeader,
  filterLogs,
  newestRound,
  roundStartIndex,
  logStamp,
  toneByLabel,
  type LogFilter,
} from "@/components/ActivityLog";
import { describeUpdateProgress, updatePercent } from "@/lib/updateStatus";
import { Combobox } from "@/components/Combobox";
import { MultiCombobox } from "@/components/MultiCombobox";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import {
  addToBagNow,
  changeLocale,
  closeBuyerWindow,
  connect,
  dismissUpdate,
  fillPickupInfo,
  installUpdate,
  openBuyerWindow,
  openReleasePage,
  openTargetProduct,
  refreshProducts,
  saveSettings,
  setCategory,
  setIntervalSeconds,
  setProductBarkUrl,
  setTargets,
  startWatching,
  stopWatching,
  testNotify,
  watcherStore,
} from "@/lib/store";
import {
  type Availability,
  type PickupDetails,
  type Category,
  type OpenOnHit,
  describeAdvice,
  formatTime,
  isUntrusted,
  type StatusTone,
  type Target,
  targetKey,
} from "@/lib/types";

import { describeDelivery, describeMonitorStatus, describePickupDate } from "@/lib/monitorLog";
import { compareNewestProducts, sortMonitorsNewestFirst } from "@/lib/productOrder";

/* 状态点：整张界面里唯一带颜色的东西，7px、紧挨着墨色文字。 */
const TONE_DOT: Record<StatusTone, string> = {
  inStock: "bg-in-stock",
  outOfStock: "bg-out-of-stock",
  presale: "bg-presale",
  comingSoon: "bg-coming-soon",
  pickupUnsupported: "bg-pickup-unsupported",
  notForSale: "bg-not-for-sale",
  unknown: "bg-unknown",
  pending: "bg-muted-foreground/40",
};

function StatusBadge({ availability, pickupDetails }: { availability: Availability; pickupDetails?: PickupDetails }) {
  const { label, tone, detail } = describeMonitorStatus({ availability, pickupDetails });
  const pickupDate = availability.kind === "in_stock" ? describePickupDate(pickupDetails?.pickupQuote) : null;
  const badge = (
    <span className="inline-flex w-full min-w-0 flex-col gap-0.5">
      <span className="inline-flex items-center gap-2.5 text-[13.5px] font-medium">
        <span className={`size-[7px] shrink-0 rounded-full ${TONE_DOT[tone]}`} aria-hidden="true" />
        {label}
      </span>
      {pickupDate ? <span className="truncate pl-[17px] text-[10.5px] leading-3 text-in-stock tabular-nums">{pickupDate} 可取</span> : null}
    </span>
  );
  if (!detail) return badge;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="cursor-help">{badge}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-90">{detail}</TooltipContent>
    </Tooltip>
  );
}

/**
 * 预计送货只展示 Apple 已返回的文案，不再自己上色：送得快慢是 Apple 的原文，
 * 不该抢在取货状态前面替用户下结论。
 */
function DeliveryBadge({ pickupDetails, lastCheckedMs }: { pickupDetails?: PickupDetails; lastCheckedMs: number | null }) {
  const delivery = describeDelivery(pickupDetails, lastCheckedMs);
  if (!delivery) return <span className="text-xs text-muted-foreground/70">未返回</span>;
  return (
    <span
      // w-full：单元格里是 inline-flex，收缩宽度会被 nowrap 文本的 min-content 顶开，
      // 长文案（例如「Apple 未提供日期」）会越过格子压到下一列。占满单元格宽度后，
      // 两行的 truncate 才会真正生效（完整文案仍在 title 与 aria-label 上）。
      className="flex w-full min-w-0 flex-col"
      title={`Apple 预计送货：${delivery.detail}`}
      aria-label={`预计送货 ${delivery.label}，${delivery.timing}。Apple 原始说明：${delivery.detail}`}
    >
      <span className="truncate text-[13px] text-muted-foreground tabular-nums">{delivery.label}</span>
      <span className="truncate text-[10.5px] leading-4 text-muted-foreground/70">{delivery.timing}</span>
    </span>
  );
}

function ProductBarkRoute({
  target,
  customUrl,
  disabled,
}: {
  target: Target;
  customUrl?: string;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const hasCustomUrl = Boolean(customUrl);

  async function save() {
    setSaving(true);
    try {
      if (await setProductBarkUrl(target, draft)) setOpen(false);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        if (nextOpen) setDraft(customUrl ?? "");
        setOpen(nextOpen);
      }}
    >
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className={`h-7 gap-1.5 rounded-[6px] px-2 text-[12.5px] ${hasCustomUrl ? "text-foreground" : "text-muted-foreground"}`}
          aria-label={`${target.productName}：${hasCustomUrl ? "已设置专属 Bark" : "使用默认 Bark"}`}
          disabled={disabled}
        >
          <BellRing aria-hidden="true" />
          {hasCustomUrl ? "专属" : "默认"}
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        className="w-96 max-w-[calc(100vw-2rem)] space-y-4 rounded-[9px] border-hair-strong p-4 shadow-[0_18px_40px_-16px_rgb(20_20_25/0.22)]"
      >
        <PopoverHeader>
          <PopoverTitle>此型号的 Bark 推送</PopoverTitle>
          <PopoverDescription className="break-words leading-5">
            {target.productName}。同一型号在不同门店共用此地址；留空则沿用右侧监控设置里的默认 Bark。
          </PopoverDescription>
        </PopoverHeader>
        <div className="field-group">
          <Label htmlFor={`product-bark-${targetKey(target)}`} className="control-label text-[10.5px] font-semibold">
            专属 Bark 地址
          </Label>
          <Input
            id={`product-bark-${targetKey(target)}`}
            type="url"
            className="control-surface select-text"
            placeholder="https://api.day.app/对方的BarkKey"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void save();
            }}
            disabled={saving}
          />
        </div>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setOpen(false)} disabled={saving}>
            取消
          </Button>
          <Button size="sm" onClick={() => void save()} disabled={saving}>
            {saving ? "保存中…" : draft.trim() ? "保存专属地址" : "使用默认地址"}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

/**
 * 表头：10.5px、字距 .11em、弱化墨色。
 * TableHead 自带 `text-sm font-medium text-foreground`，这里必须显式写出来才盖得住。
 */
const TABLE_HEAD_CLASS = "text-[10.5px] font-semibold tracking-[0.11em] text-muted-foreground/70";

type View = "monitor" | "log" | "settings";

const NAV_ITEMS: ReadonlyArray<{ value: View; label: string }> = [
  { value: "monitor", label: "监控" },
  { value: "log", label: "活动日志" },
  { value: "settings", label: "设置" },
];

function PageHead({ title, subtitle, meta }: { title: string; subtitle: string; meta: readonly string[] }) {
  return (
    <div className="flex shrink-0 items-start justify-between gap-6">
      <div className="min-w-0">
        <h1 className="text-[30px] leading-[1.1] font-semibold tracking-[-0.026em]">{title}</h1>
        <p className="mt-[7px] text-[13px] text-muted-foreground">{subtitle}</p>
      </div>
      <div className="shrink-0 text-right text-xs leading-[1.6] text-muted-foreground/70 tabular-nums">
        {meta.map((line) => (
          <div key={line}>{line}</div>
        ))}
      </div>
    </div>
  );
}

export default function App() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);
  const [clockMs, setClockMs] = useState(() => Date.now());
  const [view, setView] = useState<View>("monitor");
  const [logFilter, setLogFilter] = useState<LogFilter>("all");

  useEffect(() => {
    void connect();
  }, []);

  useEffect(() => {
    if (!ui.running) return;
    setClockMs(Date.now());
    const timer = window.setInterval(() => setClockMs(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, [ui.running]);

  const [storeNumbers, setStoreNumbers] = useState<string[]>([]);
  const [partNumbers, setPartNumbers] = useState<string[]>([]);
  const [isAdding, setIsAdding] = useState(false);
  const [barkDraft, setBarkDraft] = useState<string | null>(null);
  const [intervalDraft, setIntervalDraft] = useState<number | null>(null);
  // 清空取货信息后要靠它重挂载输入框，否则非受控的输入框还显示旧值。
  const [pickupFormKey, setPickupFormKey] = useState(0);

  const barkValue = barkDraft ?? ui.settings.barkUrl;
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;
  const latestCheckedMs = Math.max(0, ...ui.rows.map((row) => row.lastCheckedMs ?? 0));
  const secondsUntilNextCheck = latestCheckedMs
    ? Math.max(0, Math.ceil((latestCheckedMs + ui.settings.intervalSeconds * 1_000 - clockMs) / 1_000))
    : null;
  const runningLabel =
    secondsUntilNextCheck === null || secondsUntilNextCheck === 0
      ? "正在查询"
      : `约 ${secondsUntilNextCheck} 秒后检查`;

  const storeOptions = useMemo(
    () => ui.stores.map((store) => ({ value: store.number, label: store.title })),
    [ui.stores],
  );
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((product) => product.category === ui.category)
        .sort(compareNewestProducts)
        .map((product) => ({
          value: product.partNumber,
          label: product.title,
          description: `${product.partNumber}${product.capacity ? ` · ${product.capacity}` : ""}${product.color ? ` · ${product.color}` : ""}`,
        })),
    [ui.products, ui.category],
  );

  const sortedRows = useMemo(() => sortMonitorsNewestFirst(ui.rows), [ui.rows]);

  const targets = useMemo(() => ui.rows.map((row) => row.target), [ui.rows]);

  useEffect(() => {
    const valid = new Set(storeOptions.map((option) => option.value));
    setStoreNumbers((previous) => {
      const next = previous.filter((value) => valid.has(value));
      return next.length === previous.length ? previous : next;
    });
  }, [storeOptions]);

  useEffect(() => {
    const valid = new Set(productOptions.map((option) => option.value));
    setPartNumbers((previous) => {
      const next = previous.filter((value) => valid.has(value));
      return next.length === previous.length ? previous : next;
    });
  }, [productOptions]);

  const summary = useMemo(() => {
    let inStock = 0;
    let outOfStock = 0;
    let untrusted = 0;
    for (const row of ui.rows) {
      if (row.availability.kind === "in_stock") inStock += 1;
      else if (row.availability.kind === "out_of_stock") outOfStock += 1;
      if (isUntrusted(row.availability)) untrusted += 1;
    }
    return { inStock, outOfStock, untrusted };
  }, [ui.rows]);

  const storeCount = useMemo(
    () => new Set(ui.rows.map((row) => row.target.storeNumber)).size,
    [ui.rows],
  );

  const pendingTargets = useMemo(() => {
    const existing = new Set(targets.map(targetKey));
    const stores = new Map(ui.stores.map((store) => [store.number, store]));
    const products = new Map(ui.products.map((product) => [product.partNumber, product]));
    const pending: Target[] = [];

    for (const storeNumber of storeNumbers) {
      const store = stores.get(storeNumber);
      if (!store) continue;
      for (const partNumber of partNumbers) {
        const product = products.get(partNumber);
        if (!product || product.category !== ui.category) continue;
        const target: Target = {
          locale: ui.settings.locale,
          storeNumber: store.number,
          storeTitle: store.title,
          partNumber: product.partNumber,
          productName: product.title,
        };
        if (!existing.has(targetKey(target))) {
          existing.add(targetKey(target));
          pending.push(target);
        }
      }
    }
    return pending;
  }, [partNumbers, storeNumbers, targets, ui.category, ui.products, ui.settings.locale, ui.stores]);

  const hasCompleteSelection = storeNumbers.length > 0 && partNumbers.length > 0;
  const canAdd = pendingTargets.length > 0 && !isAdding;
  const duplicateCount = storeNumbers.length * partNumbers.length - pendingTargets.length;

  async function onAdd() {
    if (!canAdd) return;
    setIsAdding(true);
    try {
      if (await setTargets([...targets, ...pendingTargets])) setPartNumbers([]);
    } finally {
      setIsAdding(false);
    }
  }

  async function onRemove(target: Target) {
    await setTargets(targets.filter((item) => targetKey(item) !== targetKey(target)));
  }

  // 日志是唯一的原始记录：颜色靠当前行反查标签，不在这里另抄一份状态文案。
  const logTones = useMemo(() => toneByLabel(ui.rows), [ui.rows]);
  const filteredLogs = useMemo(() => filterLogs(ui.logs, logFilter, logTones), [ui.logs, logFilter, logTones]);
  const roundInfo = useMemo(() => newestRound(ui.logs), [ui.logs]);
  const roundHeaderIndex = roundInfo === null ? -1 : roundStartIndex(filteredLogs, roundInfo.cycle);
  const roundHeaderLine = roundHeaderIndex < 0 ? undefined : filteredLogs[roundHeaderIndex];
  const roundStamp = roundHeaderLine === undefined ? null : logStamp(roundHeaderLine);

  const secondsSinceCheck = latestCheckedMs
    ? Math.max(0, Math.floor((clockMs - latestCheckedMs) / 1_000))
    : null;
  const roundLabel = roundInfo === null ? "等待首轮查询" : `第 ${roundInfo.cycle} 轮`;
  const railMeta = ui.running
    ? secondsSinceCheck === null
      ? `${roundLabel} · 正在查询`
      : `${roundLabel} · ${secondsSinceCheck} 秒前`
    : `${roundLabel} · 已暂停`;

  const barkSummary = ui.settings.barkUrl.trim() === "" ? "未配置" : "已配置";
  const autoBagSummary = ui.settings.autoAddToBag ? "开启" : "关闭";
  const logCountLabel = `${roundInfo === null ? "" : `第 ${roundInfo.cycle} 轮 · `}共 ${ui.logs.length} 条${
    logFilter === "all" ? "" : ` · 命中 ${filteredLogs.length} 条`
  }`;

  return (
    <TooltipProvider delayDuration={180}>
      <div className="app-canvas flex h-screen flex-col overflow-hidden text-foreground">
        <div className="flex min-h-0 flex-1">
          <aside className="flex w-[208px] shrink-0 flex-col border-r border-hair px-[18px] pt-[26px] pb-5">
            <div className="flex items-center gap-2.5 px-2">
              <span className="brand-mark" aria-hidden="true" />
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold tracking-[-0.01em]">水果雷达</div>
                <div className="mt-0.5 truncate text-[11px] text-muted-foreground/70">Apple Store 库存监控</div>
              </div>
            </div>

            <nav className="mt-[30px] flex flex-col gap-0.5" aria-label="主导航">
              {NAV_ITEMS.map((item) => {
                const count =
                  item.value === "monitor" ? ui.rows.length : item.value === "log" ? ui.logs.length : null;
                return (
                  <button
                    key={item.value}
                    type="button"
                    className="rail-nav-item"
                    aria-current={view === item.value ? "page" : undefined}
                    onClick={() => setView(item.value)}
                  >
                    {item.label}
                    {count === null ? null : <span className="rail-nav-count">{count}</span>}
                  </button>
                );
              })}
            </nav>

            <div className="mt-auto px-2.5" role="status" aria-live="polite">
              <div className="mb-2 text-[11.5px] text-muted-foreground/70 tabular-nums">v{packageInfo.version}</div>
              <div className="flex items-center gap-2 text-[13px] font-medium">
                <span
                  className={`size-[7px] shrink-0 rounded-full ${ui.running ? "bg-in-stock" : "bg-muted-foreground/55"}`}
                  aria-hidden="true"
                />
                {ui.running ? "运行中" : "已暂停"}
              </div>
              <div className="mt-1.5 mb-3.5 text-[11.5px] text-muted-foreground/70 tabular-nums">{railMeta}</div>
              {ui.running ? (
                <button type="button" className="ghost-button" onClick={() => void stopWatching()}>
                  <Pause className="size-3.5" aria-hidden="true" /> 暂停监控
                </button>
              ) : (
                <button
                  type="button"
                  className="ghost-button"
                  onClick={() => void startWatching()}
                  disabled={ui.rows.length === 0}
                >
                  <Play className="size-3.5" aria-hidden="true" /> 开始监控
                </button>
              )}
            </div>
          </aside>

          <main className="flex min-h-0 min-w-0 flex-1 flex-col px-8 pt-[30px] pb-5">
            {ui.trouble !== null && (
              <Alert className="mb-4 shrink-0 rounded-[10px] border-hair-strong bg-card px-4 py-3">
                <AlertTriangle className="text-destructive" aria-hidden="true" />
                <AlertTitle className="text-destructive">监控结果暂不可信</AlertTitle>
                <AlertDescription>
                  {ui.trouble.reason}
                  <span>列表状态可能不代表真实库存，请先排查后再继续等待。</span>
                  {ui.trouble.advice !== null && (
                    <span className="font-medium text-foreground/85">{describeAdvice(ui.trouble.advice)}</span>
                  )}
                </AlertDescription>
              </Alert>
            )}

            {ui.update !== null && (
              <Alert className="mb-4 shrink-0 rounded-[10px] border-hair-strong bg-card px-4 py-3">
                <Download aria-hidden="true" />
                <AlertTitle>
                  {ui.updateInstalled ? "更新已安装" : "发现新版本"} {ui.update.version}
                </AlertTitle>
                <AlertDescription>
                  <span>
                    {ui.updateInstalled
                      ? "请退出并重新打开应用，新版本才会生效。"
                      : `当前版本 ${ui.update.currentVersion}，安装后需重启应用。`}
                  </span>
                  {ui.updateProgress !== null && (
                    <div className="w-full space-y-1" role="status" aria-live="polite">
                      <span>{describeUpdateProgress(ui.updateProgress)}</span>
                      {ui.updateProgress.phase === "downloading" && (
                        <progress
                          className="block h-2 w-full max-w-sm accent-primary"
                          aria-label="更新下载进度"
                          max={100}
                          value={updatePercent(ui.updateProgress)}
                        />
                      )}
                    </div>
                  )}
                  {ui.updateError !== null && (
                    <p className="break-words text-destructive" role="alert">
                      {ui.updateError}
                    </p>
                  )}
                  <div className="mt-1.5 flex items-center gap-2">
                    <Button size="sm" disabled={ui.installing || ui.updateInstalled} onClick={() => void installUpdate()}>
                      {ui.updateInstalled ? "已安装" : ui.installing ? "正在更新…" : ui.updateError ? "重试" : "下载并安装"}
                    </Button>
                    {ui.updateError !== null && (
                      <Button size="sm" variant="outline" onClick={() => void openReleasePage()}>
                        下载安装包
                      </Button>
                    )}
                    <Button size="sm" variant="ghost" disabled={ui.installing} onClick={dismissUpdate}>
                      <X aria-hidden="true" /> 稍后
                    </Button>
                  </div>
                </AlertDescription>
              </Alert>
            )}

            {view === "monitor" && (
              <div className="flex min-h-0 flex-1 flex-col">
                <PageHead
                  title="监控"
                  subtitle={`${ui.rows.length} 个商品 · ${storeCount} 家门店 · 每 ${ui.settings.intervalSeconds} 秒检查一次`}
                  meta={[ui.trouble === null ? "监控结果正常" : "监控结果暂不可信", ui.running ? runningLabel : "监控已暂停"]}
                />

                <div className="grid min-h-0 flex-1 grid-cols-1 gap-[22px] pt-[26px] min-[1100px]:grid-cols-[minmax(0,1fr)_320px]">
                  <div className="flex min-h-0 flex-col">
                    <section className="panel shrink-0" aria-labelledby="create-monitor-title">
                      <div className="panel-head">
                        <div>
                          <div className="eyebrow">
                            <Plus className="size-3" aria-hidden="true" /> 新建监控
                          </div>
                          <h2 id="create-monitor-title" className="panel-title mt-1.5">
                            选择想要追踪的门店与型号
                          </h2>
                        </div>
                        {hasCompleteSelection && (
                          <Badge
                            variant="outline"
                            className="rounded-[5px] border-transparent bg-secondary px-2 py-[3px] text-[11.5px] font-medium text-foreground"
                          >
                            {storeNumbers.length} × {partNumbers.length}
                          </Badge>
                        )}
                      </div>

                      <div className="mt-[18px] grid grid-cols-2 gap-3.5 lg:grid-cols-[1fr_1fr_1.2fr_1.6fr]">
                        <div className="field-group">
                          <Label className="control-label text-[10.5px] font-semibold">地区</Label>
                          <Combobox
                            className="control-surface w-full"
                            options={ui.regions.map((region) => ({ value: region.locale, label: region.title }))}
                            value={ui.settings.locale}
                            onChange={(locale) => {
                              setStoreNumbers([]);
                              setPartNumbers([]);
                              void changeLocale(locale);
                            }}
                            placeholder="选择地区"
                            searchPlaceholder="搜索地区…"
                            emptyText="没有匹配的地区"
                            disabled={isAdding}
                          />
                        </div>

                        <div className="field-group">
                          <Label className="control-label text-[10.5px] font-semibold">品类</Label>
                          <Combobox
                            className="control-surface w-full"
                            options={ui.categories.map((category) => ({ value: category.value, label: category.title }))}
                            value={ui.category}
                            onChange={(value) => {
                              setPartNumbers([]);
                              setCategory(value as Category);
                            }}
                            placeholder="选择品类"
                            searchPlaceholder="搜索品类…"
                            emptyText="没有匹配的品类"
                            disabled={isAdding || ui.categories.length === 0}
                          />
                        </div>

                        <div className="field-group">
                          <Label className="control-label text-[10.5px] font-semibold justify-between">
                            门店
                            <span className="text-[11px] font-medium tracking-normal text-muted-foreground normal-case">
                              可多选
                            </span>
                          </Label>
                          <MultiCombobox
                            key={`stores-${ui.settings.locale}`}
                            className="control-surface w-full"
                            options={storeOptions}
                            values={storeNumbers}
                            onChange={setStoreNumbers}
                            placeholder="选择自提门店"
                            searchPlaceholder="搜索门店…"
                            emptyText="没有匹配的门店"
                            selectionUnit="家门店"
                            disabled={isAdding || storeOptions.length === 0}
                          />
                        </div>

                        <div className="field-group">
                          <Label className="control-label text-[10.5px] font-semibold justify-between">
                            型号
                            <span className="text-[11px] font-medium tracking-normal text-muted-foreground normal-case">
                              可多选
                            </span>
                          </Label>
                          <MultiCombobox
                            key={`products-${ui.settings.locale}-${ui.category}`}
                            className="control-surface w-full"
                            options={productOptions}
                            values={partNumbers}
                            onChange={setPartNumbers}
                            placeholder="选择型号"
                            searchPlaceholder="搜索型号…"
                            emptyText="没有匹配的型号"
                            selectionUnit="个型号"
                            disabled={isAdding || productOptions.length === 0}
                          />
                        </div>
                      </div>

                      <div className="mt-[18px] flex flex-wrap items-center justify-between gap-3.5 border-t border-hair pt-3.5">
                        <p className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground" role="status">
                          {hasCompleteSelection
                            ? `将新增 ${pendingTargets.length} 条监控${duplicateCount > 0 ? `，跳过 ${duplicateCount} 条已有组合` : ""}`
                            : "选择门店和型号后，系统会按全部组合创建监控。"}
                        </p>
                        <div className="flex items-center gap-2.5">
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <Button
                                variant="outline"
                                size="icon-lg"
                                className="size-9 rounded-[7px] border-hair-strong bg-transparent"
                                aria-label="从 Apple 官网更新当前品类的型号列表"
                                disabled={ui.refreshing}
                                onClick={() => void refreshProducts()}
                              >
                                <RefreshCw className={ui.refreshing ? "animate-spin" : undefined} />
                              </Button>
                            </TooltipTrigger>
                            <TooltipContent>从 Apple 官网更新当前品类的型号列表</TooltipContent>
                          </Tooltip>
                          <Button className="h-9 min-w-28 rounded-[7px] px-4" onClick={() => void onAdd()} disabled={!canAdd}>
                            <Plus aria-hidden="true" />
                            {isAdding
                              ? "添加中…"
                              : canAdd
                                ? `添加 ${pendingTargets.length} 项`
                                : hasCompleteSelection
                                  ? "已在列表中"
                                  : "添加监控"}
                          </Button>
                        </div>
                      </div>
                    </section>

                    <section
                      className="surface-panel mt-5 flex min-h-[220px] flex-1 flex-col overflow-hidden"
                      aria-labelledby="monitor-list-title"
                    >
                      <div className="flex shrink-0 items-baseline justify-between gap-4 px-5 pt-[18px] pb-3.5">
                        <h2 id="monitor-list-title" className="panel-title">
                          监控列表
                        </h2>
                        <span className="text-xs text-muted-foreground/70 tabular-nums">
                          新款优先 · {ui.rows.length} 项
                        </span>
                      </div>

                      <ScrollArea className="min-h-0 flex-1">
                        <Table className="data-table min-w-[46rem] table-fixed text-[13.5px]">
                          <TableHeader className="sticky top-0 z-10 bg-card/95">
                            <TableRow className="border-0 hover:bg-transparent">
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[30%] pl-5`}>型号</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[15%] px-2`}>门店</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[13.5%] overflow-hidden px-1.5`}>取货状态</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[10%] overflow-hidden px-2`}>预计送货</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[10.5%] px-2`}>最后检查</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[10%] px-1.5`}>Bark</TableHead>
                              <TableHead className={`${TABLE_HEAD_CLASS} w-[11%] pl-1.5 pr-4`} />
                            </TableRow>
                          </TableHeader>
                          <TableBody>
                            {ui.rows.length === 0 ? (
                              <TableRow className="border-0 hover:bg-transparent">
                                <TableCell colSpan={7} className="h-44 py-3.5 text-center">
                                  <div className="mx-auto flex max-w-xs flex-col items-center">
                                    <p className="text-[13.5px] font-medium">
                                      {ui.ready ? "还没有监控项目" : "正在载入目录…"}
                                    </p>
                                    <p className="mt-1 text-xs leading-5 text-muted-foreground">
                                      {ui.ready
                                        ? "从上方选择门店和型号，添加后即可开始监控。"
                                        : "正在连接本地监控引擎，请稍候。"}
                                    </p>
                                  </div>
                                </TableCell>
                              </TableRow>
                            ) : (
                              sortedRows.map((row) => (
                                <TableRow key={targetKey(row.target)} className="group border-0 hover:bg-muted/40">
                                  <TableCell className="overflow-hidden py-3.5 pr-2 pl-5">
                                    <button
                                      type="button"
                                      className="block max-w-full truncate text-left text-[13.5px] font-medium hover:underline"
                                      title={row.target.productName}
                                      aria-label={`打开商品页：${row.target.productName}`}
                                      onClick={() => void openTargetProduct(row.target)}
                                    >
                                      {row.target.productName}
                                    </button>
                                  </TableCell>
                                  <TableCell className="overflow-hidden px-2 py-3.5">
                                    <span className="block truncate text-[13px] text-muted-foreground" title={row.target.storeTitle}>
                                      {row.target.storeTitle}
                                    </span>
                                  </TableCell>
                                  <TableCell className="overflow-hidden px-1.5 py-3.5">
                                    <StatusBadge availability={row.availability} pickupDetails={row.pickupDetails} />
                                  </TableCell>
                                  <TableCell className="overflow-hidden px-2 py-3.5">
                                    <DeliveryBadge pickupDetails={row.pickupDetails} lastCheckedMs={row.lastCheckedMs} />
                                  </TableCell>
                                  <TableCell className="px-2 py-3.5 font-mono text-[12px] text-muted-foreground/70 tabular-nums">
                                    {formatTime(row.lastCheckedMs)}
                                  </TableCell>
                                  <TableCell className="px-1 py-3.5">
                                    <ProductBarkRoute
                                      target={row.target}
                                      customUrl={ui.settings.productBarkUrls[row.target.partNumber]}
                                      disabled={isAdding}
                                    />
                                  </TableCell>
                                  <TableCell className="py-3.5 pr-4 pl-1">
                                    <div className="flex items-center justify-end gap-0.5">
                                      <Tooltip>
                                        <TooltipTrigger asChild>
                                          <Button
                                            variant="ghost"
                                            size="icon-sm"
                                            className="size-7 rounded-[6px] text-muted-foreground opacity-60 hover:text-foreground group-hover:opacity-100"
                                            aria-label={`试一次自动加购：${row.target.productName}`}
                                            disabled={isAdding}
                                            onClick={() => void addToBagNow(row.target)}
                                          >
                                            <ShoppingCart aria-hidden="true" />
                                          </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>试一次自动加购（只加到购物袋，不结账）</TooltipContent>
                                      </Tooltip>
                                      <Button
                                        variant="ghost"
                                        size="icon-sm"
                                        className="size-7 rounded-[6px] text-muted-foreground opacity-60 hover:text-destructive group-hover:opacity-100"
                                        aria-label="删除这条监控"
                                        disabled={isAdding}
                                        onClick={() => void onRemove(row.target)}
                                      >
                                        <Trash2 />
                                      </Button>
                                    </div>
                                  </TableCell>
                                </TableRow>
                              ))
                            )}
                          </TableBody>
                        </Table>
                      </ScrollArea>
                    </section>
                  </div>

                  <aside className="flex min-h-0 flex-col gap-4">
                    <section className="rpanel" aria-label="监控概览">
                      <h2 className="panel-cap">监控概览</h2>
                      <div className="mt-4 grid grid-cols-2 gap-x-3 gap-y-4">
                        <div>
                          <span className={`stat-value ${ui.rows.length === 0 ? "text-muted-foreground/70" : ""}`}>
                            {ui.rows.length}
                          </span>
                          <span className="stat-label">监控</span>
                        </div>
                        <div>
                          <span className={`stat-value ${summary.inStock > 0 ? "text-in-stock" : "text-muted-foreground/70"}`}>
                            {summary.inStock}
                          </span>
                          <span className="stat-label">有货</span>
                        </div>
                        <div>
                          <span className={`stat-value ${summary.outOfStock === 0 ? "text-muted-foreground/70" : ""}`}>
                            {summary.outOfStock}
                          </span>
                          <span className="stat-label">不可取货</span>
                        </div>
                        <div>
                          <span className={`stat-value ${summary.untrusted > 0 ? "text-unknown" : "text-muted-foreground/70"}`}>
                            {summary.untrusted}
                          </span>
                          <span className="stat-label">未确认</span>
                        </div>
                      </div>
                    </section>

                    <section className="rpanel" aria-label="监控设置">
                      <h2 className="panel-cap">监控设置</h2>
                      <div className="mt-3">
                        <div className="summary-row">
                          <span className="text-muted-foreground">检查间隔</span>
                          <span className="font-medium tabular-nums">{ui.settings.intervalSeconds} 秒</span>
                        </div>
                        <div className="summary-row">
                          <span className="text-muted-foreground">Bark 推送</span>
                          <span className="font-medium">{barkSummary}</span>
                        </div>
                        <div className="summary-row">
                          <span className="text-muted-foreground">自动加购</span>
                          <span className="font-medium">{autoBagSummary}</span>
                        </div>
                      </div>
                    </section>

                    <section
                      className="rpanel flex min-h-[140px] flex-1 flex-col overflow-hidden"
                      aria-labelledby="activity-log-title"
                    >
                      <div className="mb-3 flex shrink-0 items-baseline justify-between gap-4">
                        <h2 id="activity-log-title" className="panel-cap">
                          活动日志
                        </h2>
                        <span className="text-[11px] text-muted-foreground/70 tabular-nums">{ui.logs.length} 条</span>
                      </div>
                      <ScrollArea className="min-h-0 flex-1">
                        {ui.logs.length === 0 ? (
                          <p className="py-4 text-center text-xs text-muted-foreground">等待监控活动…</p>
                        ) : (
                          ui.logs.map((line, index) => <LogLine key={index} line={line} tones={logTones} />)
                        )}
                      </ScrollArea>
                    </section>
                  </aside>
                </div>
              </div>
            )}

            {view === "log" && (
              <div className="flex min-h-0 flex-1 flex-col">
                <PageHead
                  title="活动日志"
                  subtitle="原始记录 · 等宽显示 · 可选中复制"
                  meta={[`文件末尾 ${ui.logs.length} 行`, ui.running ? "监控运行中，日志持续追加" : "监控已暂停"]}
                />

                <div className="flex shrink-0 flex-wrap items-center justify-between gap-5 pt-6">
                  <div className="flex items-center gap-0.5 rounded-[7px] bg-secondary p-[3px]" role="group" aria-label="日志筛选">
                    {LOG_FILTERS.map((option) => {
                      const active = logFilter === option.value;
                      return (
                        <button
                          key={option.value}
                          type="button"
                          aria-pressed={active}
                          onClick={() => setLogFilter(option.value)}
                          className={`rounded-[5px] px-3 py-[5px] text-[12.5px] font-medium transition-colors ${
                            active ? "bg-card text-foreground" : "text-muted-foreground hover:text-foreground"
                          }`}
                        >
                          {option.label}
                        </button>
                      );
                    })}
                  </div>
                  <span className="text-xs text-muted-foreground/70 tabular-nums">{logCountLabel}</span>
                </div>

                <section
                  className="surface-panel mt-4 flex min-h-0 flex-1 flex-col overflow-hidden px-5 py-3.5"
                  aria-label="活动日志"
                >
                  <ScrollArea className="min-h-0 flex-1">
                    {filteredLogs.length === 0 ? (
                      <p className="py-6 text-center text-xs text-muted-foreground">
                        {ui.logs.length === 0 ? "等待监控活动…" : "当前筛选条件下没有记录。"}
                      </p>
                    ) : (
                      filteredLogs.map((line, index) => (
                        <Fragment key={index}>
                          {roundInfo !== null && index === roundHeaderIndex && (
                            <RoundHeader cycle={roundInfo.cycle} stamp={roundStamp} summary={roundInfo.summary} />
                          )}
                          <LogLine line={line} tones={logTones} />
                        </Fragment>
                      ))
                    )}
                  </ScrollArea>
                </section>
              </div>
            )}

            {view === "settings" && (
              <div className="flex min-h-0 flex-1 flex-col">
                <PageHead title="设置" subtitle="查询频率、提醒方式与取货信息" meta={[`水果雷达 v${packageInfo.version}`, barkSummary === "已配置" ? "Bark 推送已配置" : "Bark 推送未配置"]} />

                {/* 设置项会随功能增加而变长，这一块必须自己滚动。 */}
                <ScrollArea className="mt-[26px] min-h-0 flex-1 pr-1">
                  <div className="grid items-start gap-[18px] pb-6 min-[1100px]:grid-cols-2">
                    <section className="panel" aria-labelledby="preferences-title">
                      <div className="eyebrow">设置</div>
                      <h2 id="preferences-title" className="panel-title mt-1.5">
                        监控设置
                      </h2>
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">查询频率与提醒方式</p>

                      <div className="field-group mt-[18px]">
                        <Label htmlFor="interval" className="control-label text-[10.5px] font-semibold">
                          <Clock3 className="size-3" aria-hidden="true" /> 查询间隔
                        </Label>
                        <div className="relative">
                          <Input
                            id="interval"
                            type="number"
                            min={5}
                            className="control-surface select-text pr-12 tabular-nums"
                            value={intervalValue}
                            onChange={(event) => setIntervalDraft(event.target.valueAsNumber)}
                            onBlur={() => {
                              const seconds = Number.isFinite(intervalValue) ? Math.round(intervalValue) : 30;
                              setIntervalDraft(null);
                              void setIntervalSeconds(seconds);
                            }}
                          />
                          <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-xs text-muted-foreground">
                            秒
                          </span>
                        </div>
                      </div>

                      <div className="field-group mt-4">
                        <Label htmlFor="bark" className="control-label text-[10.5px] font-semibold">
                          <BellRing className="size-3" aria-hidden="true" /> 默认 Bark 推送
                        </Label>
                        <Input
                          id="bark"
                          className="control-surface select-text"
                          placeholder="https://api.day.app/你的BarkKey"
                          value={barkValue}
                          onChange={(event) => setBarkDraft(event.target.value)}
                          onBlur={() => {
                            setBarkDraft(null);
                            void saveSettings({ barkUrl: barkValue.trim() });
                          }}
                        />
                        <p className="text-xs leading-5 text-muted-foreground">
                          监控列表可为某个型号指定其他人的 Bark 地址。
                        </p>
                      </div>
                    </section>

                    <section className="panel" aria-labelledby="hit-actions-title">
                      <div className="eyebrow">到货动作</div>
                      <h2 id="hit-actions-title" className="panel-title mt-1.5">
                        有货之后做什么
                      </h2>
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">提示音、打开页面与自动加购</p>

                      <div className="mt-[18px]">
                        <div className="setting-row">
                          <span className="flex items-center gap-2 text-[13.5px]">
                            <Volume2 className="size-4 text-muted-foreground/70" aria-hidden="true" />
                            提示音
                          </span>
                          <Switch
                            id="sound"
                            aria-label="提示音"
                            checked={ui.settings.soundEnabled}
                            onCheckedChange={(value) => void saveSettings({ soundEnabled: value })}
                          />
                        </div>

                        <div className="setting-row">
                          <Label htmlFor="open-on-hit" className="flex items-center gap-2 text-[13.5px] font-normal">
                            <ShoppingBag className="size-4 text-muted-foreground/70" aria-hidden="true" />
                            到货后打开
                          </Label>
                          <Select
                            value={ui.settings.openOnHit}
                            onValueChange={(value) => void saveSettings({ openOnHit: value as OpenOnHit })}
                          >
                            <SelectTrigger
                              id="open-on-hit"
                              className="control-surface w-[8.5rem]"
                              aria-label="到货后打开"
                            >
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent align="end">
                              <SelectItem value="none">不自动打开</SelectItem>
                              <SelectItem value="bag">购物袋</SelectItem>
                              <SelectItem value="product">商品详情</SelectItem>
                            </SelectContent>
                          </Select>
                        </div>

                        <p className="px-0 py-2 text-[11px] leading-4 text-muted-foreground">
                          选「商品详情」时，手机上点通知会直接唤起 <span className="text-foreground/80">Apple Store App</span>
                          ；选「购物袋」只能打开 Safari。
                        </p>

                        <div className="setting-row">
                          <Label htmlFor="auto-add-to-bag" className="flex items-center gap-2 text-[13.5px] font-normal">
                            <ShoppingCart className="size-4 text-muted-foreground/70" aria-hidden="true" />
                            有货时自动加入购物袋
                          </Label>
                          <Switch
                            id="auto-add-to-bag"
                            aria-label="有货时自动加入购物袋"
                            checked={ui.settings.autoAddToBag}
                            onCheckedChange={(value) => void saveSettings({ autoAddToBag: value })}
                          />
                        </div>

                        {ui.settings.autoAddToBag && (
                          <div className="setting-row">
                            <Label htmlFor="bag-applecare" className="flex items-center gap-2 text-[13.5px] font-normal">
                              <ShieldPlus className="size-4 text-muted-foreground/70" aria-hidden="true" />
                              加入 AppleCare+
                            </Label>
                            <Switch
                              id="bag-applecare"
                              aria-label="加入 AppleCare+"
                              checked={ui.settings.bagApplecare}
                              onCheckedChange={(value) => void saveSettings({ bagApplecare: value })}
                            />
                          </div>
                        )}
                      </div>
                    </section>

                    {ui.settings.autoAddToBag && (
                      <section className="panel min-[1100px]:col-span-2" aria-labelledby="auto-bag-title">
                        <div className="eyebrow">自动加购</div>
                        <h2 id="auto-bag-title" className="panel-title mt-1.5">
                          买家浏览器
                        </h2>
                        <p className="mt-[18px] text-xs leading-5 text-muted-foreground">
                          自动加购在独立的「买家」浏览器里进行，用的是那个窗口自己的登录状态。
                          <span className="text-foreground/90">请先打开它并登录 Apple 账号</span>
                          ，否则加进购物袋的商品在结账时仍是未登录状态。登录同一个 Apple ID 后，
                          桌面加进购物袋的东西在手机的 Apple Store App 里也能看到。
                          开启后不再另外调用系统浏览器。程序只点到「添加到购物袋」为止，付款留给你。
                        </p>
                        <div className="mt-3 flex gap-2.5">
                          <Button variant="outline" className="h-9 flex-1 rounded-[7px] border-hair-strong" onClick={() => void openBuyerWindow()}>
                            <LogIn aria-hidden="true" /> 打开买家窗口
                          </Button>
                          <Button variant="outline" className="h-9 rounded-[7px] border-hair-strong" onClick={() => void closeBuyerWindow()}>
                            <X aria-hidden="true" /> 关闭
                          </Button>
                        </div>
                      </section>
                    )}

                    <section className="panel min-[1100px]:col-span-2" aria-labelledby="pickup-info-title">
                      <div className="eyebrow">结账代填</div>
                      <h2 id="pickup-info-title" className="panel-title mt-1.5">
                        取货信息（结账时代填）
                      </h2>
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">
                        Apple 不会从账号预填门店取货的联系人，每次都得手打。在这里填一次，走到结账页「继续填写取货详情」时点下面的按钮即可代填。
                        <span className="text-foreground/80">程序只填这五栏，不提交订单、不碰支付。</span>
                        这几项存在本机配置里，可随时清空。
                      </p>
                      <div key={pickupFormKey} className="mt-[18px] grid items-start gap-2.5 min-[900px]:grid-cols-3">
                        <Input
                          className="control-surface select-text"
                          placeholder="姓氏"
                          aria-label="取货人姓氏"
                          defaultValue={ui.settings.pickupLastName}
                          onBlur={(event) => void saveSettings({ pickupLastName: event.target.value.trim() })}
                        />
                        <Input
                          className="control-surface select-text"
                          placeholder="名字"
                          aria-label="取货人名字"
                          defaultValue={ui.settings.pickupFirstName}
                          onBlur={(event) => void saveSettings({ pickupFirstName: event.target.value.trim() })}
                        />
                        <Input
                          className="control-surface select-text"
                          placeholder="电子邮件地址"
                          aria-label="取货人电子邮箱"
                          defaultValue={ui.settings.pickupEmail}
                          onBlur={(event) => void saveSettings({ pickupEmail: event.target.value.trim() })}
                        />
                        <Input
                          className="control-surface select-text"
                          placeholder="手机号码"
                          aria-label="取货人手机号码"
                          defaultValue={ui.settings.pickupPhone}
                          onBlur={(event) => void saveSettings({ pickupPhone: event.target.value.trim() })}
                        />
                        <Input
                          className="control-surface select-text"
                          placeholder="身份证件号码后四位"
                          aria-label="身份证件后四位"
                          maxLength={4}
                          defaultValue={ui.settings.pickupIdLast4}
                          onBlur={(event) => void saveSettings({ pickupIdLast4: event.target.value.trim() })}
                        />
                        <div className="flex gap-2.5">
                          <Button variant="outline" className="h-9 flex-1 rounded-[7px] border-hair-strong" onClick={() => void fillPickupInfo()}>
                            <UserRound aria-hidden="true" /> 填写到结账页
                          </Button>
                          <Button
                            variant="outline"
                            className="h-9 rounded-[7px] border-hair-strong"
                            onClick={() => {
                              void saveSettings({
                                pickupLastName: "",
                                pickupFirstName: "",
                                pickupEmail: "",
                                pickupPhone: "",
                                pickupIdLast4: "",
                              });
                              setPickupFormKey((key) => key + 1);
                            }}
                          >
                            清空
                          </Button>
                        </div>
                      </div>
                    </section>

                    <section className="panel min-[1100px]:col-span-2" aria-labelledby="test-notify-title">
                      <div className="eyebrow">测试</div>
                      <h2 id="test-notify-title" className="panel-title mt-1.5">
                        提醒与跳转
                      </h2>
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">
                        按当前设置发一条测试提醒，并执行一次「到货后打开」的跳转；不会真的加购。
                      </p>
                      <Button
                        variant="outline"
                        className="mt-[18px] h-9 w-full rounded-[7px] border-hair-strong"
                        onClick={() => void testNotify()}
                      >
                        <BellRing aria-hidden="true" /> 测试提醒与跳转
                      </Button>
                    </section>
                  </div>
                </ScrollArea>
              </div>
            )}
          </main>
        </div>
      </div>
    </TooltipProvider>
  );
}
