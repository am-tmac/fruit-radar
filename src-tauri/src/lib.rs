//! Tauri 应用外壳。
//!
//! 这一层只做装配和转译：起引擎、把命令转成引擎消息、把引擎事件转发给前端、
//! 在有货时发提醒。**所有业务判断都在 `apw-core` 里**，这里不许出现任何
//! 「什么算有货」之类的逻辑 —— 一旦让界面层参与判断，那条核心不变量就多了
//! 一处可以被绕开的地方。
//!
//! 提醒也刻意放在这一层而不是前端：托盘模式下窗口是隐藏的，WebView 可能被
//! 系统节流甚至挂起，把「及时提醒」挂在一个会被挂起的执行环境上是不能接受的。

use std::sync::RwLock;
use std::time::{Duration, Instant};

use apw_core::catalog::Catalog;
use apw_core::config::{ClashSettings, MIN_INTERVAL_SECONDS, OpenOnHit, Settings, SettingsStore};
use apw_core::model::{Category, Product, REGIONS, Store, Target, region_by_locale};
use apw_core::notify::{Bark, Multi, Notification, Notifier, Sound};
use apw_core::watcher::{Event, TargetState, Watcher, WatcherConfig};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_updater::UpdaterExt;

mod automation;
mod bag;
mod browser_support;
mod catalog_refresh;
mod checkout;
mod chromium_fetcher;
mod clash;
mod exit_barrier;
mod query_profile;
mod route_pool;
use checkout::{CheckoutOutcome, CheckoutPlan, CheckoutRunner};
use chromium_fetcher::AppleChromiumFetcher;

/// 前端事件通道名。前端用 `listen("watcher://event", ...)` 订阅。
const EVENT_CHANNEL: &str = "watcher://event";
/// 启动过程中的降级说明通道：配置读不出来之类的事必须让用户看见。
const NOTICE_CHANNEL: &str = "watcher://notice";
/// 后端自己改了设置（例如结账完成后关闭自动结账）时，把新设置推给界面。
const SETTINGS_CHANNEL: &str = "watcher://settings";

/// 地区的可序列化形式。
///
/// `model::Region` 的字段都是 `&'static str`，而且界面不需要知道 `base_url`
/// 这类内部细节。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegionDto {
    title: &'static str,
    locale: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DeliveryLocalityOption {
    text: String,
    value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryLocalitiesDto {
    states: Vec<DeliveryLocalityOption>,
    cities: Vec<DeliveryLocalityOption>,
    districts: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WatchBandChoiceDto {
    style_key: String,
    style_name: String,
    color_key: String,
    color_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WatchBandSizeDto {
    part_number: String,
    text: String,
}

#[derive(Debug, Deserialize, Default)]
struct DeliveryLocalityField {
    #[serde(default)]
    data: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Deserialize, Default)]
struct DeliveryLocalityBody {
    #[serde(default)]
    state: DeliveryLocalityField,
    #[serde(default)]
    city: DeliveryLocalityField,
    #[serde(default)]
    district: DeliveryLocalityField,
}

#[derive(Debug, Deserialize)]
struct DeliveryLocalityResponse {
    body: DeliveryLocalityBody,
}

fn usable_localities(field: DeliveryLocalityField) -> Vec<DeliveryLocalityOption> {
    field
        .data
        .into_iter()
        .filter(|option| !option.value.trim().is_empty())
        .collect()
}

fn parse_delivery_localities(body: &[u8]) -> Result<DeliveryLocalitiesDto, serde_json::Error> {
    let parsed: DeliveryLocalityResponse = serde_json::from_slice(body)?;
    Ok(DeliveryLocalitiesDto {
        states: usable_localities(parsed.body.state),
        cities: usable_localities(parsed.body.city),
        districts: usable_localities(parsed.body.district),
    })
}

fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_watch_band_choices(body: &[u8]) -> Result<Vec<WatchBandChoiceDto>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple 表带响应不是 JSON：{error}"))?;
    let items = value
        .pointer("/body/items")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Apple 表带响应缺少 body.items".to_string())?;
    let mut choices = Vec::new();
    for (style_key, style) in items {
        let style_name = plain_text(
            style
                .get("sectionHeader")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(style_key),
        );
        let style_order = style
            .get("sortOrder")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(i64::MAX);
        let Some(colors) = style
            .get("subDimensionValue")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for color in colors {
            let Some(color_key) = color
                .get("dimensionValue")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let color_name = color
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(plain_text)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| color_key.to_string());
            let color_order = color
                .get("sortOrder")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            choices.push((
                style_order,
                color_order,
                WatchBandChoiceDto {
                    style_key: style_key.clone(),
                    style_name: style_name.clone(),
                    color_key: color_key.to_string(),
                    color_name,
                },
            ));
        }
    }
    choices.sort_by_key(|(style_order, color_order, choice)| {
        (
            *style_order,
            *color_order,
            choice.style_key.clone(),
            choice.color_key.clone(),
        )
    });
    Ok(choices.into_iter().map(|(_, _, choice)| choice).collect())
}

fn parse_watch_band_sizes(body: &[u8]) -> Result<Vec<WatchBandSizeDto>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple 表带尺码响应不是 JSON：{error}"))?;
    let options = value
        .pointer("/body/options")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Apple 表带尺码响应缺少 body.options".to_string())?;
    let mut sizes = Vec::new();
    for option in options {
        let Some(part_number) = option
            .pointer("/product/options/watch_bands")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| value.contains('/'))
        else {
            continue;
        };
        let text = option
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(plain_text)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                option
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| part_number.to_string());
        sizes.push(WatchBandSizeDto {
            part_number: part_number.to_string(),
            text,
        });
    }
    Ok(sizes)
}

/// 品类的可序列化形式。
///
/// 界面上的品类下拉框由这里驱动，而不是在前端另抄一份常量：抄一份就迟早会有
/// 一边先加了品类、另一边还蒙在鼓里。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoryDto {
    value: Category,
    title: &'static str,
}

struct AppState {
    watcher: Watcher,
    fetcher: AppleChromiumFetcher,
    exit_barrier: exit_barrier::ExitBarrier,
    control: tokio::sync::Mutex<()>,
    admission: std::sync::Mutex<automation::Admission>,
    order_watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    orders: std::sync::Mutex<std::collections::HashSet<String>>,
    catalog: Catalog,
    http: reqwest::Client,
    /// 设置的内存副本。有可用的持久化存储时，只在新设置成功落盘后更新。
    settings: RwLock<Settings>,
    /// 为 `None` 表示配置不可持久化（目录不可写，或上次读取失败已放弃写盘）。
    store: Option<SettingsStore>,
    /// 可见的买家浏览器会话。`None` 表示还没打开过。
    ///
    /// 放在状态里而不是每次现开：购物袋是按这个窗口的 cookie 记的，
    /// 窗口一旦被换掉，之前加进去的东西就找不回来了。
    bag: tokio::sync::Mutex<Option<bag::BagSession>>,
    /// 本应用启动过的买家浏览器进程，退出屏障据此回收。
    buyers: std::sync::Arc<bag::BuyerRegistry>,
    /// 自动结账的单次执行闸门；会话本身仍是上面的买家窗口。
    checkout: CheckoutRunner,
    /// 「到货后暂停监控」触发的暂停。此时本轮已受理的自动加购/结账继续完成；
    /// 用户手动暂停或开始时清除。
    auto_paused: std::sync::atomic::AtomicBool,
}

impl AppState {
    /// 自动化任务是否仍应继续：监控在跑，或刚因「到货后暂停」自动停下。
    async fn automation_active(&self) -> bool {
        self.watcher.is_running().await
            || self.auto_paused.load(std::sync::atomic::Ordering::Acquire)
    }

