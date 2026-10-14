import { useState } from "react";
import { LoaderCircle, PlayCircle, ShoppingCart } from "lucide-react";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { saveSettings, testCheckout } from "@/lib/store";
import { INSTALLMENT_PERIODS, type CheckoutPayment, type CheckoutSettings as Checkout, type Target } from "@/lib/types";

const PAYMENT_LABELS: Record<CheckoutPayment, string> = {
  alipay: "支付宝",
  wechat: "微信支付",
  installment: "分期付款",
};

/** 结账页「分期付款方案」里各方案的名称，须与页面上的完全一致。 */
const INSTALLMENT_BANKS = ["花呗分期", "微信分付", "招商银行", "中国建设银行", "工商银行"];

/** 花呗分期最多 12 期，其余方案沿用完整的期数列表。 */
function periodsFor(bank: string): readonly number[] {
  return bank === "花呗分期" ? INSTALLMENT_PERIODS.filter((periods) => periods <= 12) : INSTALLMENT_PERIODS;
}

function summarize(checkout: Checkout): string {
  if (!checkout.enabled) return "未开启";
  return checkout.placeOrder ? "到货后自动提交订单" : "到货后停在查看订单页";
}

/** 与后端 checkout_product_key 的规则一致：同一件商品在多家门店的监控项共用一个标识。 */
export function productKey(target: Target): string {
  return [target.partNumber, target.companionPart ?? "", target.kitPart ?? ""].join("|");
}

/** 监控列表里可以自动结账的不同商品（目前只支持中国大陆）。 */
export function monitoredProducts(targets: Target[]): { key: string; name: string }[] {
  const products = new Map<string, string>();
  for (const target of targets) {
    if (target.locale !== "zh_CN") continue;
    const key = productKey(target);
    if (!products.has(key)) {
      products.set(key, target.companionName ? `${target.productName} · ${target.companionName}` : target.productName);
    }
  }
  return [...products].map(([key, name]) => ({ key, name }));
}

/**
 * 到货后自动结账（移植自上游 v1.0.20）。
 *
 * 在自动加购用的买家窗口里进行；取货联系人和证件后四位使用「取货信息」里的那一份，
 * 这里不再另存一份。
 */
