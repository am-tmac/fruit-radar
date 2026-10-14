//! 到货后自动结账（中国大陆直营店取货）。移植自上游 v1.0.20，按本 fork 的
//! 自动加购改造。
//!
//! # 与上游的差异
//!
//! 上游另开一个 `checkout-profile-v1` 浏览器；本 fork 已有自动加购用的持久
//! 「买家」浏览器（`chrome-buyer`），购物袋按那个资料目录的 Cookie 记账。两个
//! 浏览器同时占用一个资料目录会互相抢锁，换资料目录又会让「自动加购」加进去的
//! 商品在结账浏览器里看不见。所以这里的流程跑在同一个买家会话上
//! （[`crate::bag::BagSession`]，经 [`CdpPage`] 抽象），并由 `AppState.bag`
//! 那把锁和自动加购、代填取货信息、订单监听串行。
//!
//! 取货联系人和证件后四位来自设置顶层的 `pickup*` 字段，与「取货信息」面板
//! 共用一份。
//!
//! 页面元素按 Apple 结账页的 `data-autom` 标识定位。付款始终由用户完成。默认
//! 停在「查看订单」页；只有用户明确开启时才点「立即下单」，并且只点一次，
//! 无论结果如何都不重试。
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use apw_core::config::{CheckoutPayment, Settings, checkout_product_key, normalized_id_suffix};
use apw_core::model::Target;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// 结账流程需要的最小 DevTools 能力。生产实现是买家浏览器会话，测试用离线假页面。
pub(crate) trait CdpPage: Send {
    fn command(
        &mut self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, String>> + Send;
    fn evaluate(
        &mut self,
        expression: &str,
        await_promise: bool,
    ) -> impl Future<Output = Result<Value, String>> + Send;
}

impl CdpPage for crate::bag::BagSession {
    fn command(
        &mut self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, String>> + Send {
        crate::bag::BagSession::command(self, method, params)
    }
    fn evaluate(
        &mut self,
        expression: &str,
        await_promise: bool,
    ) -> impl Future<Output = Result<Value, String>> + Send {
        crate::bag::BagSession::evaluate(self, expression, await_promise)
    }
}

const BAG_URL: &str = "https://www.apple.com.cn/shop/bag";
const POLL_INTERVAL: Duration = Duration::from_millis(150);
// Apple 的结账接口实测每次要 9 到 17 秒才返回，超时必须明显长于这个数，
// 否则会在请求还没回来时重复点击，反而更慢。
const ELEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(45);
/// 门店切换后日期和时段是异步加载的；超过这个时间仍没有时段，就按无需时段处理。
const SLOT_TIMEOUT: Duration = Duration::from_secs(8);
/// 门店页的状态连续这么久没有变化，才认为异步加载结束了。
const SETTLE_WINDOW: Duration = Duration::from_millis(1200);
const ORDER_TIMEOUT: Duration = Duration::from_secs(45);

/// 结账流程停下时所在的页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Browser,
    Bag,
    SignIn,
    Fulfillment,
    PickupContact,
    Billing,
    Review,
    PlaceOrder,
}

impl Stage {
    fn title(self) -> &'static str {
        match self {
            Stage::Browser => "买家窗口",
            Stage::Bag => "购物袋",
            Stage::SignIn => "登录",
            Stage::Fulfillment => "选择取货门店",
            Stage::PickupContact => "取货信息",
            Stage::Billing => "付款方式",
            Stage::Review => "查看订单",
            Stage::PlaceOrder => "提交订单",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutError {
    pub stage: Stage,
    pub message: String,
}

impl CheckoutError {
    fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "在「{}」停下：{}", self.stage.title(), self.message)
    }
}

/// 一次自动结账要完成的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPlan {
    pub store_number: String,
    pub store_title: String,
    pub product_name: String,
    pub national_id_suffix: String,
    pub payment: CheckoutPayment,
    pub installment_bank: String,
    pub installment_periods: u8,
    pub place_order: bool,
    /// 这件商品还在监控的其他门店。结账页默认选中其中一家时可以直接用。
    pub monitored_stores: Vec<String>,
    pub accept_nearest_store: bool,
    /// 要填进取货信息页的联系信息：(结账页字段名, 值)。为空表示只核对 Apple 预填的内容。
    pub contact: Vec<(&'static str, String)>,
}

impl CheckoutPlan {
    /// 取货联系人来自设置顶层的 `pickup*`（与「取货信息」面板同一份）。留空的项
    /// 不填，沿用页面上已有的值；最后仍会核对四项都不为空。
    pub fn new(target: &Target, all: &Settings, place_order: bool) -> Self {
        let settings = &all.checkout;
        let monitored_stores = all
            .targets
            .iter()
            .filter(|other| {
                other.locale == target.locale
                    && checkout_product_key(other) == checkout_product_key(target)
            })
            .map(|other| other.store_number.clone())
            .collect();
        let contact = [
            ("lastName", all.pickup_last_name.trim().to_string()),
            ("firstName", all.pickup_first_name.trim().to_string()),
            ("emailAddress", all.pickup_email.trim().to_string()),
            ("fullDaytimePhone", mainland_phone(&all.pickup_phone)),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect();
        Self {
            contact,
            monitored_stores,
            accept_nearest_store: settings.accept_nearest_store,
            store_number: target.store_number.clone(),
            store_title: target.store_title.clone(),
            product_name: target.product_name.clone(),
            national_id_suffix: normalized_id_suffix(&all.pickup_id_last4),
            payment: settings.payment,
            installment_bank: settings.installment_bank.clone(),
            installment_periods: settings.installment_periods,
            place_order,
        }
    }
}

/// 结账页的区号固定是 +86，输入框里只要号码本身。
fn mainland_phone(raw: &str) -> String {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    match digits.strip_prefix("86") {
        Some(rest) if rest.len() == 11 => rest.to_string(),
        _ => digits,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutOutcome {
    /// 已停在「查看订单」页，等用户自己点「立即下单」。
    ReachedReview { store: String, total: String },
    /// 已点「立即下单」。`order_number` 为空表示页面上没有读到订单号，
    /// 订单是否成立需要用户在浏览器窗口里确认。
    OrderSubmitted {
        store: String,
        order_number: Option<String>,
        total: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LineItem {
    name: String,
    quantity: String,
}

/// 型号名称只比较去掉空白后的文字，大小写不敏感。
fn normalized_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 购物袋必须恰好是到货的那一件商品。
///
/// 宁可拒绝也不放宽：多一件商品、数量不是 1、或型号对不上，继续下去买到的
/// 就不是用户监控的东西。
fn verify_items(items: &[LineItem], product_name: &str) -> Result<(), String> {
    let [item] = items else {
        return Err(if items.is_empty() {
            "购物袋是空的，请先在买家窗口里把要买的商品加入购物袋".into()
        } else {
            format!(
                "购物袋里有 {} 件商品，自动结账只处理购物袋里只有到货型号的情况",
                items.len()
            )
        });
    };
    if normalized_name(&item.name) != normalized_name(product_name) {
        return Err(format!(
            "购物袋里是「{}」，到货的是「{}」",
            item.name.trim(),
            product_name.trim()
        ));
    }
    let quantity = item.quantity.trim();
    if !quantity.is_empty() && quantity != "1" {
        return Err(format!("购物袋里的数量是 {quantity}，自动结账只买 1 件"));
    }
    Ok(())
}

/// 结账页的门店标签形如「2 Apple 静安 明天 可取货 店内取货」或「… 目前不可取货 …」。
fn store_label_unavailable(label: &str) -> bool {
    let compact: String = label.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("不可取货") || compact.contains("无法取货") || compact.contains("暂无供应")
}

fn js_string(value: &str) -> String {
    // JSON 字符串字面量同时是合法的 JavaScript 字符串字面量。
    serde_json::Value::String(value.to_owned()).to_string()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageState {
    step: String,
    sign_in: bool,
}

const PAGE_STATE_JS: &str = r#"(() => {
  const step = new URLSearchParams(location.search).get('_s') || '';
  const signIn = /signin/i.test(location.pathname) || /signin/i.test(step) || /^idmsa\./.test(location.host);
  return JSON.stringify({ step, signIn });
})()"#;

/// 页面上当前可见的错误提示，用于解释为什么点了继续却没有前进。
const PAGE_ERRORS_JS: &str = r#"(() => {
  const nodes = [...document.querySelectorAll('[role="alert"], .rs-form-error, .form-message-wrapper, .rs-error-message')];
  const texts = nodes.filter((e) => e.offsetParent).map((e) => (e.innerText || '').replace(/\s+/g, ' ').trim()).filter(Boolean);
  return JSON.stringify([...new Set(texts)].slice(0, 3));
})()"#;

/// 购物袋页和查看订单页都用同一组 `bag-item-N` 标识列出商品。
const LINE_ITEMS_JS: &str = r#"(() => {
  const rows = [...document.querySelectorAll('[data-autom^="bag-item-"]')].filter((e) => /^bag-item-\d+$/.test(e.getAttribute('data-autom')));
  const items = rows.map((row) => {
    const title = row.querySelector('[data-autom="bag-item-name"], h2, h3');
    const select = row.querySelector('select[data-autom="item-quantity-dropdown"]');
    const text = (row.innerText || '').replace(/\s+/g, ' ');
    const counted = text.match(/数量\s*(\d+)/);
    return { name: (title ? title.innerText : '').replace(/\s+/g, ' ').trim(), quantity: select ? select.value : counted ? counted[1] : '' };
  });
  const total = document.querySelector('[data-autom="bagtotalvalue"]');
  return JSON.stringify({
    items,
    total: total ? total.innerText.replace(/\s+/g, ' ').trim() : '',
    canCheckout: !!document.querySelector('button[data-autom="checkout"]'),
    canPlaceOrder: !!document.querySelector('button[data-autom="continue-button-placeOrder"]'),
  });
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lines {
    items: Vec<LineItem>,
    total: String,
    can_checkout: bool,
    can_place_order: bool,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PickupDate {
    value: String,
    checked: bool,
    disabled: bool,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreState {
    count: usize,
    found: bool,
    disabled: bool,
    checked: bool,
    visible: bool,
    label: String,
    /// 页面当前选中的门店，没有选中时为空。
    selected: String,
    /// 「查附近门店」的请求已发出但还没有返回。
    search_pending: bool,
    selected_title: String,
    selected_label: String,
    /// `None` 表示页面上没有时段下拉框。
    slots: Option<Vec<String>>,
    slot: String,
    dates: Vec<PickupDate>,
    can_continue: bool,
}

fn store_state_js(store_number: &str) -> String {
    format!(
        r#"(() => {{
  const radios = [...document.querySelectorAll('input[name="store-locator-result"]')];
  const radio = radios.find((e) => e.value === {store});
  const selected = radios.find((e) => e.checked && !e.disabled);
  const text = (e) => e && e.labels[0] ? e.labels[0].innerText.replace(/\s+/g, ' ').trim() : '';
  const title = (e) => {{ const t = e && e.labels[0] && e.labels[0].querySelector('.form-selector-title'); return t ? t.innerText.trim() : ''; }};
  const select = document.querySelector('select[data-autom="pickup-availablewindow-dropdown"]');
  const next = document.querySelector('button[data-autom="fulfillment-continue-button"]');
  return JSON.stringify({{
    count: radios.length,
    found: !!radio,
    disabled: !!radio && radio.disabled,
    checked: !!radio && radio.checked,
    visible: !!radio && !!radio.offsetParent,
    label: text(radio),
    selected: selected ? selected.value : '',
    searchPending: !!window.__apwPickupClicked && !performance.getEntriesByType('resource').some((e) => e.name.includes('/checkoutx/fulfillment') && e.name.includes('_a=search')),
    selectedTitle: title(selected),
    selectedLabel: text(selected),
    slots: select ? [...select.options].map((o) => o.value).filter(Boolean) : null,
    slot: select ? select.value : '',
    dates: [...document.querySelectorAll('input[name="bartPickupDateSelectorButtonGroup"]')].map((e) => ({{ value: e.value, checked: e.checked, disabled: e.disabled }})),
    canContinue: !!next && !next.disabled,
  }});
}})()"#,
        store = js_string(store_number)
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactState {
    ready: bool,
    missing: Vec<String>,
    needs_id: bool,
}

const CONTACT_STATE_JS: &str = r#"(() => {
  const field = (name) => document.querySelector('input[data-autom="form-field-' + name + '"]');
  const labels = { lastName: '姓氏', firstName: '名字', emailAddress: '电子邮件地址', fullDaytimePhone: '手机号码' };
  const missing = Object.keys(labels).filter((name) => { const e = field(name); return !e || !e.value.trim(); }).map((name) => labels[name]);
  return JSON.stringify({ ready: !!field('lastName'), missing, needsId: !!field('nationalIdSelf') });
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingChoice {
    count: usize,
    found: bool,
    id: String,
    checked: bool,
    options: Vec<String>,
}

fn billing_choice_js(plan: &CheckoutPlan) -> String {
    let (kind, bank) = match plan.payment {
        CheckoutPayment::Alipay => ("ALIPAY", ""),
        CheckoutPayment::Wechat => ("WECHAT", ""),
        CheckoutPayment::Installment => ("INSTALLMENT", plan.installment_bank.as_str()),
    };
    format!(
        r#"(() => {{
  const kind = {kind};
  const bank = {bank}.replace(/\s+/g, '');
  const radios = [...document.querySelectorAll('input[data-autom^="checkout-billingOptions-"]')];
  const text = (e) => {{
    const label = e.labels[0];
    if (!label) return e.value;
    const alts = [...label.querySelectorAll('img')].map((img) => img.alt).join(' ');
    return ((label.innerText || '') + ' ' + alts).replace(/\s+/g, '') || e.value;
  }};
  const radio = kind === 'INSTALLMENT'
    ? radios.find((e) => e.value.startsWith('installments') && bank && text(e).includes(bank))
    : radios.find((e) => e.value === kind);
  // 选中状态要等服务器返回才更新。这段脚本会被反复执行，
  // 所以两次点击之间留出间隔，免得每次轮询都重新发一遍请求。
  const now = Date.now();
  if (radio && !radio.checked && now - (window.__apwLastOptionPick || 0) > 20000) {{
    window.__apwLastOptionPick = now;
    radio.click();
    if (!radio.checked && radio.labels[0]) radio.labels[0].click();
  }}
  return JSON.stringify({{
    count: radios.length,
    found: !!radio,
    id: radio ? radio.value : '',
    checked: !!radio && radio.checked,
    options: radios.map(text),
  }});
}})()"#,
        kind = js_string(kind),
        bank = js_string(bank)
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeriodChoice {
    found: bool,
    checked: bool,
    periods: Vec<String>,
}

fn period_choice_js(option_id: &str, periods: u8) -> String {
    format!(
        r#"(() => {{
  const prefix = {id} + '-';
  const radios = [...document.querySelectorAll('input[type="radio"]')].filter((e) => (e.getAttribute('data-autom') || '').startsWith(prefix) && e.offsetParent);
  const radio = radios.find((e) => e.getAttribute('data-autom') === prefix + {periods});
  // 选中状态要等服务器返回才更新。这段脚本会被反复执行，
  // 所以两次点击之间留出间隔，免得每次轮询都重新发一遍请求。
  const now = Date.now();
  if (radio && !radio.checked && now - (window.__apwLastPeriodPick || 0) > 20000) {{
    window.__apwLastPeriodPick = now;
    radio.click();
    if (!radio.checked && radio.labels[0]) radio.labels[0].click();
  }}
  return JSON.stringify({{
    found: !!radio,
    checked: !!radio && radio.checked,
    periods: radios.map((e) => e.value),
  }});
}})()"#,
        id = js_string(option_id),
        periods = js_string(&periods.to_string())
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderState {
    step: String,
    order: String,
}

const ORDER_STATE_JS: &str = r#"(() => {
  const found = (document.body ? document.body.innerText : '').match(/W\d{8,12}/);
  return JSON.stringify({ step: new URLSearchParams(location.search).get('_s') || '', order: found ? found[0] : '' });
})()"#;