    fn settings_snapshot(&self) -> Settings {
        self.settings
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 先落盘再更新内存副本。否则 Windows 上文件被占用或权限异常时，
    /// 界面、监控引擎和下次启动读到的设置会变成三份不同的状态。
    fn put_settings(&self, next: Settings) -> Result<(), String> {
        if let Some(store) = &self.store {
            store.save(&next).map_err(|e| e.to_string())?;
        }

        let mut guard = self.settings.write().unwrap_or_else(|e| e.into_inner());
        *guard = next;
        Ok(())
    }
}

#[tauri::command]
fn list_regions() -> Vec<RegionDto> {
    REGIONS
        .iter()
        .map(|r| RegionDto {
            title: r.title,
            locale: r.locale,
        })
        .collect()
}

#[tauri::command]
fn list_categories() -> Vec<CategoryDto> {
    Category::ALL
        .iter()
        .map(|c| CategoryDto {
            value: *c,
            title: c.title(),
        })
        .collect()
}

/// 读取 Apple 官网“送货选项”使用的省、市、区三级联动数据。
///
/// 中国大陆官网当前明确返回三个 select；其他地区多为邮编或定位输入，不能硬套
/// 省市区模型，因此先拒绝而不是给用户一组看似可选、实际查询无效的值。
#[tauri::command]
async fn list_delivery_localities(
    state: tauri::State<'_, AppState>,
    locale: String,
    state_name: String,
    city_name: String,
) -> Result<DeliveryLocalitiesDto, String> {
    if locale != "zh_CN" {
        return Err("Apple 官网当前仅在中国大陆站提供省、市、区三级选择".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let mut query = vec![("fae", "true")];
    let state_name = state_name.trim();
    let city_name = city_name.trim();
    if !state_name.is_empty() {
        query.push(("state", state_name));
    }
    if !city_name.is_empty() {
        query.push(("city", city_name));
    }

    let response = state
        .http
        .get(format!("{}/shop/address/locality-lookup", region.base_url))
        .query(&query)
        .timeout(Duration::from_secs(20))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/javascript, */*; q=0.01",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .map_err(|error| format!("读取 Apple 送货地区失败：{error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Apple 送货地区响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("Apple 送货地区接口返回 HTTP {}", status.as_u16()));
    }
    if body.len() > 512 * 1024 {
        return Err("Apple 送货地区响应超过 512 KiB 安全上限".into());
    }
    parse_delivery_localities(&body).map_err(|error| format!("Apple 送货地区响应无法解析：{error}"))
}

fn watch_band_product(state: &AppState, locale: &str, case_part: &str) -> Result<Product, String> {
    let mut product = state
        .catalog
        .product_by_part(locale, case_part)
        .ok_or_else(|| format!("型号目录中找不到 Apple Watch 表壳 {case_part}"))?;
    if product.category != Category::Watch {
        return Err(format!("{case_part} 不是 Apple Watch 表壳"));
    }
    if product.kit_part.as_deref().is_none_or(str::is_empty) {
        return Err("这条 Apple Watch 型号缺少整表套件号，请先刷新型号目录".into());
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        let words: Vec<_> = product.title.split_whitespace().collect();
        product.watch_case_size = words.windows(2).find_map(|pair| {
            (pair[1] == "毫米" && pair[0].chars().all(|ch| ch.is_ascii_digit()))
                .then(|| format!("{}mm", pair[0]))
        });
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        return Err("这条 Apple Watch 型号缺少表壳尺寸，请先刷新型号目录".into());
    }
    Ok(product)
}

async fn fetch_watch_band_api(
    state: &AppState,
    url: reqwest::Url,
    label: &str,
) -> Result<Vec<u8>, String> {
    let response = state
        .http
        .get(url)
        .timeout(Duration::from_secs(20))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/javascript, */*; q=0.01",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .map_err(|error| format!("读取 Apple {label}失败：{error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Apple {label}响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("Apple {label}接口返回 HTTP {}", status.as_u16()));
    }
    if body.len() > 2 * 1024 * 1024 {
        return Err(format!("Apple {label}响应超过 2 MiB 安全上限"));
    }
    Ok(body.to_vec())
}

/// 按表壳读取 Apple 官网当前允许搭配的表带款式和颜色。
#[tauri::command]
async fn list_watch_band_choices(
    state: tauri::State<'_, AppState>,
    locale: String,
    case_part: String,
) -> Result<Vec<WatchBandChoiceDto>, String> {
    if locale != "zh_CN" {
        return Err("Apple Watch 精确送货表带选择目前仅支持中国大陆站".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let product = watch_band_product(&state, &locale, &case_part)?;
    let kit = product
        .kit_part
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let case_size = product.watch_case_size.as_deref().unwrap_or_default();
    let mut url = reqwest::Url::parse(&format!("{}/shop/api/band-selection", region.base_url))
        .map_err(|error| format!("无法构造 Apple 表带接口：{error}"))?;
    url.query_pairs_mut()
        .append_pair("fae", "true")
        .append_pair("product", &kit)
        .append_pair("option.watch_cases", &product.part_number)
        .append_pair("dm.watch_cases-dimensionCaseSize", case_size);
    let body = fetch_watch_band_api(&state, url, "表带选项").await?;
    parse_watch_band_choices(&body)
}

/// 读取某一款式、颜色下的尺码；每个选项直接携带查询送货所需的表带零件号。
#[tauri::command]
async fn list_watch_band_sizes(
    state: tauri::State<'_, AppState>,
    locale: String,
    case_part: String,
    style_key: String,
    color_key: String,
) -> Result<Vec<WatchBandSizeDto>, String> {
    if locale != "zh_CN" {
        return Err("Apple Watch 精确送货表带选择目前仅支持中国大陆站".into());
    }
    if style_key.len() > 80 || color_key.len() > 80 {
        return Err("Apple 表带选项值过长".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let product = watch_band_product(&state, &locale, &case_part)?;
    let kit = product
        .kit_part
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let case_size = product.watch_case_size.as_deref().unwrap_or_default();
    let mut url = reqwest::Url::parse(&format!("{}/shop/api/band_sizes", region.base_url))
        .map_err(|error| format!("无法构造 Apple 表带尺码接口：{error}"))?;
    url.query_pairs_mut()
        .append_pair("fae", "true")
        .append_pair("product", &kit)
        .append_pair("option.watch_cases", &product.part_number)
        .append_pair("dm.watch_bands-dimensionBandStyle", style_key.trim())
        .append_pair("dm.watch_bands-dimensionColor", color_key.trim())
        .append_pair("dm.watch_cases-dimensionCaseSize", case_size);
    let body = fetch_watch_band_api(&state, url, "表带尺码").await?;
    parse_watch_band_sizes(&body)
}

#[tauri::command]
fn list_stores(state: tauri::State<'_, AppState>, locale: String) -> Result<Vec<Store>, String> {
    state.catalog.stores(&locale).map_err(|e| e.to_string())
}

/// 活动日志落盘用的文件名。
const ACTIVITY_LOG_FILE: &str = "activity.log";

/// 把界面上的活动日志追加到磁盘。
///
/// 界面那份只活在内存里，应用一关就没了。用户问「刚才那个 541 是什么时候出现的」
/// 时，没有落盘就只能靠回忆 —— 而这类问题恰恰最需要时间点。
#[tauri::command]
fn append_activity_log(lines: Vec<String>) {
    let Ok(dir) = apw_core::config::app_dir() else {
        return;
    };
    // 写不进去就算了：日志不该有机会干扰监控本身。具体滚动策略见
    // apw_core::activity_log，那里有单测盯着。
    let _ = apw_core::activity_log::append(&dir.join(ACTIVITY_LOG_FILE), &lines);
}

#[tauri::command]
fn list_products(
    state: tauri::State<'_, AppState>,
    locale: String,
) -> Result<Vec<Product>, String> {
    state.catalog.products(&locale).map_err(|e| e.to_string())
}

/// 手动刷新门店与当前品类型号；任一接口失败不影响另一部分生效。
#[tauri::command]
async fn refresh_products(
    state: tauri::State<'_, AppState>,
    locale: String,
    category: Option<Category>,
) -> Result<catalog_refresh::CatalogRefresh, String> {
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let result = catalog_refresh::refresh(&state.catalog, region, category, &state.http).await;

    // 抓取不占启停锁；仅同步元数据时串行化，避免覆盖同时编辑的设置。
    let _control = state.control.lock().await;
    let mut next = state.settings_snapshot();
    let before = next.targets.clone();
    state.catalog.hydrate_watch_targets(&mut next.targets);
    state.catalog.hydrate_store_targets(&mut next.targets);
    state.catalog.attach_pickup_locations(&mut next.targets);
    if next.targets != before {
        state.admission.lock().unwrap().cancel();
        state.put_settings(next.clone())?;
        state.watcher.set_targets(next.targets).await;
    }
    result
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings_snapshot()
}

#[tauri::command]
async fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let _control = state.control.lock().await;
    state.admission.lock().unwrap().cancel();
    // 自动结账的两个开关以后端为准，只能经 set_checkout_switches 修改：结账成功后
    // 后端已经关掉它们，界面那份旧副本整份保存时不能把它们重新打开。
    let mut next = checkout::merge_ui_settings(&state.settings_snapshot(), settings);
    next.normalize();
    state.catalog.hydrate_watch_targets(&mut next.targets);
    state.catalog.hydrate_store_targets(&mut next.targets);
    state.catalog.attach_pickup_locations(&mut next.targets);

    // 先成功落盘，再把同一份设置同步给引擎，避免三者分叉。
    state.put_settings(next.clone())?;
    state.watcher.set_targets(next.targets.clone()).await;
    state.watcher.set_interval(next.interval()).await;
    state
        .watcher
        .set_delivery_region(next.delivery_region.clone())
        .await;

    state.fetcher.set_network(next.network.clone()).await;
    apw_core::apple::Fetcher::set_backoff_enabled(&state.fetcher, next.backoff_enabled).await;
    Ok(next)
}

/// 用户在界面上切换「到货后自动结账」或「自动点立即下单」。
#[tauri::command]
async fn set_checkout_switches(
    app: AppHandle,
    enabled: Option<bool>,
    place_order: Option<bool>,
) -> Result<Settings, String> {
    let state = app.state::<AppState>();
    let _control = state.control.lock().await;
    let mut next = checkout::with_switches(&state.settings_snapshot(), enabled, place_order);
    next.normalize();
    state.put_settings(next.clone())?;
    let _ = app.emit(SETTINGS_CHANNEL, &next);
    Ok(next)
}

/// 发现新订单后关闭自动结账与自动下单并落盘、通知界面。
///
/// 订单可能是自动结账点出来的，也可能是用户在买家窗口里手动下的；只要已经有订单，
/// 再次到货时自动结账都可能重复下单，所以一律关掉。
async fn disarm_checkout_after_order(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let _control = state.control.lock().await;
    let current = state.settings_snapshot();
    if !current.checkout.enabled && !current.checkout.place_order {
        return;
    }
    let next = checkout::disarmed(&current);
    match state.put_settings(next.clone()) {
        Ok(()) => {
            let _ = app.emit(SETTINGS_CHANNEL, &next);
            let _ = app.emit(
                NOTICE_CHANNEL,
                "检测到新订单：自动结账与自动下单已关闭并保存，避免重复下单。",
            );
        }
        Err(err) => {
            let _ = app.emit(NOTICE_CHANNEL, format!("关闭自动结账开关失败：{err}"));
        }
    }
}

/// Clash 线路测试结果。只访问本机控制接口和端口，不向 Apple 发请求。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClashRouteCheck {
    version: String,
    #[serde(flatten)]
    group: clash::GroupNodes,
    /// 专用代理端口是否能在本机连上。
    port_open: bool,
    /// 批量测速超时的节点数（不含 DIRECT）。并发测速可能略微偏高。
    timeout_count: usize,
}

#[tauri::command]
async fn test_clash_route(clash: ClashSettings) -> Result<ClashRouteCheck, String> {
    let mut network = apw_core::config::NetworkSettings {
        mode: apw_core::config::NetworkMode::Clash,
        clash,
    };
    let mut settings = Settings {
        network: network.clone(),
        ..Settings::default()
    };
    settings.normalize();
    network = settings.network;
    let controller = clash::ClashController::new(&network.clash)?;
    let version = controller.version().await?;
    let group = controller
        .group_nodes(&network.clash.group, &network.clash.node_filter)
        .await?;
    let timeout_count = match controller.group_delays(&network.clash.group).await {
        Ok(delays) => group
            .nodes
            .iter()
            .filter(|node| node.as_str() != "DIRECT" && !delays.contains_key(*node))
            .count(),
        Err(_) => 0,
    };
    let port_open = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", network.clash.proxy_port)),
    )
    .await
    .is_ok_and(|connected| connected.is_ok());
    Ok(ClashRouteCheck {
        version,
        group,
        port_open,
        timeout_count,
    })
}

#[tauri::command]
async fn list_clash_nodes(
    state: tauri::State<'_, AppState>,
) -> Result<chromium_fetcher::RouteList, String> {
    state.fetcher.list_routes().await
}

#[tauri::command]
async fn select_clash_node(
    state: tauri::State<'_, AppState>,
    node: String,
) -> Result<Option<u32>, String> {
    state.fetcher.select_route(&node).await
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<Vec<TargetState>, String> {
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_targets(
    state: tauri::State<'_, AppState>,
    targets: Vec<Target>,
) -> Result<Vec<TargetState>, String> {
    let _control = state.control.lock().await;
    state.admission.lock().unwrap().cancel();
    let mut next = state.settings_snapshot();
    next.targets = targets;
    next.normalize();
    state.catalog.hydrate_watch_targets(&mut next.targets);
    state.catalog.hydrate_store_targets(&mut next.targets);
    state.catalog.attach_pickup_locations(&mut next.targets);
    state.put_settings(next.clone())?;
    state.watcher.set_targets(next.targets).await;
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_interval(state: tauri::State<'_, AppState>, seconds: u64) -> Result<u64, String> {
    let _control = state.control.lock().await;
    let secs = seconds.max(MIN_INTERVAL_SECONDS);
    let mut next = state.settings_snapshot();
    next.interval_seconds = secs;
    state.put_settings(next)?;
    state.watcher.set_interval(Duration::from_secs(secs)).await;
    Ok(secs)
}

#[tauri::command]
async fn start_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let _control = state.control.lock().await;
    if state.exit_barrier.is_exiting() {
        return Err("应用正在退出".into());
    }
    state
        .auto_paused
        .store(false, std::sync::atomic::Ordering::Release);
    state.watcher.start().await;
    remember_running(state.watcher.is_running().await);
    Ok(())
}

#[tauri::command]
async fn stop_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let _control = state.control.lock().await;
    state.admission.lock().unwrap().cancel();
    state
        .auto_paused
        .store(false, std::sync::atomic::Ordering::Release);
    state.watcher.stop().await;
    remember_running(state.watcher.is_running().await);
    Ok(())
}

/// 保留已有运行状态记录兼容性；启动不再读取它，始终等待用户手动开始。
/// 写入失败不影响启停命令。
fn remember_running(running: bool) {
    if let Ok(path) = apw_core::config::resume_state_path() {
        let _ = apw_core::resume::write(&path, running);
    }
}

#[tauri::command]
async fn retry_watching_now(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.watcher.retry_now().await)
}

#[tauri::command]
async fn is_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.watcher.is_running().await)
}

/// 一个待安装的更新。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

/// 查询有没有新版本。返回 `None` 表示已经是最新。
///
/// **刻意不做静默自动安装**：给用户装东西这件事应该由用户点头。何况这是个会在
/// 抢购当口挂着的程序，自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(20), updater.check())
        .await
        .map_err(|_| "检查更新超时，请稍后重试".to_string())?
    {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        // 检查更新失败不是错误状态，只是这次没查到 —— 网络不通、GitHub 抽风都
        // 会走到这里，没必要弹给用户看，写进日志即可。
        Err(err) => Err(err.to_string()),
    }
}

