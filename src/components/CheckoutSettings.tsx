import { useState } from "react";
import { LoaderCircle, PlayCircle, ShoppingCart } from "lucide-react";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { saveSettings, setCheckoutSwitches, testCheckout } from "@/lib/store";
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
  const mode = checkout.autoAddToBag ? "按到货型号加购，" : "";
  return mode + (checkout.placeOrder ? "到货后自动提交订单" : "到货后停在查看订单页");
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
  // 按到货型号加购时，任意一件中国大陆商品都能触发，不需要事先选定。
  const targetReady = checkout.autoAddToBag ? products.length > 0 : !!chosen;
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
            disabled={(!suffixReady || !targetReady) && !checkout.enabled}
            onCheckedChange={(enabled) => void setCheckoutSwitches({ enabled })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.autoAddToBag
            ? "到货后在买家窗口按本次到货型号加购 1 件，核对零件号与数量后结账。"
            : autoAddToBag
              ? "到货后先在买家窗口自动加购，再从购物袋开始结账。购物袋里必须只有这一件商品、数量为 1。"
              : "未开启自动加购：到货后直接从买家窗口的购物袋开始结账，请提前把这件商品放进购物袋。"}
          已点「立即下单」或检测到新订单时，会连同「自动点立即下单」一起关闭并保存。付款始终由你自己完成。
        </p>
        {!suffixReady ? (
          <p role="alert" className="text-[11px] leading-4 break-words text-destructive">
            请先在下方「取货信息」里填写证件号码后四位，才能开启自动结账。
          </p>
        ) : null}

        <div className="setting-row">
          <Label htmlFor="checkout-auto-add" className="text-[13.5px] font-normal">按到货型号自动加入购物袋</Label>
          <Switch
            id="checkout-auto-add"
            aria-label="按到货型号自动加入购物袋"
            checked={checkout.autoAddToBag}
            onCheckedChange={(value) => save({ autoAddToBag: value })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.autoAddToBag ? (
            <>
              <strong className="font-medium text-foreground">开启前建议先「打开买家窗口」手动清空购物袋，减少失败清理的等待。</strong>
              <br />
              监控列表里的中国大陆型号都能触发，同一时间只处理一件。已有本次型号 1 件时沿用；其他商品或数量不符会结束本轮并清空购物袋。提交前失败会移除本次商品、确认空袋后关闭买家窗口并继续监控；清理没完成时下次先继续清理，自动结账保持开启。请勿在这个购物袋保存其他商品；需另配表带的 Watch 暂不支持。
            </>
          ) : "关闭时使用事先准备的购物袋，只对下面选定的商品结账。"}
        </p>

        <div className="setting-row">
          <Label htmlFor="checkout-disable-after-success" className="text-[13.5px] font-normal">停在查看订单页后关闭自动结账</Label>
          <Switch
            id="checkout-disable-after-success"
            aria-label="停在查看订单页后关闭自动结账"
            checked={checkout.disableAfterSuccess}
            onCheckedChange={(value) => save({ disableAfterSuccess: value })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.disableAfterSuccess
            ? "到达「查看订单」页后关闭上方总开关。"
            : "到达「查看订单」页后保持总开关开启；监控继续运行时，持续有货会再次结账并切换买家窗口当前页面。"}
          已点「立即下单」（读到或没读到订单号）或提交结果不明时，总是关闭自动结账并保留页面，避免重复下单。Apple 要求重新登录时停下并保留页面，本应用不保存 Apple ID 密码、也不会替你登录。
        </p>

        {!checkout.autoAddToBag ? <div className="field-group">
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
        </div> : null}

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
            onCheckedChange={(placeOrder) => void setCheckoutSwitches({ placeOrder })}
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
          disabled={busy || !suffixReady || !targetReady}
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
          {checkout.autoAddToBag
            ? "先用「打开买家窗口」登录 Apple 账户并留空购物袋。试跑会加购监控列表里第一件中国大陆商品，停在「查看订单」页，不会提交订单；试跑失败保留页面供检查，不会清理购物袋。"
            : "首次使用先用「打开买家窗口」登录 Apple 账户，把要买的商品放进购物袋并手动走一次结账。试跑用「要下单的商品」和它第一条监控项的门店，不会提交订单。"}
        </p>
      </div>
    </CollapsiblePanel>
  );
}