fn click_js(selector: &str) -> String {
    format!(
        r#"(() => {{
  const e = document.querySelector({selector});
  if (!e || e.disabled) return false;
  e.click();
  return true;
}})()"#,
        selector = js_string(selector)
    )
}

struct Flow<'a, P: CdpPage> {
    session: &'a mut P,
    plan: &'a CheckoutPlan,
    progress: &'a (dyn Fn(String) + Send + Sync),
    /// 应用退出时置位：所有等待循环立即停下，不再操作页面。
    cancel: &'a AtomicBool,
    /// 实际取货的门店。默认是到货的那一家，沿用结账页默认选中的门店时会变。
    store_number: String,
    store_title: String,
}

impl<P: CdpPage> Flow<'_, P> {
    fn report(&self, message: impl Into<String>) {
        (self.progress)(message.into());
    }

    fn check_cancel(&self, stage: Stage) -> Result<(), CheckoutError> {
        if self.cancel.load(Ordering::Acquire) {
            Err(CheckoutError::new(stage, "应用正在退出，自动结账已中止"))
        } else {
            Ok(())
        }
    }

    async fn read<T: DeserializeOwned>(&mut self, script: &str) -> Result<T, String> {
        let value = self
            .session
            .evaluate(script, false)
            .await
            .map_err(|e| e.to_string())?;
        let raw = value
            .as_str()
            .ok_or_else(|| "页面脚本没有返回结果".to_string())?;
        serde_json::from_str(raw).map_err(|e| format!("页面返回的数据无法解析：{e}"))
    }

    /// 反复读取页面状态直到满足条件。
    ///
    /// 页面跳转瞬间执行脚本会失败（执行上下文被销毁），这属于正常现象，
    /// 所以读取失败只记下来，继续等到超时。
    async fn wait<T: DeserializeOwned>(
        &mut self,
        stage: Stage,
        script: &str,
        timeout: Duration,
        waiting_for: &str,
        done: impl Fn(&T) -> bool,
    ) -> Result<T, CheckoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            self.check_cancel(stage)?;
            let last_error = match self.read::<T>(script).await {
                Ok(state) if done(&state) => {
                    #[cfg(test)]
                    eprintln!(
                        "    等到{waiting_for}：{:.1}s",
                        (timeout - (deadline - Instant::now())).as_secs_f32()
                    );
                    return Ok(state);
                }
                Ok(_) => None,
                Err(error) => Some(error),
            };
            if Instant::now() >= deadline {
                #[cfg(test)]
                eprintln!("    等待{waiting_for}超时：{:.1}s", timeout.as_secs_f32());
                let detail = last_error.map(|e| format!("（{e}）")).unwrap_or_default();
                return Err(CheckoutError::new(
                    stage,
                    format!("等待{waiting_for}超时{detail}"),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn click(
        &mut self,
        stage: Stage,
        selector: &str,
        name: &str,
    ) -> Result<(), CheckoutError> {
        self.check_cancel(stage)?;
        let clicked = self
            .session
            .evaluate(&click_js(selector), false)
            .await
            .map_err(|e| CheckoutError::new(stage, e.to_string()))?;
        if clicked.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(CheckoutError::new(stage, format!("「{name}」按钮不可点击")))
        }
    }

    /// 等结账流程进入下一页。途中出现登录页就立即停下，交给用户处理。
    async fn wait_for_step(
        &mut self,
        stage: Stage,
        step_prefix: &str,
    ) -> Result<(), CheckoutError> {
        let deadline = Instant::now() + NAVIGATION_TIMEOUT;
        loop {
            self.check_cancel(stage)?;
            if let Ok(state) = self.read::<PageState>(PAGE_STATE_JS).await {
                if state.step.starts_with(step_prefix) {
                    #[cfg(test)]
                    eprintln!(
                        "    进入 {step_prefix}：{:.1}s",
                        (NAVIGATION_TIMEOUT - (deadline - Instant::now())).as_secs_f32()
                    );
                    return Ok(());
                }
                if state.sign_in {
                    return Err(CheckoutError::new(
                        Stage::SignIn,
                        "Apple 要求重新登录。请在买家窗口里登录并手动走一次结账，之后再开启自动结账",
                    ));
                }
            }
            if Instant::now() >= deadline {
                let errors: Vec<String> = self.read(PAGE_ERRORS_JS).await.unwrap_or_default();
                let detail = if errors.is_empty() {
                    String::new()
                } else {
                    format!("，页面提示：{}", errors.join("；"))
                };
                return Err(CheckoutError::new(
                    stage,
                    format!("点击继续后页面没有前进{detail}"),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn bag(&mut self) -> Result<(), CheckoutError> {
        self.report("自动结账：打开购物袋");
        let navigation = self
            .session
            .command("Page.navigate", json!({ "url": BAG_URL }))
            .await
            .map_err(|e| CheckoutError::new(Stage::Bag, e.to_string()))?;
        if let Some(error) = navigation
            .get("errorText")
            .and_then(serde_json::Value::as_str)
            .filter(|error| !error.trim().is_empty())
        {
            return Err(CheckoutError::new(
                Stage::Bag,
                format!("购物袋页面打不开：{error}"),
            ));
        }
        let lines: Lines = self
            .wait(
                Stage::Bag,
                LINE_ITEMS_JS,
                ELEMENT_TIMEOUT,
                "购物袋出现可结账的商品",
                |lines: &Lines| lines.can_checkout && !lines.items.is_empty(),
            )
            .await
            .map_err(|_| {
                CheckoutError::new(
                    Stage::Bag,
                    "购物袋里没有可结账的商品，请先在买家窗口里登录并把商品加入购物袋",
                )
            })?;
        verify_items(&lines.items, &self.plan.product_name)
            .map_err(|message| CheckoutError::new(Stage::Bag, message))?;
        self.click(Stage::Bag, r#"button[data-autom="checkout"]"#, "结账")
            .await?;
        self.wait_for_step(Stage::Bag, "Fulfillment").await
    }

    async fn fulfillment(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::Fulfillment;
        self.report(format!("自动结账：选择 {} 取货", self.plan.store_title));

        // 结账默认停在「为我送货」，取货门店列表要切到「我要取货」后才加载。
        const PICKUP_TAB_JS: &str = r#"(() => {
  const tab = [...document.querySelectorAll('button')].find((e) => (e.innerText || '').trim() === '我要取货');
  if (!tab) return JSON.stringify('missing');
  // 这是一组 role="radio" 的按钮，选中状态写在 aria-checked 上。
  if (tab.getAttribute('aria-checked') === 'true' || tab.getAttribute('aria-pressed') === 'true') return JSON.stringify('selected');
  // 切过去会触发一次「查附近门店」的请求，后面要靠资源计时记录判断它是否返回，
  // 所以先清空记录并放大缓冲区，免得记录因为缓冲区满了而丢失。
  if (!window.__apwPickupClicked) {
    performance.clearResourceTimings();
    performance.setResourceTimingBufferSize(2000);
    window.__apwPickupClicked = true;
  }
  tab.click();
  return JSON.stringify('clicked');
})()"#;
        self.wait(
            stage,
            PICKUP_TAB_JS,
            ELEMENT_TIMEOUT,
            "「我要取货」选项",
            |state: &String| state == "selected",
        )
        .await?;

        let mut script = store_state_js(&self.plan.store_number);
        self.wait(
            stage,
            &script,
            ELEMENT_TIMEOUT,
            "取货门店列表",
            |s: &StoreState| s.count > 0,
        )
        .await?;
        // 门店列表会先用页面自带的数据显示出来，但「查附近门店」的请求实测要十秒左右
        // 才返回，返回时页面会重画一遍，把这期间选的门店和时段清掉。Apple 的服务器
        // 又是按顺序处理请求的，抢在它前面操作并不会更快，所以等它回来再动手。
        let _ = self
            .wait(
                stage,
                &script,
                ELEMENT_TIMEOUT,
                "附近门店查询返回",
                |s: &StoreState| !s.search_pending,
            )
            .await;
        let mut state = self.settle(&script).await?;

        // Apple 会默认选中离取货地区最近的有货门店。换门店要再等它加载一轮（实测
        // 十几秒），所以默认选中的门店可以接受时直接用，不去换。
        let acceptable =
            self.plan.accept_nearest_store || self.plan.monitored_stores.contains(&state.selected);
        if !state.selected.is_empty()
            && state.selected != self.plan.store_number
            && acceptable
            && !store_label_unavailable(&state.selected_label)
        {
            self.store_number = state.selected.clone();
            self.store_title = if state.selected_title.is_empty() {
                state.selected.clone()
            } else {
                state.selected_title.clone()
            };
            self.report(format!(
                "自动结账：结账页已默认选中 {}，可取货，直接在这家取货",
                self.store_title
            ));
            script = store_state_js(&self.store_number);
            // 页面刚刚已经稳定，这里只是换成以这家门店为准重新读一次。
            state = self
                .read(&script)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
        }
        if !state.found {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "结账页的门店列表里没有 {}（{}）。请在买家窗口的购物袋里把取货地区改到这家门店附近",
                    self.plan.store_title, self.plan.store_number
                ),
            ));
        }
        if state.disabled || store_label_unavailable(&state.label) {
            return Err(CheckoutError::new(
                stage,
                format!("结账页显示这家门店目前不可取货：{}", state.label),
            ));
        }
        // 换门店会触发一轮异步加载：门店先短暂变回未选中，上一家门店的时段列表
        // 也会多留一会儿才被替换。所以每一步都等页面稳定下来再读，
        // 选完时段还要确认门店和时段都保持住，被重置了就重来。
        let mut confirmed = false;
        let mut last_seen = String::new();
        for _ in 0..3 {
            let state = self.settle(&script).await?;
            if !state.checked {
                last_seen = "门店未选中".into();
                self.select_store(&script, &state).await?;
                // 门店加载期间继续按钮是禁用的，它恢复可用才说明这家门店的日期和时段到位了。
                // 过早去选时段，选中的值会在加载完成时被清空。
                let _ = self
                    .wait(
                        stage,
                        &script,
                        ELEMENT_TIMEOUT,
                        "门店加载完成",
                        |s: &StoreState| s.checked && s.can_continue,
                    )
                    .await;
                continue;
            }
            let slot = self.pick_slot(&script, &state).await?;
            // 选完时段后 Apple 要向服务器校验，这期间继续按钮是禁用的，要等它恢复。
            let wanted = slot.clone().unwrap_or_default();
            let _ = self
                .wait(
                    stage,
                    &script,
                    ELEMENT_TIMEOUT,
                    "继续按钮可用",
                    // 时段被页面清掉时也立刻返回，交给下一轮重选，不用干等到超时。
                    |s: &StoreState| (s.checked && s.can_continue) || s.slot != wanted,
                )
                .await;
            let state = self.settle(&script).await?;
            // 「查看订单」页不显示门店，这里是确认门店选对的最后机会。
            if state.checked && state.can_continue && state.slot == wanted {
                if let Some(slot) = slot {
                    self.report(format!("自动结账：取货时段 {slot}"));
                }
                confirmed = true;
                break;
            }
            last_seen = format!(
                "门店{}，要选的时段 {}，页面上的时段 {}，可选 {} 个，继续按钮{}",
                if state.checked {
                    "已选中"
                } else {
                    "未选中"
                },
                slot.as_deref().unwrap_or("无"),
                if state.slot.is_empty() {
                    "空"
                } else {
                    &state.slot
                },
                state.slots.as_ref().map_or(0, Vec::len),
                if state.can_continue {
                    "可用"
                } else {
                    "不可用"
                },
            );
        }
        if !confirmed {
            return Err(CheckoutError::new(
                stage,
                format!("门店或取货时段没有保持选中，没有继续（{last_seen}）"),
            ));
        }
        self.click(
            stage,
            r#"button[data-autom="fulfillment-continue-button"]"#,
            "继续填写取货详情",
        )
        .await?;
        self.wait_for_step(stage, "PickupContact").await
    }

    /// 等门店页稳定：连续一段时间读到的状态完全相同。
    async fn settle(&mut self, script: &str) -> Result<StoreState, CheckoutError> {
        let stage = Stage::Fulfillment;
        let deadline = Instant::now() + ELEMENT_TIMEOUT;
        let mut last: Option<(StoreState, Instant)> = None;
        loop {
            self.check_cancel(stage)?;
            if let Ok(state) = self.read::<StoreState>(script).await {
                match &last {
                    Some((previous, since)) if *previous == state => {
                        if since.elapsed() >= SETTLE_WINDOW {
                            #[cfg(test)]
                            eprintln!(
                                "    页面稳定：{:.1}s",
                                (ELEMENT_TIMEOUT - (deadline - Instant::now())).as_secs_f32()
                            );
                            return Ok(state);
                        }
                    }
                    _ => last = Some((state, Instant::now())),
                }
            }
            if Instant::now() >= deadline {
                return Err(CheckoutError::new(
                    stage,
                    "门店页面一直在变化，没有稳定下来",
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn select_store(
        &mut self,
        script: &str,
        state: &StoreState,
    ) -> Result<(), CheckoutError> {
        let stage = Stage::Fulfillment;
        // 列表默认只展开前几家，其余收在「显示更多取货地点」里。
        // 收起状态下的单选项点了不会生效，还会把已选的门店清掉，所以先展开。
        if !state.visible {
            self.click(
                stage,
                r#"button[data-autom="show-more-stores-button"]"#,
                "显示更多取货地点",
            )
            .await?;
            self.wait(
                stage,
                script,
                ELEMENT_TIMEOUT,
                "门店列表展开",
                |s: &StoreState| s.visible,
            )
            .await?;
            // 展开动画期间点门店不会生效。
            self.settle(script).await?;
        }
        let selector = format!(
            r#"input[name="store-locator-result"][value={}]"#,
            js_string(&self.store_number)
        );
        self.click(stage, &selector, "门店").await
    }

    /// 选中最早可用的取货时段并返回它；这件商品不需要时段时返回 `None`。
    ///
    /// 日期默认选中最早的一天。那一天没有时段时，依次试后面的日期。
    async fn pick_slot(
        &mut self,
        script: &str,
        current: &StoreState,
    ) -> Result<Option<String>, CheckoutError> {
        // 时段已经选好就不要再动它：每改一次都会触发一轮服务器校验。
        if !current.slot.is_empty()
            && current
                .slots
                .as_ref()
                .is_some_and(|slots| slots.contains(&current.slot))
        {
            return Ok(Some(current.slot.clone()));
        }
        let stage = Stage::Fulfillment;
        let mut tried_dates: Vec<String> = Vec::new();
        let slot = loop {
            let loaded = self
                .wait(
                    stage,
                    script,
                    SLOT_TIMEOUT,
                    "取货时段",
                    |s: &StoreState| {
                        s.checked && s.slots.as_ref().is_some_and(|slots| !slots.is_empty())
                    },
                )
                .await;
            match loaded {
                Ok(state) => break state.slots.and_then(|slots| slots.into_iter().next()),
                Err(error) => {
                    let state: StoreState = self
                        .read(script)
                        .await
                        .map_err(|e| CheckoutError::new(stage, e))?;
                    if !state.checked {
                        return Err(CheckoutError::new(stage, "门店没有选中"));
                    }
                    if state.slots.is_none() {
                        // 这件商品不需要预约签到时段。
                        break None;
                    }
                    if let Some(current) = state.dates.iter().find(|date| date.checked) {
                        tried_dates.push(current.value.clone());
                    }
                    let Some(next) = state
                        .dates
                        .iter()
                        .find(|date| !date.disabled && !tried_dates.contains(&date.value))
                    else {
                        return Err(CheckoutError::new(
                            stage,
                            format!("没有可选的取货时段（{}）", error.message),
                        ));
                    };
                    let selector = format!(
                        r#"input[name="bartPickupDateSelectorButtonGroup"][value={}]"#,
                        js_string(&next.value)
                    );
                    self.click(stage, &selector, "取货日期").await?;
                    self.settle(script).await?;
                }
            }
        };
        let Some(slot) = slot else {
            return Ok(None);
        };

        // 受控的下拉框只认原生 setter 加事件，直接赋值不会更新页面状态。
        let set_slot = format!(
            r#"(() => {{
  const select = document.querySelector('select[data-autom="pickup-availablewindow-dropdown"]');
  if (!select) return JSON.stringify('');
  Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(select, {slot});
  select.dispatchEvent(new Event('input', {{ bubbles: true }}));
  select.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return JSON.stringify(select.value);
}})()"#,
            slot = js_string(&slot)
        );
        let applied: String = self
            .read(&set_slot)
            .await
            .map_err(|e| CheckoutError::new(stage, e))?;
        if applied != slot {
            return Err(CheckoutError::new(stage, "取货时段没有选中"));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(Some(slot))
    }

    async fn pickup_contact(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::PickupContact;
        self.report("自动结账：填写取货信息");
        let state: ContactState = self
            .wait(
                stage,
                CONTACT_STATE_JS,
                ELEMENT_TIMEOUT,
                "取货信息表单",
                |s: &ContactState| s.ready,
            )
            .await?;
        // 设置里开了「填写联系信息」就用设置里的值覆盖，否则沿用 Apple 预填的内容。
        let state = if self.plan.contact.is_empty() {
            state
        } else {
            let fields: serde_json::Map<String, serde_json::Value> = self
                .plan
                .contact
                .iter()
                .map(|(field, value)| ((*field).to_owned(), json!(value)))
                .collect();
            let fill = format!(
                r#"(() => {{
  const values = {values};
  const failed = [];
  for (const [name, value] of Object.entries(values)) {{
    const input = document.querySelector('input[data-autom="form-field-' + name + '"]');
    if (!input) {{ failed.push(name); continue; }}
    input.focus();
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, value);
    input.dispatchEvent(new Event('input', {{ bubbles: true }}));
    input.dispatchEvent(new Event('change', {{ bubbles: true }}));
    input.blur();
    if (input.value !== value) failed.push(name);
  }}
  return JSON.stringify(failed);
}})()"#,
                values = serde_json::Value::Object(fields)
            );
            let failed: Vec<String> = self
                .read(&fill)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
            if !failed.is_empty() {
                return Err(CheckoutError::new(
                    stage,
                    format!("联系信息没有填进去：{}", failed.join("、")),
                ));
            }
            self.read(CONTACT_STATE_JS)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?
        };
        // 下单后取货信息无法更改，缺了任何一项都停下，不带着空项往下走。
        if !state.missing.is_empty() {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "取货联系人缺少{}。请在设置的「取货信息」里填好，或在买家窗口里手动走一次结账",
                    state.missing.join("、")
                ),
            ));
        }
        if state.needs_id {
            let fill = format!(
                r#"(() => {{
  const input = document.querySelector('input[data-autom="form-field-nationalIdSelf"]');
  if (!input) return JSON.stringify(false);
  input.focus();
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, {suffix});
  input.dispatchEvent(new Event('input', {{ bubbles: true }}));
  input.dispatchEvent(new Event('change', {{ bubbles: true }}));
  input.blur();
  return JSON.stringify(input.value === {suffix});
}})()"#,
                suffix = js_string(&self.plan.national_id_suffix)
            );
            let filled: bool = self
                .read(&fill)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
            if !filled {
                return Err(CheckoutError::new(stage, "证件号码后四位没有填进去"));
            }
        }
        self.click(
            stage,
            r#"button[data-autom="continue-button-label"]"#,
            "继续选择付款方式",
        )
        .await?;
        self.wait_for_step(stage, "Billing").await
    }

    async fn billing(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::Billing;
        self.report("自动结账：选择付款方式");
        let script = billing_choice_js(self.plan);
        let choice: BillingChoice = self
            .wait(
                stage,
                &script,
                ELEMENT_TIMEOUT,
                "付款方式列表",
                |c: &BillingChoice| c.count > 0 && (!c.found || c.checked),
            )
            .await?;
        if !choice.found {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "结账页没有设置里选的付款方式，当前可选：{}",
                    choice.options.join("、")
                ),
            ));
        }
        if self.plan.payment == CheckoutPayment::Installment {
            // 期数选项在选中分期方案后才异步加载。
            let script = period_choice_js(&choice.id, self.plan.installment_periods);
            let period = self
                .wait(
                    stage,
                    &script,
                    ELEMENT_TIMEOUT,
                    "分期期数",
                    |p: &PeriodChoice| p.found && p.checked,
                )
                .await;
            if period.is_err() {
                let available: PeriodChoice = self
                    .read(&script)
                    .await
                    .map_err(|e| CheckoutError::new(stage, e))?;
                return Err(CheckoutError::new(
                    stage,
                    format!(
                        "{} 没有 {} 期，当前可选：{}",
                        self.plan.installment_bank,
                        self.plan.installment_periods,
                        available.periods.join("、")
                    ),
                ));
            }
        }
        self.click(
            stage,
            r#"button[data-autom="continue-button-review"]"#,
            "检查订单",
        )
        .await?;
        self.wait_for_step(stage, "Review").await
    }

    async fn review(&mut self) -> Result<CheckoutOutcome, CheckoutError> {
        let stage = Stage::Review;
        let lines: Lines = self
            .wait(
                stage,
                LINE_ITEMS_JS,
                ELEMENT_TIMEOUT,
                "订单确认内容",
                |lines: &Lines| lines.can_place_order && !lines.items.is_empty(),
            )
            .await?;
        verify_items(&lines.items, &self.plan.product_name)
            .map_err(|message| CheckoutError::new(stage, message))?;
        if !self.plan.place_order {
            return Ok(CheckoutOutcome::ReachedReview {
                store: self.store_title.clone(),
                total: lines.total,
            });
        }

        self.report("自动结账：核对通过，提交订单");
        // 这个按钮只点一次。之后无论页面怎样，都不重试，避免重复下单。
        self.click(
            Stage::PlaceOrder,
            r#"button[data-autom="continue-button-placeOrder"]"#,
            "立即下单",
        )
        .await?;
        let order = self
            .wait(
                Stage::PlaceOrder,
                ORDER_STATE_JS,
                ORDER_TIMEOUT,
                "订单号",
                |state: &OrderState| !state.order.is_empty() && !state.step.starts_with("Review"),
            )
            .await
            .ok()
            .map(|state| state.order);
        Ok(CheckoutOutcome::OrderSubmitted {
            store: self.store_title.clone(),
            order_number: order,
            total: lines.total,
        })
    }

    async fn run(&mut self) -> Result<CheckoutOutcome, CheckoutError> {
        self.bag().await?;
        self.fulfillment().await?;
        self.pickup_contact().await?;
        self.billing().await?;
        self.review().await
    }
}