/// 下载并安装更新。安装完成后需要重启应用才生效。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let update = tokio::time::timeout(Duration::from_secs(20), updater.check())
        .await
        .map_err(|_| "检查更新超时，请稍后重试".to_string())?
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "已经是最新版本".to_string())?;

    let handle = app.clone();
    let finished = app.clone();
    let mut downloaded = 0_u64;
    let mut last_emit = std::time::Instant::now() - Duration::from_secs(1);
    let _ = app.emit(
        "watcher://update-progress",
        serde_json::json!({
            "phase": "downloading", "downloaded": 0, "total": null
        }),
    );
    let bytes = update
        .download(
            move |chunk, total| {
                // 插件回调给的是本次数据块大小，界面需要累计字节数。
                downloaded = downloaded.saturating_add(chunk as u64);
                if last_emit.elapsed() >= Duration::from_millis(100) || total == Some(downloaded) {
                    let _ = handle.emit(
                        "watcher://update-progress",
                        serde_json::json!({
                            "phase": "downloading", "downloaded": downloaded, "total": total
                        }),
                    );
                    last_emit = std::time::Instant::now();
                }
            },
            move || {
                let _ = finished.emit(
                    "watcher://update-progress",
                    serde_json::json!({
                        "phase": "verifying", "downloaded": 0, "total": null
                    }),
                );
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    // download 完成签名验证后才允许安装，校验失败绝不进入此分支。
    let _ = app.emit(
        "watcher://update-progress",
        serde_json::json!({
            "phase": "installing", "downloaded": 0, "total": null
        }),
    );
    tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 与库存目标共用目录信息，避免 Apple Watch 的表壳料号跳到不存在的详情页。
fn target_purchase_url(app: &AppHandle, target: &Target) -> Option<String> {
    let product = app.try_state::<AppState>().and_then(|state| {
        state
            .catalog
            .product_by_part(&target.locale, &target.part_number)
    });
    target.purchase_url(product.as_ref())
}

/// 按用户选择返回这个监控目标对应的跳转地址。
fn target_open_url(app: &AppHandle, target: &Target, destination: OpenOnHit) -> Option<String> {
    match destination {
        OpenOnHit::None => None,
        OpenOnHit::Bag => region_by_locale(&target.locale).map(|region| region.bag_url()),
        OpenOnHit::Product => target_purchase_url(app, target),
    }
}

fn destination_title(destination: OpenOnHit) -> &'static str {
    match destination {
        OpenOnHit::None => "",
        OpenOnHit::Bag => "购物袋",
        OpenOnHit::Product => "商品页",
    }
}

#[tauri::command]
fn open_target_product(app: AppHandle, target: Target) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = target_purchase_url(&app, &target).ok_or("无法识别目标地区")?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 买家浏览器（自动加购用）的持久数据目录名。
///
/// 和库存查询的一次性临时目录刻意分开：查询会话随时可以丢，而这个目录
/// 装着用户的登录态和购物袋，必须长期保留。
const BUYER_PROFILE_DIR: &str = "chrome-buyer";

/// 在买家窗口里完成一次加购全流程。
async fn add_target_to_bag(
    app: &AppHandle,
    target: &Target,
    ticket: Option<automation::Ticket>,
) -> Result<bag::BagOutcome, String> {
    let state = app.state::<AppState>();
    let mut guard = state.bag.lock().await;
    add_target_to_bag_locked(app, target, ticket, &mut guard).await
}

/// 同上，调用方已持有买家会话锁（到货后「加购 → 结账」在同一次持锁里完成）。
async fn add_target_to_bag_locked(
    app: &AppHandle,
    target: &Target,
    ticket: Option<automation::Ticket>,
    guard: &mut Option<bag::BagSession>,
) -> Result<bag::BagOutcome, String> {
    let region = region_by_locale(&target.locale).ok_or("无法识别目标地区")?;
    let product_url = target_purchase_url(app, target).ok_or("无法生成该商品的购买页地址")?;
    let bag_url = region.bag_url();
    let profile = apw_core::config::runtime_dir(BUYER_PROFILE_DIR).map_err(|e| e.to_string())?;

    let state = app.state::<AppState>();
    let settings = state.settings_snapshot();
    if let Some(ticket) = &ticket {
        if !state.admission.lock().unwrap().valid(ticket)
            || !settings.auto_add_to_bag
            || !settings.targets.iter().any(|t| {
                t.locale == target.locale
                    && t.part_number == target.part_number
                    && t.store_number == target.store_number
            })
            || !state.automation_active().await
        {
            return Err("自动加购任务已取消或过期".into());
        }
    }
    let session = bag::ensure_session(guard, &profile, &state.buyers).await?;
    // 自动加购这一步仍然不进入结账/订单页；结账由 checkout 模块在加购完成后接手。
    bag::ensure_no_checkout_pages(&profile).await?;
    // Never navigate a checkout/order page, including manual retries.
    if !automation::safe_to_navigate(&session.current_url().await?) {
        return Err("买家窗口已进入结账或订单页面；请手动操作".into());
    }
    // Serialize the final running/settings check with pause and settings writes.
    // No await between commit and the first Page.navigate poll; once committed,
    // cancellation or an uncertain CDP result must continue to deduplicate.
    let control = state.control.lock().await;
    let settings = state.settings_snapshot();
    if let Some(ticket) = &ticket {
        if !settings.auto_add_to_bag
            || !settings.targets.iter().any(|t| {
                t.locale == target.locale
                    && t.part_number == target.part_number
                    && t.store_number == target.store_number
            })
            || !state.automation_active().await
            || !state.admission.lock().unwrap().commit(ticket)
        {
            return Err("自动加购任务已取消或过期".into());
        }
    }
    drop(control);
    let result = session
        .add_to_bag(&product_url, &bag_url, settings.bag_applecare)
        .await;
    match result {
        Ok(outcome) => {
            session.focus();
            Ok(outcome)
        }
        Err(err) => {
            // 会话可能已经不可用（用户把窗口关了，或者连接断了）。丢掉它，
            // 下次命中重新建一个 —— 留着一个坏会话只会一直失败下去。
            *guard = None;
            Err(err)
        }
    }
}

/// 打开买家浏览器窗口，供用户登录 Apple 账号并确认购物袋。
///
/// 自动加购发生在**这个窗口自己的 cookie 会话**里。用户不在这里登录，
/// 加购之后的结账页就还是未登录状态 —— 那就等于把最省事的一步留在了最
/// 费事的位置。所以这一步必须由用户主动做一次，程序不能代劳。
#[tauri::command]
async fn open_buyer_window(app: AppHandle) -> Result<String, String> {
    let profile = apw_core::config::runtime_dir(BUYER_PROFILE_DIR).map_err(|e| e.to_string())?;
    let target_url = settings_bag_url(&app);
    {
        let state = app.state::<AppState>();
        let mut guard = state.bag.lock().await;
        // 连接在探活之后才断开（例如探活与导航之间窗口被关）时，丢掉它再试一次。
        for attempt in 0..2 {
            let session = bag::ensure_session(&mut guard, &profile, &state.buyers).await?;
            let navigated = match target_url.as_deref() {
                Some(url) => session.navigate(url).await,
                None => Ok(()),
            };
            match navigated {
                Ok(()) => {
                    session.focus();
                    break;
                }
                Err(err) if attempt == 0 && bag::connection_lost(&err) => *guard = None,
                Err(err) => return Err(err),
            }
        }
    }
    Ok(profile.display().to_string())
}

/// 关闭买家浏览器窗口。
#[tauri::command]
async fn close_buyer_window(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut guard = state.bag.lock().await;
    if let Some(session) = guard.as_mut() {
        session.shutdown();
    }
    *guard = None;
    Ok(())
}

/// 手动试一次自动加购，用于在真的开售之前验证整条链路。
#[tauri::command]
async fn add_to_bag_now(app: AppHandle, target: Target) -> Result<String, String> {
    let outcome = add_target_to_bag(&app, &target, None).await?;
    // 手动试加购之后用户同样会去结账、下单，所以订单监听也要起 ——
    // 只在「真检测到有货」那条路上起监听的话，手工演练就永远测不到订单推送。
    watch_for_new_order(app.clone());
    Ok(format!(
        "{}；已停在 {}；接下来请手动结账并下单，订单确认后会自动推送到手机",
        outcome.summary, outcome.url
    ))
}

/// 把设置里的取货人信息填进当前打开的结账页。
///
/// Apple **不会**从账号预填取货联系人（实测登录状态下五栏全空），而这一步
/// 每次都要手打。程序只填这五项，**不点提交、不碰支付**，填完由用户核对。
#[tauri::command]
async fn fill_pickup_info(app: AppHandle) -> Result<String, String> {
    let settings = app
        .try_state::<AppState>()
        .map(|state| state.settings_snapshot())
        .unwrap_or_default();
    let info = bag::PickupInfo {
        last_name: settings.pickup_last_name.clone(),
        first_name: settings.pickup_first_name.clone(),
        email: settings.pickup_email.clone(),
        phone: settings.pickup_phone.clone(),
        id_last4: settings.pickup_id_last4.clone(),
    };
    let missing = info.missing();
    if !missing.is_empty() {
        return Err(format!("请先在设置里填写取货信息：{}", missing.join("、")));
    }

    let profile = apw_core::config::runtime_dir(BUYER_PROFILE_DIR).map_err(|e| e.to_string())?;
    let state = app.state::<AppState>();
    let mut guard = state.bag.lock().await;
    let session = bag::ensure_session(&mut guard, &profile, &state.buyers).await?;
    match session.fill_pickup_info(&info).await {
        Ok(summary) => {
            session.focus();
            Ok(summary)
        }
        Err(err) => {
            // 会话可能已经不可用，丢掉它，下次重新接。
            *guard = None;
            Err(err)
        }
    }
}

/// 下单后盯住订单确认页的时长。
///
/// 订单创建后是**待付款**状态，30 分钟不付自动取消；这段时间用户很可能已经
/// 离开电脑，所以推送必须发出去。
const ORDER_WATCH_SECONDS: u64 = 40 * 60;

/// 后台盯一会儿买家窗口，发现订单确认页就把订单号推到手机。
///
/// 不放在加购流程里同步等：用户从加购到真正点「立即下单」中间可能隔着几分钟，
/// 把流程卡在那里既没有意义，也会挡住别的提醒。
fn watch_for_new_order(app: AppHandle) {
    let Some(lease) = automation::OrderLease::acquire(app.state::<AppState>().order_watch.clone())
    else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let _lease = lease;
        let deadline = Instant::now() + Duration::from_secs(ORDER_WATCH_SECONDS);
        while Instant::now() < deadline {
            match read_pending_order(&app).await {
                Ok(Some(order)) => {
                    let fresh = app
                        .state::<AppState>()
                        .orders
                        .lock()
                        .unwrap()
                        .insert(order.clone());
                    if fresh {
                        disarm_checkout_after_order(&app).await;
                        for attempt in 0..3 {
                            if notify_order_created(&app, &order).await {
                                break;
                            }
                            if attempt < 2 {
                                tokio::time::sleep(Duration::from_secs(4)).await;
                            }
                        }
                    }
                    // Keep the one listener alive for subsequent orders.
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = app.emit(NOTICE_CHANNEL, format!("读取订单状态失败：{err}"));
                    return;
                }
            }
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
}

async fn read_pending_order(app: &AppHandle) -> Result<Option<String>, String> {
    let profile = apw_core::config::runtime_dir(BUYER_PROFILE_DIR).map_err(|e| e.to_string())?;
    // 没有买家窗口就说明用户还没走到下单这一步，不必现开一个窗口去看。
    if crate::bag::live_port_of(&profile).await.is_none() {
        return Ok(None);
    }
    let state = app.state::<AppState>();
    let mut guard = state.bag.lock().await;
    for attempt in 0..2 {
        let session = bag::ensure_session(&mut guard, &profile, &state.buyers).await?;
        match session.detect_order().await {
            Err(err) if attempt == 0 && bag::connection_lost(&err) => *guard = None,
            other => return other,
        }
    }
    Ok(None)
}

async fn notify_order_created(app: &AppHandle, order: &str) -> bool {
    let settings = app
        .try_state::<AppState>()
        .map(|state| state.settings_snapshot())
        .unwrap_or_default();
    // 订单页链接用地区站点拼；`/shop/order/list` 在 Apple 的域名关联文件里
    // 属于被 Apple Store App 接管的路径，手机上点开会直接进 App 的订单页。
    let url = region_by_locale(&settings.locale)
        .map(|region| format!("{}/shop/order/list", region.base_url))
        .unwrap_or_else(|| "https://www.apple.com.cn/shop/order/list".to_owned());

    let mut notification = Notification::new(
        "订单已创建 · 待付款",
        format!("{order} · 30 分钟内有效。点这里打开订单页付款"),
    );
    notification = notification.with_url(url.clone());
    let bark_url = settings.bark_url.clone();
    match dispatch_notification(app, notification, &bark_url).await {
        Ok(()) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("订单 {order} 已创建，提醒已推送（{url}）"),
            );
            true
        }
        Err(err) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("订单 {order} 已创建，但发送提醒失败：{err}"),
            );
            false
        }
    }
}