export function CheckoutSettings({
  checkout, targets, pickupIdLast4, autoAddToBag,
}: { checkout: Checkout; targets: Target[]; pickupIdLast4: string; autoAddToBag: boolean }) {
  const products = monitoredProducts(targets);
  // 只监控一件时就是它；监控多件时以设置里选的为准，选的那件已不在列表里则视为没选。
  const chosen = products.length === 1
    ? products[0]!.key
    : products.some((product) => product.key === checkout.product) ? checkout.product : "";
  const [busy, setBusy] = useState(false);
  const bankOptions = INSTALLMENT_BANKS.includes(checkout.installmentBank) || !checkout.installmentBank
    ? INSTALLMENT_BANKS
    : [...INSTALLMENT_BANKS, checkout.installmentBank];
  const suffixReady = pickupIdLast4.replace(/[^0-9a-zA-Z]/g, "").length >= 4;

  const save = (patch: Partial<Checkout>) => void saveSettings({ checkout: { ...checkout, ...patch } });

  return (
    <CollapsiblePanel
      id="checkout"
      icon={<ShoppingCart className="size-4" />}
      title="自动结账"
      subtitle="到货后在买家窗口里走完结账页面，仅支持中国大陆直营店取货"
      summary={summarize(checkout)}
    >
      <div className="mt-[18px] grid min-w-0 gap-3">
        <div className="setting-row">
          <Label htmlFor="checkout-enabled" className="text-[13.5px] font-normal">到货后自动结账</Label>
          <Switch
            id="checkout-enabled"
            aria-label="到货后自动结账"
            checked={checkout.enabled}
            disabled={(!suffixReady || !chosen) && !checkout.enabled}
            onCheckedChange={(enabled) => save({ enabled })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {autoAddToBag
            ? "到货后先在买家窗口自动加购，再从购物袋开始结账。购物袋里必须只有这一件商品、数量为 1。"
            : "未开启自动加购：到货后直接从买家窗口的购物袋开始结账，请提前把这件商品放进购物袋。"}
          成功执行一次后会自动关闭，需要时再手动开启。付款始终由你自己完成。
        </p>
        {!suffixReady ? (
          <p role="alert" className="text-[11px] leading-4 break-words text-destructive">
            请先在下方「取货信息」里填写证件号码后四位，才能开启自动结账。
          </p>
        ) : null}

        <div className="field-group">
          <Label htmlFor="checkout-product" className="control-label">要下单的商品</Label>
          {products.length > 1 ? (
            <Select value={chosen} onValueChange={(product) => save({ product })}>
              <SelectTrigger id="checkout-product" className="control-surface w-full" aria-label="要下单的商品">
                <SelectValue placeholder="请选择" />
              </SelectTrigger>
              <SelectContent>
                {products.map((product) => (
                  <SelectItem key={product.key} value={product.key}>{product.name}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <p id="checkout-product" className="text-sm break-words">
              {products[0]?.name ?? "监控列表里还没有中国大陆的商品"}
            </p>
          )}
          {products.length > 1 && !chosen ? (
            <p role="alert" className="text-[11px] leading-4 break-words text-destructive">
              监控列表里有 {products.length} 件商品，先选出要下单的那一件，才能开启自动结账。
            </p>
          ) : (
            <p className="text-[11px] leading-4 break-words text-muted-foreground">
              只有这件商品到货才会结账；开启自动结账后，自动加购也只加这一件。其他商品到货照常提醒。
            </p>
          )}
        </div>

        <div className="field-group">
          <Label htmlFor="checkout-payment" className="control-label">付款方式</Label>
          <Select value={checkout.payment} onValueChange={(payment) => save({ payment: payment as CheckoutPayment })}>
            <SelectTrigger id="checkout-payment" className="control-surface w-full" aria-label="付款方式">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(PAYMENT_LABELS) as CheckoutPayment[]).map((payment) => (
                <SelectItem key={payment} value={payment}>{PAYMENT_LABELS[payment]}</SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {checkout.payment === "installment" ? (
          <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_6rem] gap-2">
            <div className="field-group min-w-0">
              <Label htmlFor="checkout-bank" className="control-label">分期方案</Label>
              <Select value={checkout.installmentBank} onValueChange={(installmentBank) => {
                const allowed = periodsFor(installmentBank);
                const installmentPeriods = allowed.includes(checkout.installmentPeriods)
                  ? checkout.installmentPeriods
                  : Math.max(...allowed);
                save({ installmentBank, installmentPeriods });
              }}>
                <SelectTrigger id="checkout-bank" className="control-surface w-full" aria-label="分期方案">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {bankOptions.map((name) => (
                    <SelectItem key={name} value={name}>{name}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="field-group">
              <Label htmlFor="checkout-periods" className="control-label">期数</Label>
              <Select
                value={String(checkout.installmentPeriods)}
                onValueChange={(periods) => save({ installmentPeriods: Number(periods) })}
              >
                <SelectTrigger id="checkout-periods" className="control-surface w-full" aria-label="期数">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {periodsFor(checkout.installmentBank).map((periods) => (
                    <SelectItem key={periods} value={String(periods)}>{periods} 期</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
        ) : null}

        <div className="setting-row">
          <Label htmlFor="checkout-nearest-store" className="text-[13.5px] font-normal">优先用结账页默认选中的门店</Label>
          <Switch
            id="checkout-nearest-store"
            aria-label="优先用结账页默认选中的门店"
            checked={checkout.acceptNearestStore}
            onCheckedChange={(acceptNearestStore) => save({ acceptNearestStore })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.acceptNearestStore
            ? "结账页默认选中的最近有货门店可取货时直接在那里取货，可能不是监控到货的那一家。"
            : "只在监控到货的门店取货；结账页默认选中的门店也在这件商品的监控列表里时，直接用它。"}
        </p>

        <div className="setting-row">
          <Label htmlFor="checkout-place-order" className="text-[13.5px] font-normal">核对通过后自动点「立即下单」</Label>
          <Switch
            id="checkout-place-order"
            aria-label="核对通过后自动点立即下单"
            checked={checkout.placeOrder}
            onCheckedChange={(placeOrder) => save({ placeOrder })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.placeOrder
            ? "会生成真实订单，只点一次、不重试；下单后取货信息无法更改。付款（含支付密码）仍需你在 30 分钟内自己完成。"
            : "关闭时停在「查看订单」页，由你自己点「立即下单」并付款。"}
        </p>

        <Button
          type="button"
          variant="outline"
          className="h-9 rounded-[7px] border-hair-strong"
          disabled={busy || !suffixReady || !chosen}
          onClick={async () => {
            setBusy(true);
            await testCheckout();
            setBusy(false);
          }}
        >
          {busy ? <LoaderCircle className="animate-spin" aria-hidden="true" /> : <PlayCircle aria-hidden="true" />}
          试跑到查看订单页（不下单）
        </Button>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          首次使用先用「打开买家窗口」登录 Apple 账户，把要买的商品放进购物袋并手动走一次结账。试跑用「要下单的商品」和它第一条监控项的门店，不会提交订单。
        </p>
      </div>
    </CollapsiblePanel>
  );
}