/// 自动结账的单次执行闸门。
///
/// 同一时间只允许一次结账：到货事件在同一轮里可能连来好几条（同一商品的多家
/// 门店），持续有货时下一轮还会再来。拿不到租约的直接放弃，不排队。浏览器会话
/// 本身由 `AppState.bag` 持有，这里只管「能不能开始」与「退出时叫停」。
#[derive(Clone, Default)]
pub struct CheckoutRunner {
    busy: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

/// 持有期间其他结账不能开始；析构即释放，任务被取消也一样。
pub struct CheckoutLease(Arc<AtomicBool>);

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CheckoutRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试占住结账。已有结账在进行，或应用正在退出时返回 `None`。
    pub fn try_begin(&self) -> Option<CheckoutLease> {
        if self.cancel.load(Ordering::Acquire) {
            return None;
        }
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(CheckoutLease(self.busy.clone()))
    }

    /// 是否有一次结账正在进行。
    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// 应用退出时调用：叫停正在进行的结账，之后不再接受新的结账。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// 在给定页面上执行一次结账。调用方须持有 [`CheckoutLease`] 和买家会话锁。
    pub async fn run<P: CdpPage>(
        &self,
        _lease: &CheckoutLease,
        session: &mut P,
        plan: &CheckoutPlan,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<CheckoutOutcome, CheckoutError> {
        if self.is_cancelled() {
            return Err(CheckoutError::new(Stage::Browser, "应用正在退出，自动结账已中止"));
        }
        if plan.national_id_suffix.chars().count() != 4 {
            return Err(CheckoutError::new(
                Stage::PickupContact,
                "还没有在「取货信息」里填写完整的证件号码后四位",
            ));
        }
        // 把窗口带到前台：流程停下时用户需要立刻看到页面。
        let _ = session.command("Page.bringToFront", json!({})).await;
        Flow {
            session,
            plan,
            progress,
            cancel: &self.cancel,
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
        }
        .run()
        .await
    }
}

/// 把取货联系人信息从要写进活动日志或推送的文字里抹掉。
///
/// 结账页的错误提示可能原样带出页面上的值。只处理两个字符以上的值：单字姓氏
/// 这类常见字替换掉只会把门店名之类的正常文字弄坏。
pub fn redact_contact(message: &str, settings: &Settings) -> String {
    let mut values: Vec<String> = [
        settings.pickup_last_name.trim().to_string(),
        settings.pickup_first_name.trim().to_string(),
        format!(
            "{}{}",
            settings.pickup_last_name.trim(),
            settings.pickup_first_name.trim()
        ),
        settings.pickup_email.trim().to_string(),
        settings.pickup_phone.trim().to_string(),
        mainland_phone(&settings.pickup_phone),
        settings.pickup_id_last4.trim().to_string(),
        normalized_id_suffix(&settings.pickup_id_last4),
    ]
    .into_iter()
    .filter(|value| value.chars().count() >= 2)
    .collect();
    // 先替换长的，避免短值先把长值切碎后漏掉一部分。
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    let mut redacted = message.to_string();
    for value in values {
        redacted = redacted.replace(&value, "***");
    }
    redacted
}

/// 一次到货事件要触发的自动化动作。纯函数，便于离线验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HitActions {
    /// 在买家窗口里自动加入购物袋。
    pub add_to_bag: bool,
    /// 自动结账（加购成功后接着做；未开自动加购时直接从购物袋开始）。
    pub checkout: bool,
    /// 这一轮处理完后暂停监控。
    pub pause_monitoring: bool,
}