/// 当前设置下购物袋页面的地址。
fn settings_bag_url(app: &AppHandle) -> Option<String> {
    let settings = app.try_state::<AppState>()?.settings_snapshot();
    let locale = settings
        .targets
        .first()
        .map_or(settings.locale.as_str(), |target| target.locale.as_str());
    region_by_locale(locale).map(|region| region.bag_url())
}

/// 手动测试提醒和首个目标的跳转，便于提前验证实际操作链路。
#[tauri::command]
async fn test_notify(app: AppHandle) -> Result<(), String> {
    let settings = app.state::<AppState>().settings_snapshot();
    let bark_url = settings.targets.first().map_or_else(
        || settings.bark_url.as_str(),
        |target| settings.bark_url_for(target),
    );
    if !settings.sound_enabled
        && bark_url.trim().is_empty()
        && settings.open_on_hit == OpenOnHit::None
    {
        return Err("请先开启提示音、页面跳转或配置 Bark，再测试提醒".into());
    }
    let mut notification = Notification::new(
        "提醒测试（不代表有货）",
        "请确认已开启的提醒是否收到；Apple 页面仍需自行结账",
    );
    if settings.open_on_hit == OpenOnHit::Product
        && settings.targets.is_empty()
        && !settings.sound_enabled
        && bark_url.trim().is_empty()
    {
        return Err("请先添加监控目标，再测试商品页跳转".into());
    }

    let jump_url = match settings.open_on_hit {
        OpenOnHit::None => None,
        OpenOnHit::Bag => {
            let locale = settings
                .targets
                .first()
                .map_or(settings.locale.as_str(), |target| target.locale.as_str());
            region_by_locale(locale).map(|region| region.bag_url())
        }
        OpenOnHit::Product => settings
            .targets
            .first()
            .and_then(|target| target_purchase_url(&app, target)),
    };

    if settings.open_on_hit != OpenOnHit::None
        && jump_url.is_none()
        && !settings.sound_enabled
        && bark_url.trim().is_empty()
    {
        return Err(format!(
            "无法生成{}跳转地址",
            destination_title(settings.open_on_hit)
        ));
    }

    if let Some(url) = jump_url {
        notification = notification.with_url(url.clone());
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| e.to_string())?;
    }
    dispatch_notification(&app, notification, bark_url)
        .await
        .map_err(|e| e.to_string())
}

/// 按用户设置发送提示音和 Bark 提醒。
async fn dispatch_notification(
    app: &AppHandle,
    notification: Notification,
    bark_url: &str,
) -> Result<(), apw_core::notify::NotifyError> {
    let settings = match app.try_state::<AppState>() {
        Some(state) => state.settings_snapshot(),
        None => return Ok(()),
    };

    let mut channels = Multi::new();
    if settings.sound_enabled {
        channels.push(Sound::embedded());
    }
    if !bark_url.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // Bark 每次现构造：地址是用户随时可改的设置项，缓存实例会在改完地址后
        // 继续往旧地址推。共享的 http 客户端一并传进去，连接池仍然复用。
        channels.push(Bark::new(bark_url.to_owned(), http));
    }
    if channels.is_empty() {
        return Ok(());
    }
    channels.notify(&notification).await
}

fn in_stock_notification_body(target: &Target) -> String {
    let mut body = format!("{} {}", target.store_title, target.product_name);
    if let Some(companion) = target
        .companion_part
        .as_deref()
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let label = target
            .companion_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some(label) = label {
            body.push_str(&format!("\nWatch 送货表带 {label} [{companion}]"));
        } else {
            body.push_str(&format!("\nWatch 送货搭配表带 {companion}"));
        }
    }
    body
}

/// 在买家窗口里执行一次结账。调用方须已持有结账租约。
///
/// 与自动加购、代填、订单监听共用 `AppState.bag` 这把锁：同一个资料目录同一时间
/// 只有一个浏览器、一条 DevTools 连接在操作。
async fn checkout_in_buyer_window(
    app: &AppHandle,
    lease: &checkout::CheckoutLease,
    target: &Target,
    place_order: bool,
) -> Result<CheckoutOutcome, checkout::CheckoutError> {
    let state = app.state::<AppState>();
    let mut guard = state.bag.lock().await;
    checkout_in_buyer_window_locked(app, lease, target, place_order, &mut guard).await
}

/// 同上，调用方已持有买家会话锁。
async fn checkout_in_buyer_window_locked(
    app: &AppHandle,
    lease: &checkout::CheckoutLease,
    target: &Target,
    place_order: bool,
    guard: &mut Option<bag::BagSession>,
) -> Result<CheckoutOutcome, checkout::CheckoutError> {
    let browser_error =
        |message: String| checkout::CheckoutError { stage: checkout::Stage::Browser, message };
    let state = app.state::<AppState>();
    let settings = state.settings_snapshot();
    let plan = CheckoutPlan::new(target, &settings, &state.catalog, place_order);
    let profile = apw_core::config::runtime_dir(BUYER_PROFILE_DIR)
        .map_err(|e| browser_error(e.to_string()))?;
    // 订单页已经在买家窗口里时绝不重来：订单可能已经生成。
    bag::ensure_no_order_pages(&profile)
        .await
        .map_err(browser_error)?;
    let session = bag::ensure_session(guard, &profile, &state.buyers)
        .await
        .map_err(browser_error)?;
    let progress = {
        let app = app.clone();
        let settings = settings.clone();
        move |message: String| {
            let _ = app.emit(NOTICE_CHANNEL, checkout::redact_contact(&message, &settings));
        }
    };
    let result = state.checkout.run(lease, session, &plan, &progress).await;
    if session.current_url().await.is_err() {
        // 连接已经断了（窗口被关、进程退出），下次重新接。
        *guard = None;
    } else if result.as_ref().is_err_and(|error| bag::connection_lost(&error.message)) {
        *guard = None;
    } else {
        session.focus();
    }
    result
}

/// 结账结束：按需关闭自动结账、通知界面、推送提醒并开始盯订单。
async fn finish_checkout(
    app: &AppHandle,
    target: &Target,
    result: Result<CheckoutOutcome, checkout::CheckoutError>,
) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    {
        // 与保存设置串行，避免和界面上同时进行的修改互相覆盖。
        let _control = state.control.lock().await;
        let current = state.settings_snapshot();
        if let Some(next) = checkout::settings_after_checkout(&current, &result) {
            match state.put_settings(next.clone()) {
                Ok(()) => {
                    let _ = app.emit(SETTINGS_CHANNEL, &next);
                    let _ = app.emit(
                        NOTICE_CHANNEL,
                        "自动结账与自动下单已关闭并保存；需要再次使用时请手动打开。",
                    );
                }
                Err(err) => {
                    let _ = app.emit(NOTICE_CHANNEL, format!("关闭自动结账开关失败：{err}"));
                }
            }
        }
    }
    let settings = state.settings_snapshot();
    let bark_url = settings.bark_url_for(target).to_owned();
    let (title, body) = match &result {
        Ok(CheckoutOutcome::ReachedReview { store, total }) => (
            "已到查看订单页",
            format!(
                "{store} {}，总计 {total}。请在买家窗口里点「立即下单」并付款。",
                target.product_name
            ),
        ),
        Ok(CheckoutOutcome::OrderSubmitted {
            store,
            order_number: Some(order),
            total,
        }) => (
            "已提交订单，请尽快付款",
            format!(
                "{store} {}，总计 {total}，订单 {order}。请在 30 分钟内自己完成付款。",
                target.product_name
            ),
        ),
        Ok(CheckoutOutcome::OrderSubmitted {
            store,
            order_number: None,
            total,
        }) => (
            "已点立即下单，请确认结果",
            format!(
                "{store} {}，总计 {total}。页面上没有读到订单号，请在买家窗口里确认订单是否成立。",
                target.product_name
            ),
        ),
        Err(error) if error.stage == checkout::Stage::PlaceOrder => (
            "提交订单时出错，请确认结果",
            format!("{error}。订单可能已经生成，请在买家窗口里确认。自动结账已关闭。"),
        ),
        Err(error) => (
            "自动结账没有完成",
            format!("{error}。请在买家窗口里手动继续。"),
        ),
    };
    let body = checkout::redact_contact(&body, &settings);
    let _ = app.emit(NOTICE_CHANNEL, format!("{title}：{body}"));
    if result.is_ok() {
        // 用户接下来要下单/付款；订单确认页出现时推送订单号。
        watch_for_new_order(app.clone());
    }
    if let Err(err) = dispatch_notification(app, Notification::new(title, body), &bark_url).await {
        let _ = app.emit(NOTICE_CHANNEL, format!("发送结账提醒时出错：{err}"));
    }
}