/// 决定一次到货要做什么。
///
/// 开着自动结账时，自动加购只加「要下单的商品」：结账要求购物袋里恰好只有这一件，
/// 别的商品被加进去反而会让结账在购物袋页停下。其他商品照常提醒。
pub fn hit_actions(settings: &Settings, target: &Target) -> HitActions {
    let checkout = settings.checkout.enabled && settings.is_checkout_target(target);
    let add_to_bag = settings.auto_add_to_bag && (!settings.checkout.enabled || checkout);
    HitActions {
        add_to_bag,
        checkout,
        pause_monitoring: settings.stop_on_hit,
    }
}

/// 结账结束后要写回的设置。
///
/// 成功一次（停在查看订单页或已点立即下单）就关闭自动结账：再次到货时重跑会
/// 打断用户正在看的页面，已下单时还会重复下单。在「立即下单」这一步出错时无法
/// 确定订单有没有生成，同样关闭。其他失败保留开关，下次到货可以再试。
pub fn settings_after_checkout(
    current: &Settings,
    result: &Result<CheckoutOutcome, CheckoutError>,
) -> Option<Settings> {
    let disable = match result {
        Ok(_) => true,
        Err(error) => error.stage == Stage::PlaceOrder,
    };
    if !disable || !current.checkout.enabled {
        return None;
    }
    let mut next = current.clone();
    next.checkout.enabled = false;
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, quantity: &str) -> LineItem {
        LineItem {
            name: name.into(),
            quantity: quantity.into(),
        }
    }

    #[test]
    fn 购物袋恰好是到货型号时通过核对() {
        let items = [item("iPhone 18 Pro 256GB 冰川蓝色", "1")];
        assert_eq!(verify_items(&items, "iPhone 18 Pro 256GB 冰川蓝色"), Ok(()));
        // 空白和大小写的差异不算不一致。
        assert_eq!(
            verify_items(&items, "iphone 18 Pro  256gb 冰川蓝色"),
            Ok(())
        );
        // 查看订单页可能读不到数量下拉框。
        let unknown_quantity = [item("iPhone 18 Pro 256GB 冰川蓝色", "")];
        assert_eq!(
            verify_items(&unknown_quantity, "iPhone 18 Pro 256GB 冰川蓝色"),
            Ok(())
        );
    }

    #[test]
    fn 型号不一致或数量不是一件时拒绝继续() {
        let other = [item("iPhone 18 Pro Max 256GB 冰川蓝色", "1")];
        assert!(verify_items(&other, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
        let two = [item("iPhone 18 Pro 256GB 冰川蓝色", "2")];
        assert!(verify_items(&two, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
    }

    #[test]
    fn 购物袋为空或有多件商品时拒绝继续() {
        assert!(verify_items(&[], "iPhone 18 Pro 256GB 冰川蓝色").is_err());
        let many = [
            item("iPhone 18 Pro 256GB 冰川蓝色", "1"),
            item("腕带 – 清透蓝色", "1"),
        ];
        assert!(verify_items(&many, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
    }

    #[test]
    fn 门店标签能区分可取货与不可取货() {
        assert!(!store_label_unavailable(
            "2 Apple 静安 明天 可取货 店内取货"
        ));
        assert!(store_label_unavailable(
            "4 Apple 七宝 目前不可取货 店内取货"
        ));
        assert!(store_label_unavailable("4Apple 七宝目前不可取货店内取货"));
    }

    #[test]
    fn 写进页面脚本的文字不会破坏脚本结构() {
        let script = store_state_js("R1\"); alert(1); (\"");
        assert!(script.contains(r#"e.value === "R1\"); alert(1); (\"""#));
        let script = billing_choice_js(&CheckoutPlan {
            store_number: "R678".into(),
            store_title: "Apple 静安".into(),
            product_name: "x".into(),
            national_id_suffix: "123X".into(),
            payment: CheckoutPayment::Installment,
            installment_bank: "招商'银行".into(),
            installment_periods: 24,
            place_order: false,
            monitored_stores: Vec::new(),
            contact: Vec::new(),
            accept_nearest_store: false,
        });
        assert!(script.contains(r#"const kind = "INSTALLMENT";"#));
        assert!(script.contains(r#"const bank = "招商'银行""#));
    }

    #[test]
    fn 错误信息带上停下的页面() {
        let error = CheckoutError::new(Stage::Fulfillment, "门店没有选中");
        assert_eq!(error.to_string(), "在「选择取货门店」停下：门店没有选中");
    }

    // ---- 离线行为测试：假页面按脚本内容应答，不启动浏览器、不访问网络 ----

    struct FakePage {
        step: &'static str,
        items: Vec<(&'static str, &'static str)>,
        store_checked: bool,
        clicks: Vec<String>,
        scripts: Vec<String>,
    }

    impl FakePage {
        fn new(items: Vec<(&'static str, &'static str)>) -> Self {
            Self { step: "", items, store_checked: true, clicks: Vec::new(), scripts: Vec::new() }
        }
        fn clicked(&self, needle: &str) -> usize {
            self.clicks.iter().filter(|c| c.contains(needle)).count()
        }
    }

    fn js(value: serde_json::Value) -> Value {
        Value::String(value.to_string())
    }

    impl CdpPage for FakePage {
        async fn command(&mut self, method: &str, _params: Value) -> Result<Value, String> {
            if method == "Page.navigate" {
                self.step = "Bag";
            }
            Ok(json!({}))
        }
        async fn evaluate(&mut self, script: &str, _await: bool) -> Result<Value, String> {
            self.scripts.push(script.to_string());
            if script.contains("if (!e || e.disabled) return false;") {
                self.clicks.push(script.to_string());
                if script.contains(r#"data-autom=\"checkout\""#) && self.step == "Bag" {
                    self.step = "Fulfillment";
                } else if script.contains("fulfillment-continue-button") {
                    self.step = "PickupContact";
                } else if script.contains("continue-button-label") {
                    self.step = "Billing";
                } else if script.contains("continue-button-review") {
                    self.step = "Review";
                } else if script.contains("continue-button-placeOrder") {
                    self.step = "Thankyou";
                } else if script.contains("store-locator-result") {
                    self.store_checked = true;
                }
                return Ok(Value::Bool(true));
            }
            if script.contains("needsId") {
                return Ok(js(json!({"ready": self.step == "PickupContact", "missing": [], "needsId": true})));
            }
            if script.contains("Object.entries(values)") {
                return Ok(js(json!([])));
            }
            if script.contains("form-field-nationalIdSelf") {
                return Ok(js(json!(true)));
            }
            if script.contains("我要取货") {
                return Ok(js(json!("selected")));
            }
            if script.contains("searchPending") {
                return Ok(js(json!({
                    "count": 1, "found": true, "disabled": false, "checked": self.store_checked,
                    "visible": true, "label": "Apple 测试店 今天 可取货", "selected": "R000",
                    "searchPending": false, "selectedTitle": "Apple 测试店",
                    "selectedLabel": "Apple 测试店 今天 可取货",
                    "slots": ["slot-1"], "slot": "slot-1", "dates": [], "canContinue": true
                })));
            }
            if script.contains("checkout-billingOptions-") {
                return Ok(js(json!({"count": 1, "found": true, "id": "ALIPAY", "checked": true, "options": ["支付宝"]})));
            }
            if script.contains("bag-item-") {
                let items: Vec<_> = self.items.iter().map(|(n, q)| json!({"name": n, "quantity": q})).collect();
                return Ok(js(json!({
                    "items": items, "total": "RMB 0.00",
                    "canCheckout": self.step == "Bag",
                    "canPlaceOrder": self.step == "Review"
                })));
            }
            if script.contains("signIn") {
                return Ok(js(json!({"step": self.step, "signIn": false})));
            }
            if script.contains(r"W\d{8,12}") {
                return Ok(js(json!({"step": self.step, "order": if self.step == "Thankyou" { "W0000000000" } else { "" }})));
            }
            if script.contains("role=\"alert\"") {
                return Ok(js(json!([])));
            }
            Err(format!("假页面不认识的脚本：{}", &script[..script.len().min(60)]))
        }
    }

    fn fixture_settings(place_order: bool) -> (Target, Settings) {
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R000".into(),
            store_title: "Apple 测试店".into(),
            part_number: "TEST1CH/A".into(),
            product_name: "测试商品 256GB".into(),
            companion_part: None,
            companion_name: None,
            kit_part: None,
            pickup_location: None,
        };
        let mut settings = Settings {
            targets: vec![target.clone()],
            pickup_last_name: "FixtureLast".into(),
            pickup_first_name: "FixtureFirst".into(),
            pickup_email: "pickup-fixture@example.invalid".into(),
            pickup_phone: "+86 000-0000-0000".into(),
            pickup_id_last4: "000x".into(),
            ..Default::default()
        };
        settings.checkout.enabled = true;
        settings.checkout.place_order = place_order;
        settings.normalize();
        (target, settings)
    }

    #[test]
    fn 计划里的联系人来自取货信息且只有一份来源() {
        let (target, settings) = fixture_settings(false);
        let plan = CheckoutPlan::new(&target, &settings, false);
        assert_eq!(plan.national_id_suffix, "000X");
        assert_eq!(
            plan.contact,
            vec![
                ("lastName", "FixtureLast".to_string()),
                ("firstName", "FixtureFirst".to_string()),
                ("emailAddress", "pickup-fixture@example.invalid".to_string()),
                ("fullDaytimePhone", "00000000000".to_string()),
            ]
        );
        assert_eq!(plan.monitored_stores, vec!["R000".to_string()]);
    }

    #[tokio::test]
    async fn 默认停在查看订单页且不点立即下单() {
        let (target, settings) = fixture_settings(false);
        let plan = CheckoutPlan::new(&target, &settings, settings.checkout.place_order);
        let runner = CheckoutRunner::new();
        let lease = runner.try_begin().unwrap();
        let mut page = FakePage::new(vec![("测试商品 256GB", "1")]);
        let result = runner.run(&lease, &mut page, &plan, &|_| {}).await;
        assert!(matches!(result, Ok(CheckoutOutcome::ReachedReview { .. })), "{result:?}");
        assert_eq!(page.clicked("continue-button-placeOrder"), 0);
        // 证件与联系人确实填进了页面脚本（取自取货信息）。
        assert!(page.scripts.iter().any(|s| s.contains("FixtureLast")));
        assert!(page.scripts.iter().any(|s| s.contains("000X")));
        // 成功一次后自动结账关闭。
        let next = settings_after_checkout(&settings, &result).expect("应当写回设置");
        assert!(!next.checkout.enabled);
    }

    #[tokio::test]
    async fn 开启自动下单时只点一次立即下单_成功后关闭开关() {
        let (target, settings) = fixture_settings(true);
        let plan = CheckoutPlan::new(&target, &settings, settings.checkout.place_order);
        let runner = CheckoutRunner::new();
        let lease = runner.try_begin().unwrap();
        let mut page = FakePage::new(vec![("测试商品 256GB", "1")]);
        let result = runner.run(&lease, &mut page, &plan, &|_| {}).await;
        match &result {
            Ok(CheckoutOutcome::OrderSubmitted { order_number, .. }) => {
                assert_eq!(order_number.as_deref(), Some("W0000000000"));
            }
            other => panic!("应当已提交订单：{other:?}"),
        }
        assert_eq!(page.clicked("continue-button-placeOrder"), 1);
        let next = settings_after_checkout(&settings, &result).expect("应当写回设置");
        assert!(!next.checkout.enabled);
        assert!(next.checkout.place_order, "只关总开关，不改用户的其他选择");
        // 关掉之后同一份设置不会再触发结账。
        assert!(!(next.checkout.enabled && next.is_checkout_target(&target)));
    }

    #[tokio::test]
    async fn 购物袋不是恰好一件到货商品时在购物袋页停下且不点结账() {
        let (target, settings) = fixture_settings(true);
        let plan = CheckoutPlan::new(&target, &settings, true);
        let runner = CheckoutRunner::new();
        let lease = runner.try_begin().unwrap();
        let mut page = FakePage::new(vec![("测试商品 256GB", "1"), ("别的商品", "1")]);
        let result = runner.run(&lease, &mut page, &plan, &|_| {}).await;
        let error = result.clone().unwrap_err();
        assert_eq!(error.stage, Stage::Bag);
        assert!(page.clicks.is_empty());
        // 普通失败保留开关，下次到货还能再试。
        assert!(settings_after_checkout(&settings, &result).is_none());
    }

    #[test]
    fn 结账只对设置里的那件商品触发_默认什么都不做() {
        let (target, settings) = fixture_settings(false);
        let mut other = target.clone();
        other.part_number = "OTHER1CH/A".into();
        other.product_name = "另一件商品".into();

        // 默认设置：不加购、不结账、不暂停。
        let defaults = Settings { targets: vec![target.clone()], ..Default::default() };
        assert_eq!(hit_actions(&defaults, &target), HitActions::default());

        let mut both = settings.clone();
        both.targets.push(other.clone());
        both.checkout.product = checkout_product_key(&target);
        both.auto_add_to_bag = true;
        both.normalize();
        assert!(both.checkout.enabled);
        let hit = hit_actions(&both, &target);
        assert!(hit.checkout && hit.add_to_bag && !hit.pause_monitoring);
        // 其他商品到货：不结账，也不往购物袋里塞会让结账停下的东西。
        assert_eq!(hit_actions(&both, &other), HitActions::default());

        // 没开自动结账时，自动加购保持原有行为：哪件到货加哪件。
        let mut bag_only = both.clone();
        bag_only.checkout.enabled = false;
        bag_only.stop_on_hit = true;
        let hit = hit_actions(&bag_only, &other);
        assert!(hit.add_to_bag && !hit.checkout && hit.pause_monitoring);
    }

    #[test]
    fn 联系信息不会出现在日志与推送文字里() {
        let (_, settings) = fixture_settings(false);
        let message = "页面提示：FixtureLastFixtureFirst pickup-fixture@example.invalid 00000000000 000X 已填";
        let redacted = redact_contact(message, &settings);
        for secret in ["FixtureLast", "FixtureFirst", "pickup-fixture@example.invalid", "00000000000", "000X"] {
            assert!(!redacted.contains(secret), "{secret} 没有被抹掉：{redacted}");
        }
        assert!(redacted.starts_with("页面提示："));
        // 单字值不替换，避免误伤门店名等正常文字。
        let mut single = settings.clone();
        single.pickup_last_name = "上".into();
        assert!(redact_contact("Apple 上海", &single).contains("上海"));
    }

    #[test]
    fn 提交订单这一步出错时也关闭开关() {
        let (_, settings) = fixture_settings(true);
        let failed = Err(CheckoutError::new(Stage::PlaceOrder, "超时"));
        assert!(!settings_after_checkout(&settings, &failed).unwrap().checkout.enabled);
        let failed = Err(CheckoutError::new(Stage::Billing, "没有该付款方式"));
        assert!(settings_after_checkout(&settings, &failed).is_none());
    }

    #[test]
    fn 同一时间只允许一次结账_租约析构后释放() {
        let runner = CheckoutRunner::new();
        let first = runner.try_begin().expect("第一次应当拿到");
        assert!(runner.is_busy());
        assert!(runner.try_begin().is_none(), "已有结账时不能再开始第二次");
        let clone = runner.clone();
        assert!(clone.try_begin().is_none(), "克隆共享同一个闸门");
        drop(first);
        assert!(!runner.is_busy());
        assert!(runner.try_begin().is_some());
    }

    #[tokio::test]
    async fn 退出叫停后不再开始_进行中的流程立即中止() {
        let (target, settings) = fixture_settings(false);
        let plan = CheckoutPlan::new(&target, &settings, false);
        let runner = CheckoutRunner::new();
        let lease = runner.try_begin().unwrap();
        runner.cancel();
        assert!(runner.try_begin().is_none());
        let mut page = FakePage::new(vec![("测试商品 256GB", "1")]);
        let started = Instant::now();
        let error = runner.run(&lease, &mut page, &plan, &|_| {}).await.unwrap_err();
        assert!(error.message.contains("正在退出"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(page.clicks.is_empty());
    }

    #[test]
    fn 缺证件后四位时拒绝开始() {
        let (target, mut settings) = fixture_settings(false);
        settings.pickup_id_last4 = "12".into();
        let plan = CheckoutPlan::new(&target, &settings, false);
        let runner = CheckoutRunner::new();
        let lease = runner.try_begin().unwrap();
        let mut page = FakePage::new(vec![("测试商品 256GB", "1")]);
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(runner.run(&lease, &mut page, &plan, &|_| {}))
            .unwrap_err();
        assert_eq!(error.stage, Stage::PickupContact);
        assert!(page.scripts.is_empty());
    }
}