/// 到货后：（可选）先自动加购，再（可选）自动结账。
///
/// 两步在**同一次**持有买家会话锁期间依次执行（[`automation::chain_hit`]）：结账
/// 只在加购完成之后开始，中间也不会被订单监听、打开窗口或试跑插进来换页面。
async fn automate_hit(
    app: AppHandle,
    target: Target,
    actions: checkout::HitActions,
    ticket: Option<automation::Ticket>,
    lease: Option<checkout::CheckoutLease>,
) {
    let state = app.state::<AppState>();
    let checkout_wanted = lease.is_some() && actions.checkout;
    // 这件商品本次运行里已经加购过（去重）：不再重复加，由结账在购物袋页逐项核对。
    let already_attempted = ticket.is_none()
        && actions.add_to_bag
        && state
            .admission
            .lock()
            .unwrap()
            .attempted(&target.locale, &target.part_number);
    let add = ticket.map(|ticket| {
        let (app, target) = (&app, &target);
        async move |guard: &mut Option<bag::BagSession>| {
            add_target_to_bag_locked(app, target, Some(ticket), guard).await
        }
    });
    let run_checkout = (checkout_wanted && lease.is_some()).then(|| {
        let (app, target, lease) = (&app, &target, lease.as_ref());
        async move |guard: &mut Option<bag::BagSession>, step: &automation::BagStep| {
            let lease = lease.expect("checkout lease");
            let state = app.state::<AppState>();
            let settings = state.settings_snapshot();
            // 加购期间用户可能暂停了监控、关掉了自动结账或改了要下单的商品。
            if !settings.checkout.enabled
                || !settings.is_checkout_target(target)
                || !state.automation_active().await
                || state.exit_barrier.is_exiting()
            {
                return Err("自动结账已取消：设置或监控状态已变化。".to_string());
            }
            if !settings.has_pickup_id_suffix() {
                return Err(
                    "自动结账未执行：还没有在「取货信息」里填写完整的证件号码后四位。".to_string(),
                );
            }
            if *step == automation::BagStep::AlreadyAttempted {
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    "这件商品本次已自动加购过，不再重复加购；自动结账会先核对购物袋里恰好是它。",
                );
            }
            Ok(checkout_in_buyer_window_locked(
                app,
                lease,
                target,
                settings.checkout.place_order,
                guard,
            )
            .await)
        }
    });
    let (step, checkout_result, added) =
        automation::chain_hit(&state.bag, add, already_attempted, run_checkout).await;

    match (&step, added) {
        (automation::BagStep::Added, Some(outcome)) => {
            if !checkout_wanted {
                // 不结账时沿用原来的做法：用户接着自己结账，从这里开始盯订单。
                watch_for_new_order(app.clone());
            }
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!(
                    "自动加购完成：{} {}（{}）",
                    target.store_title, target.product_name, outcome.summary
                ),
            );
        }
        (automation::BagStep::Failed(err), _) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!(
                    "自动加购失败：{} {} —— {err}",
                    target.store_title, target.product_name
                ),
            );
            if checkout_wanted {
                let _ = app.emit(NOTICE_CHANNEL, "自动结账未执行：本次自动加购没有完成。");
            }
        }
        _ => {}
    }
    drop(lease);
    match checkout_result {
        Some(Ok(result)) => finish_checkout(&app, &target, result).await,
        Some(Err(notice)) => {
            let _ = app.emit(NOTICE_CHANNEL, notice);
        }
        None => {}
    }
}

/// 试跑自动结账，走到「查看订单」页为止，无论设置如何都不提交订单。
#[tauri::command]
async fn test_checkout(app: AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    if state.exit_barrier.is_exiting() {
        return Err("应用正在退出".into());
    }
    let settings = state.settings_snapshot();
    if !settings.has_pickup_id_suffix() {
        return Err("请先在「取货信息」里填写证件号码后四位".into());
    }
    let target = settings
        .targets
        .iter()
        .find(|target| settings.is_checkout_target(target))
        .cloned()
        .ok_or("试跑需要先确定要下单的商品：监控列表里要有它的中国大陆监控项，有多件商品时请在自动结账里选一件")?;
    let lease = state
        .checkout
        .try_begin()
        .ok_or("已有一次结账正在进行")?;
    match checkout_in_buyer_window(&app, &lease, &target, false).await {
        Ok(CheckoutOutcome::ReachedReview { store, total }) => Ok(format!(
            "试跑完成：{store} {} 已到「查看订单」页（总计 {total}），没有提交订单。",
            target.product_name
        )),
        Ok(CheckoutOutcome::OrderSubmitted { .. }) => {
            Err("试跑不应提交订单，请立即在买家窗口里检查".into())
        }
        Err(error) => Err(checkout::redact_contact(&error.to_string(), &settings)),
    }
}

/// 「到货后暂停监控」：本轮处理完后停下监控，但保留本轮已受理的自动加购/结账。
///
/// 返回是否真的执行了暂停。与启停命令共用 control 锁；退出中不做任何事。
async fn pause_after_hit(
    watcher: &Watcher,
    control: &tokio::sync::Mutex<()>,
    auto_paused: &std::sync::atomic::AtomicBool,
    exit_barrier: &exit_barrier::ExitBarrier,
) -> bool {
    let _control = control.lock().await;
    if exit_barrier.is_exiting() || !watcher.is_running().await {
        return false;
    }
    auto_paused.store(true, std::sync::atomic::Ordering::Release);
    watcher.stop().await;
    true
}

/// 退出时回收自动化：叫停结账、不再接受新的买家浏览器、回收本应用启动的进程。
///
/// 不等买家会话锁：结账或加购可能正占着它等页面，退出不能被它卡住。进程被回收后
/// 那条 DevTools 连接随之断开，流程会在下一次读页面时停下。
async fn shutdown_automation(checkout: &CheckoutRunner, buyers: &std::sync::Arc<bag::BuyerRegistry>) {
    checkout.cancel();
    let buyers = buyers.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || buyers.close_and_terminate()).await;
}

/// 消费引擎事件：转发给前端，并在有货时发提醒。
async fn pump_events(app: AppHandle, mut events: tokio::sync::mpsc::Receiver<Event>) {
    while let Some(event) = events.recv().await {
        // 先原样转发。前端拿到的事件流应当与引擎发出的完全一致，
        // 中间少一层可能出错的翻译。
        let _ = app.emit(EVENT_CHANNEL, &event);

        if let Event::InStock { state } = &event {
            let target = &state.target;
            let settings = app
                .try_state::<AppState>()
                .map(|s| s.settings_snapshot())
                .unwrap_or_default();
            let destination_url = target_open_url(&app, target, settings.open_on_hit);
            let bark_url = settings.bark_url_for(target).to_owned();
            let has_product_bark = settings.product_bark_urls.contains_key(&target.part_number);
            let mut notification = Notification::new("有货了", in_stock_notification_body(target));
            if let Some(url) = &destination_url {
                notification = notification.with_url(url.clone());
            }

            let mut opened_destination = None;
            let hit_actions = checkout::hit_actions(&settings, target);
            if settings.stop_on_hit {
                // 放到独立任务里停：引擎可能正等着往这个事件通道里发下一条事件，
                // 在这里原地等它停下会互相卡住。本轮已经发出的到货事件仍会照常处理。
                let pause_app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = pause_app.state::<AppState>();
                    if pause_after_hit(
                        &state.watcher,
                        &state.control,
                        &state.auto_paused,
                        &state.exit_barrier,
                    )
                    .await
                    {
                        remember_running(false);
                        let _ = pause_app.emit(NOTICE_CHANNEL, "监控到有货，已按设置暂停监控。");
                    }
                });
            }
            if settings.auto_add_to_bag || hit_actions.checkout {
                let state = app.state::<AppState>();
                // 结账租约在派发前拿：同一轮同一商品多家门店到货时只跑一次结账。
                let lease = if hit_actions.checkout { state.checkout.try_begin() } else { None };
                let ticket = if hit_actions.add_to_bag {
                    state
                        .admission
                        .lock()
                        .unwrap()
                        .admit(&target.locale, &target.part_number)
                } else {
                    None
                };
                // 同一件商品已有加购在排队：由那个任务接着结账，这里不再重复。
                let bag_in_flight = hit_actions.add_to_bag
                    && ticket.is_none()
                    && state
                        .admission
                        .lock()
                        .unwrap()
                        .is_pending(&target.locale, &target.part_number);
                // 要求加购却既没受理、也没加购过（例如受理名额已满）：不能结账，
                // 否则结账会在加购之前、对着一个不确定的购物袋开始。
                let bag_unknown = hit_actions.add_to_bag
                    && ticket.is_none()
                    && !state
                        .admission
                        .lock()
                        .unwrap()
                        .attempted(&target.locale, &target.part_number);
                let lease = if bag_in_flight || bag_unknown { None } else { lease };
                if ticket.is_some() || lease.is_some() {
                    // 加购和结账要等页面加载、走完整段流程，放进后台任务，
                    // 不能堵住事件泵。
                    tauri::async_runtime::spawn(automate_hit(
                        app.clone(),
                        target.clone(),
                        hit_actions,
                        ticket,
                        lease,
                    ));
                }
            } else if settings.open_on_hit != OpenOnHit::None {
                use tauri_plugin_opener::OpenerExt;
                match destination_url {
                    Some(url) => match app.opener().open_url(url, None::<&str>) {
                        Ok(()) => opened_destination = Some(settings.open_on_hit),
                        Err(err) => {
                            let _ = app.emit(
                                NOTICE_CHANNEL,
                                format!(
                                    "自动打开{}失败：{err}",
                                    destination_title(settings.open_on_hit)
                                ),
                            );
                        }
                    },
                    None => {
                        let _ = app.emit(
                            NOTICE_CHANNEL,
                            format!(
                                "自动打开{}失败：无法生成跳转地址",
                                destination_title(settings.open_on_hit)
                            ),
                        );
                    }
                }
            }

            if let Err(err) = dispatch_notification(&app, notification, &bark_url).await {
                // 提醒没发出去是遗憾，但绝不能让监控本身停下来。
                let _ = app.emit(NOTICE_CHANNEL, format!("发送提醒时出错：{err}"));
            } else {
                let mut actions = Vec::new();
                if settings.sound_enabled {
                    actions.push("提示音");
                }
                if !bark_url.trim().is_empty() {
                    actions.push(if has_product_bark {
                        "Bark（型号专属）"
                    } else {
                        "Bark"
                    });
                }
                if hit_actions.add_to_bag {
                    // 加购结果由后台任务单独报，这里只说一声已经开始。
                    actions.push("自动加购受去重与结账保护约束");
                }
                if hit_actions.checkout {
                    actions.push("自动结账（结果单独报告）");
                }
                if let Some(destination) = opened_destination {
                    match destination {
                        OpenOnHit::Bag => actions.push("已打开购物袋"),
                        OpenOnHit::Product => actions.push("已打开商品页"),
                        OpenOnHit::None => {}
                    }
                }
                if actions.is_empty() {
                    continue;
                }
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    format!(
                        "到货提醒已执行：{} {}（{}）",
                        target.store_title,
                        target.product_name,
                        actions.join("、")
                    ),
                );
            }
        }
    }
}

/// 建系统托盘。
///
/// 关窗口时不退出而是收进托盘：这个工具的正常用法就是挂上几个小时等发售，
/// 让它一直占着一个窗口和 Dock 图标没有道理。
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("默认窗口图标缺失，无法建立托盘".into())
            })?,
        )
        .tooltip("水果雷达")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_window(app),
            // AppHandle::exit also delivers ExitRequested: same barrier as Cmd-Q.
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

async fn shutdown_monitor(watcher: &Watcher, fetcher: &AppleChromiumFetcher) {
    watcher.stop().await;
    fetcher.shutdown().await;
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 载入设置。读不出来时放弃写盘并留档原文件。
///
/// 这一条是刻意的：读不到旧配置**不等于**用户没有配置。若照常写盘，界面初始化
/// 时的几次控件赋值就会把仅存的那份原子替换成一份空的默认配置，监控列表再也
/// 找不回来。Go 版正是这么丢过数据。
fn load_settings(notices: &mut Vec<String>) -> (Settings, Option<SettingsStore>) {
    let store = match SettingsStore::new() {
        Ok(s) => s,
        Err(err) => {
            notices.push(format!("配置目录不可用，本次运行的设置不会被保存：{err}"));
            return (Settings::default(), None);
        }
    };

    // 「新版配置文件还不存在」才是首次运行的判据。
    //
    // 不能用「目标列表为空」代替：用户删光目标后保存的是一份合法的空目标配置，
    // 而旧版 settings.json 是刻意保留不删的（为了能回退），于是下次启动会把他
    // 亲手删掉的目标连同 locale、Bark 地址一起原样倒回来。
    let first_run = store.path().symlink_metadata().is_err();

    match store.load() {
        Ok(settings) => {
            if first_run && let Some(previous) = store.import_previous_version() {
                notices.push(format!(
                    "已从改名前版本迁移了 {} 条监控目标。",
                    previous.targets.len()
                ));
                if let Err(err) = store.save(&previous) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (previous, Some(store));
            }
            if first_run
                && let Some(legacy) = store.import_legacy()
                && !legacy.targets.is_empty()
            {
                notices.push(format!(
                    "已从旧版设置迁移了 {} 条监控目标。",
                    legacy.targets.len()
                ));
                // 立刻落盘。否则用户不改任何设置时新版文件一直不存在，
                // 每次启动都要重迁一遍，用户删掉的目标也会一直复活。
                if let Err(err) = store.save(&legacy) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (legacy, Some(store));
            }
            (settings, Some(store))
        }
        Err(err) => {
            match store.preserve_corrupted() {
                Ok(Some(path)) => {
                    notices.push(format!("读取设置失败，原文件已备份到 {}", path.display()))
                }
                Ok(None) => {}
                Err(backup_err) => {
                    notices.push(format!("读取设置失败，且无法备份原文件：{backup_err}"));
                }
            }
            notices.push(format!(
                "读取设置失败，已回退到默认设置，并且本次运行不会覆盖磁盘上的配置：{err}"
            ));
            (Settings::default(), None)
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        // 第二次打开应用时不再起一个新实例（两个实例会抢查询资料目录的锁），
        // 而是把已有的窗口带到前台。必须最先注册。
        .plugin(tauri_plugin_single_instance::init(|app, _, _| reveal_window(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let mut notices = Vec::new();
            let (mut settings, store) = load_settings(&mut notices);
            let catalog = apw_core::config::app_dir()
                .map(|dir| Catalog::with_cache_dir(dir.join("catalog-cache")))
                .unwrap_or_else(|_| Catalog::new());
            let saved_targets = settings.targets.clone();
            catalog.hydrate_watch_targets(&mut settings.targets);
            catalog.hydrate_store_targets(&mut settings.targets);
            if settings.targets != saved_targets
                && let Some(store) = store.as_ref()
                && let Err(error) = store.save(&settings)
            {
                notices.push(format!(
                    "监控项的型号或门店名称已在本次运行修复，但暂时无法保存：{error}"
                ));
            }
            catalog.attach_pickup_locations(&mut settings.targets);

            // 用 Watcher::new 而不是 Watcher::spawn：setup 回调跑在主线程上，
            // 并不处在 tokio 运行时上下文里，在这里 tokio::spawn 会 panic，
            // 而且因为发生在不可展开的回调中，进程会直接 abort。
            // 引擎任务交给 Tauri 自己的运行时去驱动。
            let watcher_config = WatcherConfig {
                delivery_region: settings.delivery_region.clone(),
                ..WatcherConfig::default()
            };
            let fetcher = AppleChromiumFetcher::new();
            let (watcher, events, engine) = Watcher::new(fetcher.clone(), watcher_config);
            tauri::async_runtime::spawn(engine);

            {
                let watcher = watcher.clone();
                let fetcher = fetcher.clone();
                let network = settings.network.clone();
                let backoff_enabled = settings.backoff_enabled;
                let targets = settings.targets.clone();
                let interval = settings.interval();
                // 与 3d7c56e 一致：仅初始化，由用户手动开始监控。
                // 保留 actor 屏障，确保开放命令前目标和间隔已经生效；不读取恢复标记。
                tauri::async_runtime::block_on(async move {
                    fetcher.set_network(network).await;
                    apw_core::apple::Fetcher::set_backoff_enabled(&fetcher, backoff_enabled).await;
                    watcher.set_targets(targets).await;
                    watcher.set_interval(interval).await;
                    let _ = watcher.is_running().await;
                });
            }

            app.manage(AppState {
                watcher,
                fetcher,
                exit_barrier: exit_barrier::ExitBarrier::default(),
                control: tokio::sync::Mutex::new(()),
                admission: std::sync::Mutex::new(automation::Admission::default()),
                order_watch: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                orders: std::sync::Mutex::new(std::collections::HashSet::new()),
                catalog,
                http: reqwest::Client::new(),
                settings: RwLock::new(settings),
                store,
                bag: tokio::sync::Mutex::new(None),
                buyers: std::sync::Arc::new(bag::BuyerRegistry::default()),
                checkout: CheckoutRunner::new(),
                auto_paused: std::sync::atomic::AtomicBool::new(false),
            });

            let handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(pump_events(handle.clone(), events));

            if let Err(err) = setup_tray(&handle) {
                // 托盘建不起来只是少一项能力，不该让程序起不来。
                notices.push(format!("系统托盘不可用：{err}"));
            }

            if !notices.is_empty() {
                // 界面还没订阅上，稍等一下再发。这些是降级说明，用户必须看见 ——
                // 只写到 stderr 是没用的，用户是双击图标启动的。
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    for notice in notices {
                        let _ = handle.emit(NOTICE_CHANNEL, notice);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 收进托盘而不是退出。真要退出走托盘菜单里的「退出」。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_regions,
            list_categories,
            list_delivery_localities,
            list_watch_band_choices,
            list_watch_band_sizes,
            list_stores,
            list_products,
            refresh_products,
            get_settings,
            save_settings,
            set_checkout_switches,
            test_clash_route,
            list_clash_nodes,
            select_clash_node,
            get_snapshot,
            set_targets,
            set_interval,
            start_watching,
            stop_watching,
            retry_watching_now,
            is_running,
            test_notify,
            open_target_product,
            open_buyer_window,
            close_buyer_window,
            add_to_bag_now,
            fill_pickup_info,
            test_checkout,
            append_activity_log,
            check_for_update,
            install_update,
        ])
        .build(tauri::generate_context!())
        .expect("Tauri 应用启动失败")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                let state = app.state::<AppState>();
                let cleanup_app = app.clone();
                let exit_app = app.clone();
                state.exit_barrier.request(
                    || api.prevent_exit(),
                    async move {
                        let state = cleanup_app.state::<AppState>();
                        // Serialize with in-flight start/settings commands; do not let a
                        // late Start recreate Chromium after shutdown releases its owner.
                        // 先叫停结账、回收买家浏览器：它们可能正占着买家会话锁等页面，
                        // 不能让后面的步骤被它们拖住。
                        shutdown_automation(&state.checkout, &state.buyers).await;
                        let _control = state.control.lock().await;
                        state.admission.lock().unwrap().cancel();
                        shutdown_monitor(&state.watcher, &state.fetcher).await;
                    },
                    move || exit_app.exit(code.unwrap_or(0)),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{
        in_stock_notification_body, parse_delivery_localities, parse_watch_band_choices,
        parse_watch_band_sizes,
    };
    use apw_core::model::Target;

    #[test]
    fn apple地区响应过滤占位项并保留三级选项() {
        let response = r#"{
          "body": {
            "state": {"data":[{"text":"省份","value":""},{"text":"江苏","value":"江苏"}]},
            "city": {"data":[{"text":"城镇/城市","value":""},{"text":"苏州","value":"苏州"}]},
            "district": {"data":[{"text":"区","value":""},{"text":"吴江区","value":"吴江区"}]}
          }
        }"#;
        let parsed =
            parse_delivery_localities(response.as_bytes()).expect("应当解析 Apple 地区响应");
        assert_eq!(parsed.states[0].value, "江苏");
        assert_eq!(parsed.cities[0].value, "苏州");
        assert_eq!(parsed.districts[0].value, "吴江区");
        assert_eq!(parsed.states.len(), 1);
    }

    #[test]
    fn watch到货提醒明确标出送货查询使用的表带() {
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R390".into(),
            store_title: "上海-香港广场".into(),
            part_number: "MJCX4CH/B".into(),
            product_name: "Apple Watch Ultra 4 49 毫米 黑色".into(),
            companion_part: Some("MKDY4FE/A".into()),
            companion_name: None,
            kit_part: Some("Z0YQ".into()),
            pickup_location: None,
        };
        let body = in_stock_notification_body(&target);
        assert!(body.contains("Apple Watch Ultra 4"));
        assert!(body.contains("Watch 送货搭配表带 MKDY4FE/A"));
    }

    #[test]
    fn apple表带接口保留官网款式颜色和精确尺码零件号() {
        let choices = r#"{"body":{"items":{"sololoop":{"sortOrder":20,"sectionHeader":"<span>新配色</span> 单圈表带","subDimensionValue":[{"dimensionValue":"burgundy","text":"勃艮第酒红色","sortOrder":10}]}}}}"#;
        let parsed = parse_watch_band_choices(choices.as_bytes()).expect("应当解析表带选项");
        assert_eq!(parsed[0].style_key, "sololoop");
        assert_eq!(parsed[0].style_name, "新配色 单圈表带");
        assert_eq!(parsed[0].color_name, "勃艮第酒红色");

        let sizes = r#"{"body":{"options":[{"key":"6","text":"6","product":{"options":{"watch_bands":"MKJP4FE/A"}}}]}}"#;
        let parsed = parse_watch_band_sizes(sizes.as_bytes()).expect("应当解析表带尺码");
        assert_eq!(parsed[0].text, "6");
        assert_eq!(parsed[0].part_number, "MKJP4FE/A");
    }

    /// 离线：真实 Watcher + 只记录的 Fetcher，不访问网络、不读配置。
    #[derive(Clone, Default)]
    struct InStockFetcher;
    impl apw_core::apple::Fetcher for InStockFetcher {
        async fn pickup_message(
            &self,
            _region: &'static apw_core::model::Region,
            store: &str,
            targets: &[Target],
            _delivery_region: Option<&apw_core::model::DeliveryRegion>,
        ) -> Result<apw_core::apple::StoreAvailability, apw_core::apple::ApiError> {
            use apw_core::apple::{PartStatus, StoreAvailability};
            use apw_core::model::Availability;
            Ok(StoreAvailability {
                store_number: store.into(),
                store_name: "Offline".into(),
                parts: targets
                    .iter()
                    .map(|t| {
                        (
                            t.part_number.clone(),
                            PartStatus {
                                part_number: t.part_number.clone(),
                                availability: Availability::InStock,
                                product_title: None,
                                pickup_display: "available".into(),
                                pickup_details: None,
                            },
                        )
                    })
                    .collect(),
            })
        }
    }

    fn offline_target() -> Target {
        Target {
            locale: "zh_CN".into(),
            store_number: "R000".into(),
            store_title: "Offline".into(),
            part_number: "TEST1CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            companion_name: None,
            kit_part: None,
            pickup_location: None,
        }
    }

    #[tokio::test]
    async fn stop_on_hit_pauses_after_a_hit_round_and_never_during_exit() {
        use apw_core::watcher::{Event, Watcher, WatcherConfig};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let mut settings = apw_core::config::Settings {
            targets: vec![offline_target()],
            ..Default::default()
        };
        // 默认不暂停。
        assert!(!super::checkout::hit_actions(&settings, &settings.targets[0]).pause_monitoring);
        settings.stop_on_hit = true;
        assert!(super::checkout::hit_actions(&settings, &settings.targets[0]).pause_monitoring);

        let (watcher, mut events) = Watcher::spawn(
            InStockFetcher,
            WatcherConfig { interval: Duration::from_secs(3600), ..Default::default() },
        );
        watcher.set_targets(settings.targets.clone()).await;
        watcher.start().await;
        let control = tokio::sync::Mutex::new(());
        let auto_paused = AtomicBool::new(false);
        let barrier = super::exit_barrier::ExitBarrier::default();
        let mut saw_hit = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                match event {
                    Event::InStock { .. } => saw_hit = true,
                    Event::CycleComplete { .. } => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("离线一轮应当完成");
        assert!(saw_hit);
        assert!(watcher.is_running().await);
        assert!(super::pause_after_hit(&watcher, &control, &auto_paused, &barrier).await);
        assert!(!watcher.is_running().await, "到货一轮后应当暂停");
        assert!(auto_paused.load(Ordering::Acquire), "本轮已受理的自动化仍可继续");
        // 已暂停时重复触发不做任何事。
        assert!(!super::pause_after_hit(&watcher, &control, &auto_paused, &barrier).await);

        // 退出中：不去动监控（退出屏障自己负责停）。
        watcher.start().await;
        barrier.request(|| {}, std::future::pending::<()>(), || {});
        assert!(barrier.is_exiting());
        assert!(!super::pause_after_hit(&watcher, &control, &auto_paused, &barrier).await);
        watcher.stop().await;
    }

    #[tokio::test]
    async fn exit_barrier_cancels_checkout_and_reaps_buyer_browser_without_waiting_for_its_lock() {
        use std::process::Command;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let runner = super::CheckoutRunner::new();
        let buyers = Arc::new(super::bag::BuyerRegistry::default());
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        buyers
            .register(super::bag::BuyerProcess::for_test(child))
            .unwrap();
        // 一次结账正在进行，并占着买家会话锁（模拟正在等 Apple 页面）。
        let lease = runner.try_begin().expect("结账租约");
        let bag_lock = Arc::new(tokio::sync::Mutex::new(()));
        let held = bag_lock.clone().lock_owned().await;

        let barrier = super::exit_barrier::ExitBarrier::default();
        let exits = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (cleanup_runner, cleanup_buyers) = (runner.clone(), buyers.clone());
        let e = exits.clone();
        barrier.request(
            || {},
            async move {
                super::shutdown_automation(&cleanup_runner, &cleanup_buyers).await;
            },
            move || {
                e.fetch_add(1, Ordering::SeqCst);
                let _ = done_tx.send(());
            },
        );
        // 退出期间 Start 被拒绝的判据。
        assert!(barrier.is_exiting());
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("退出不能被结账占着的会话锁卡住")
            .unwrap();
        assert_eq!(exits.load(Ordering::SeqCst), 1);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "买家浏览器进程应已回收");
        assert!(runner.is_cancelled());
        assert!(runner.try_begin().is_none(), "退出后不能再开始结账");
        assert!(buyers.is_closed());
        let late = Command::new("sleep").arg("60").spawn().unwrap();
        let late_pid = late.id() as i32;
        assert!(buyers.register(super::bag::BuyerProcess::for_test(late)).is_err());
        assert_eq!(unsafe { libc::kill(late_pid, 0) }, -1);
        drop(held);
        drop(lease);
    }
}
