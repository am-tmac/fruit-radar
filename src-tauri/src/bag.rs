//! 有货时把商品自动加入购物袋。
//!
//! # 为什么不能复用库存查询那个浏览器
//!
//! 库存查询用的是一次性临时 profile 的无头会话，查完就丢；而购物袋是按
//! **profile 的 cookie** 记的。在临时会话里加进购物袋，用户在自己的窗口里
//! 根本看不到 —— 加购等于白加。所以这里单开一个持久的「买家 profile」：
//! 用户在里面登录一次，之后每次命中都复用同一份 cookie。
//!
//! # 边界
//!
//! 这里只做两件事：**选 AppleCare 选项**、**点「添加到购物袋」**。
//! 不填地址、不碰支付、不提交订单 —— 停在购物袋页面上，剩下的交给用户。
//! 这是刻意的：付款是不可撤销的，把最后一步交给人的成本只是一次点击，
//! 而自动结账出了错是无法挽回的。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::chromium_fetcher::{chromium_user_agent, find_chromium};

/// 浏览器进程启动并交出调试端口的等待上限。
const CHROME_START_TIMEOUT: Duration = Duration::from_secs(20);
/// 单条 DevTools 命令的等待上限。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// 购买页把加购按钮渲染出来（含风控握手）的等待上限。
const PAGE_READY_TIMEOUT: Duration = Duration::from_secs(90);
/// 从打开商品页到完成加购的总时间上限。
///
/// 给得比直觉宽：这期间要等购买流程的脚本接管页面、按钮稳定、点击生效，
/// 慢网络下每一步都可能花掉十几秒，而失败一方是「有货却没加进购物袋」。
const ADD_TO_BAG_TIMEOUT: Duration = Duration::from_secs(150);
/// 点击之后确认商品真的进了购物袋的等待上限。
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(45);
/// 按钮解除禁用后、点击之前必须再等的时间。
///
/// 实测：解锁后立刻点或 1 秒后点都会被完全忽略，约 5 秒后点才提交。
/// 这是购买流程自身的时序，不是可以靠加长超时绕开的。
const BUTTON_SETTLE: Duration = Duration::from_secs(5);
/// 点击「添加到购物袋」的尝试次数。每次都以 URL 是否变化判定上一次是否生效。
const CLICK_ATTEMPTS: usize = 3;
/// 单次点击后，等待页面进入下一步的上限。
const CLICK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// 判定「已经离开商品页初始状态」：加购成功会走到 `step=attach`，
/// 有的流程则直接进购物袋。比找页面上的「购物袋」字样可靠。
const LEFT_PRODUCT_PAGE_EXPR: &str =
    r#"location.href.includes("step=attach") || location.href.includes("shop/bag") || location.href.includes("step=")"#;
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 加购按钮的语义标识。Apple 在按钮上保留了 `data-autom`，比按文字或
/// class 匹配稳定得多 —— 后者只需改一次版式就会静默失效。
const ADD_TO_CART_SELECTOR: &str = r#"[data-autom="add-to-cart"]"#;
/// 「不折抵换购」单选。
///
/// 这是加购的**第一道**前置问题：它没被回答之前，AppleCare 单选框是 disabled
/// 的，点也点不动。自动加购固定选「不折抵」——折抵需要评估旧机状态、还要用户
/// 确认估价，那是人在页面上做的事，程序不该替他决定。
const NO_TRADE_IN_SELECTOR: &str = r#"input[data-autom="choose-noTradeIn"]"#;
/// 「加 AppleCare+」单选。
const APPLECARE_SELECTOR: &str = r#"input[data-autom="acp"]"#;
/// 「不加 AppleCare+」单选。
const NO_APPLECARE_SELECTOR: &str = r#"input[data-autom="noapplecare"]"#;
/// 按钮上必须出现的字样。改版后若这个标识被挪作他用，宁可失败也不误点。
const ADD_TO_CART_LABEL: &str = "添加到购物袋";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DebugTarget {
    #[serde(rename = "type")]
    kind: String,
    web_socket_debugger_url: Option<String>,
    /// 页面地址。用于在多个标签页里挑出真正相关的那一个。
    url: Option<String>,
}

/// 一次加购的结果，用于在活动日志里说明到底做成了什么。
pub struct BagOutcome {
    /// 给用户看的说明，如「已加入购物袋（未选 AppleCare）」。
    pub summary: String,
    /// 加购完成后停留的地址。
    pub url: String,
}

/// 取货人信息：门店取货的结账表单需要的那五项。
///
/// 实测（iPhone 17 门店取货，登录状态下）：Apple **不会**从账号预填其中任何
/// 一项，五栏全空，每次都要手打 —— 这就是「抢不过别人」里最贵的那几秒。
/// 这些值由用户自己在本机填写并保存，程序只负责填进表单，
/// **不提交订单、不接触支付信息**。
#[derive(Debug, Clone, Default)]
pub struct PickupInfo {
    pub last_name: String,
    pub first_name: String,
    pub email: String,
    pub phone: String,
    pub id_last4: String,
}

impl PickupInfo {
    /// 缺哪几项。缺项时应当让用户先补齐，而不是填一半就把他送到付款页。
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.last_name.trim().is_empty() {
            missing.push("姓氏");
        }
        if self.first_name.trim().is_empty() {
            missing.push("名字");
        }
        if self.email.trim().is_empty() {
            missing.push("电子邮件");
        }
        if self.phone.trim().is_empty() {
            missing.push("手机号码");
        }
        if self.id_last4.trim().is_empty() {
            missing.push("身份证件后四位");
        }
        missing
    }
}

/// 可见的买家浏览器会话。
///
/// 刻意**不实现 Drop 去杀进程**：用户可能正在这个窗口里结账，程序自己
/// 关窗口是最糟的时机。进程生命周期交由用户和应用退出决定。
pub struct BagSession {
    child: Option<Child>,
    socket: Socket,
    next_command_id: u64,
}

impl BagSession {
    /// 连接（必要时启动）可见的买家浏览器。
    ///
    /// 已经开着的实例优先复用：用户结账到一半时，程序重开窗口会把购物袋
    /// 和三步结账进度一起弄丢。
    pub async fn start_visible(profile: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(profile)
            .map_err(|e| format!("无法创建买家浏览器目录 {}：{e}", profile.display()))?;

        if let Some(port) = live_port(profile).await
            && let Ok(socket) = attach(port).await
        {
            return Ok(Self {
                child: None,
                socket,
                next_command_id: 1,
            });
        }

        let chrome = find_chromium()
            .ok_or("未找到 Google Chrome 或 Microsoft Edge，无法自动加入购物袋")?;
        let profile_arg = format!("--user-data-dir={}", profile.display());
        let user_agent_arg = format!("--user-agent={}", chromium_user_agent());
        // 浏览器自己的 stderr 落到 profile 目录里。
        //
        // 不留这个文件的话，「加购失败」只剩一句超时，而超时是最没有信息量的
        // 失败方式：到底是页面没加载、进程被系统回收，还是别的，全靠猜。
        // 每次启动覆盖写，上限就是一次会话的量，不会无限增长。
        let stderr_log = std::fs::File::create(profile.join("chrome-stderr.log"))
            .map_err(|e| format!("无法创建浏览器日志文件：{e}"))?;
        let mut child = Command::new(chrome)
            .args([
                // 与查询会话的关键差别：不加 `--headless=new`。
                // 这个窗口用户要看得见、要能接着结账。
                "--remote-debugging-port=0",
                profile_arg.as_str(),
                user_agent_arg.as_str(),
                "--disable-blink-features=AutomationControlled",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--window-size=1440,1000",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .map_err(|e| format!("无法启动买家浏览器：{e}"))?;

        let port = match wait_for_port(profile, &mut child).await {
            Ok(port) => port,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        };
        let socket = attach(port).await?;
        Ok(Self {
            child: Some(child),
            socket,
            next_command_id: 1,
        })
    }

    /// 地址栏当前地址，用于诊断和活动日志。
    pub async fn current_url(&mut self) -> Result<String, String> {
        let value = self.evaluate("location.href", false).await?;
        Ok(value.as_str().unwrap_or_default().to_owned())
    }

    /// 把窗口导航到指定地址（并尽量把它带到前台）。
    pub async fn navigate(&mut self, url: &str) -> Result<(), String> {
        self.command("Page.navigate", json!({ "url": url }))
            .await?;
        Ok(())
    }

    /// 把买家窗口带到前台。
    ///
    /// 只做「让用户看见」这一件事；失败不算错误 —— 窗口最坏也只是待在后面，
    /// 而为此中断整个提醒流程是不划算的。
    ///
    /// 注意：这里只看一眼最小化的窗口是否被挡，不做窗口管理。
    pub fn focus(&self) {
        #[cfg(target_os = "macos")]
        {
            let app = chromium_app_bundle();
            if let Some(app) = app {
                let _ = Command::new("open").arg(app).status();
            }
        }
    }

    /// 关掉买家浏览器。目前只在用户显式要求时调用。
    pub fn shutdown(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 打开商品页、选好 AppleCare、点「添加到购物袋」，然后停在购物袋。
    pub async fn add_to_bag(
        &mut self,
        product_url: &str,
        bag_url: &str,
        applecare: bool,
    ) -> Result<BagOutcome, String> {
        self.command("Page.navigate", json!({ "url": product_url }))
            .await?;

        // 1. 等购买流程把加购按钮渲染出来。
        //
        // 注意：这个按钮在页面的静态 HTML 里就已经存在（且是 disabled），所以
        // 「元素存在」**不等于**「购买流程已经初始化」。真正的判据是文档加载完成
        // 且按钮已经开始响应状态变化 —— 见下一步的轮询。
        self.wait_for(
            &format!(
                r#"document.readyState !== "loading" && !!document.querySelector('{ADD_TO_CART_SELECTOR}')"#
            ),
            PAGE_READY_TIMEOUT,
            "Apple 购买页加载",
        )
        .await?;

        // 2. 统一状态机：回答前置问题 → 等按钮**稳定**可用 → 点击 → 用 URL 变化确认。
        //
        // 购买页上的两道前置问题，顺序**不能换**（2026-09 在 iPhone 17 购买页实测）：
        //
        //   初始                加购按钮 disabled，AppleCare 单选框 disabled/disabled
        //   选「不折抵换购」      加购按钮 disabled，AppleCare 单选框被解锁
        //   选「不加 AppleCare」  加购按钮 ENABLED
        //
        // 也就是说 AppleCare 单选框在换购问题被回答之前是**点不动的**。只点
        // AppleCare，会看到「单选框一直没被选中、按钮一直禁用」——这种现象和
        // 「页面结构变了」一模一样，很容易误判成 Apple 把购买流程停掉了。
        //
        // 而「按钮 ENABLED」也**不等于**「点得动」：实测解锁后立刻点、甚至 1 秒后点
        // 都会被完全忽略，约 5 秒后点才会提交（`disabled` 属性先被移除，提交处理器
        // 随后才接上）。所以判据取「连续可用 ≥ BUTTON_SETTLE」，不是「当前可用」。
        //
        // 整件事写成一个循环而不是顺序几步，是因为页面会在中途重置自己的状态
        // （选项被清掉、按钮重新变灰）。任何一步失败都只是下一轮重来一次，比在
        // 中途抛错更接近用户要的结果：有货就一定要加进去。
        let selector = if applecare {
            APPLECARE_SELECTOR
        } else {
            NO_APPLECARE_SELECTOR
        };
        let option_expr = format!(
            r#"(() => {{
                const tradeIn = document.querySelector('{NO_TRADE_IN_SELECTOR}');
                if (tradeIn && !tradeIn.disabled && !tradeIn.checked) tradeIn.click();

                const radio = document.querySelector('{selector}');
                if (radio && !radio.disabled && !radio.checked) radio.click();

                const button = document.querySelector('{ADD_TO_CART_SELECTOR}');
                return [
                    tradeIn ? (tradeIn.checked ? "tradeIn=no" : "tradeIn=unset") : "tradeIn=absent",
                    radio
                        ? (radio.disabled ? "applecare=locked"
                            : (radio.checked ? "applecare=set" : "applecare=unset"))
                        : "applecare=absent",
                    button ? (button.disabled ? "button=disabled" : "button=ENABLED")
                           : "button=absent"
                ].join(" ");
            }})()"#
        );
        let click_expr = format!(
            r#"(() => {{
                const button = document.querySelector('{ADD_TO_CART_SELECTOR}');
                if (!button) return "missing";
                if (button.disabled) return "disabled";
                // 点之前复核按钮文字。这个语义标识若在改版后被挪作他用，
                // 宁可不加购，也不能误点到别的东西上。
                if (!(button.innerText || "").includes("{ADD_TO_CART_LABEL}")) return "unexpected-label";
                button.click();
                return "clicked";
            }})()"#
        );

        let deadline = Instant::now() + ADD_TO_BAG_TIMEOUT;
        let mut enabled_since: Option<Instant> = None;
        // 循环至少执行一次，且每轮开头都会赋值，所以不需要初值；
        // 给个初值反而会让编译器指出「赋了但从没被读过」。
        let mut last_state: String;
        let mut clicks = 0usize;
        loop {
            let state = self
                .evaluate(&option_expr, false)
                .await?
                .as_str()
                .unwrap_or_default()
                .to_owned();
            last_state = state.clone();

            if !state.contains("button=ENABLED") {
                // 还没到可点的状态：计时清零，下一轮重新看。
                enabled_since = None;
            } else if enabled_since.get_or_insert_with(Instant::now).elapsed() >= BUTTON_SETTLE {
                let clicked = self
                    .evaluate(&click_expr, false)
                    .await?
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                match clicked.as_str() {
                    "clicked" => {
                        clicks += 1;
                        // 用 URL 是否离开商品页初始状态来确认这次点击被受理了。
                        // 生效的点击约 1 秒内就会改变 URL，因此「没变」等价于
                        // 「没被受理」，重试不会造成重复加购。
                        let confirm_deadline = Instant::now() + CLICK_CONFIRM_TIMEOUT;
                        let mut left_product_page = false;
                        loop {
                            if self
                                .evaluate(LEFT_PRODUCT_PAGE_EXPR, false)
                                .await?
                                .as_bool()
                                == Some(true)
                            {
                                left_product_page = true;
                                break;
                            }
                            if Instant::now() >= confirm_deadline {
                                break;
                            }
                            tokio::time::sleep(POLL_INTERVAL).await;
                        }
                        if left_product_page {
                            break;
                        }
                        eprintln!("第 {clicks} 次点击「{ADD_TO_CART_LABEL}」没有生效，重新尝试");
                        enabled_since = None;
                        if clicks >= CLICK_ATTEMPTS {
                            return Err(format!(
                                "点击「{ADD_TO_CART_LABEL}」{clicks} 次后页面仍未进入下一步"
                            ));
                        }
                    }
                    // 查询与点击之间按钮又变灰了。这是竞态，不是故障：
                    // 下一轮重新计时、重新选选项即可。
                    "disabled" | "missing" => enabled_since = None,
                    other => {
                        return Err(format!(
                            "点击「{ADD_TO_CART_LABEL}」前的复核未通过：{other}"
                        ));
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err(format!(
                    "等待「添加到购物袋」可点击并完成加购超时（{}秒，最后状态：{last_state}）",
                    ADD_TO_BAG_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        let applecare_selected = if last_state.contains("applecare=absent") {
            "missing"
        } else {
            "selected"
        };


        // 6. 停在购物袋页面，把最后一步和付款留给用户。
        self.command("Page.navigate", json!({ "url": bag_url }))
            .await?;
        self.wait_for(
            "document.readyState !== 'loading'",
            CONFIRM_TIMEOUT,
            "购物袋页面加载",
        )
        .await?;
        let url = self.current_url().await.unwrap_or_else(|_| bag_url.to_owned());

        let applecare_note = if applecare {
            "含 AppleCare+"
        } else {
            "未选 AppleCare"
        };
        let note = if applecare_selected == "missing" {
            "（本产品无 AppleCare 选项）"
        } else {
            ""
        };
        Ok(BagOutcome {
            summary: format!("已加入购物袋（{applecare_note}）{note}"),
            url,
        })
    }

    /// 把取货人信息填进当前打开着的结账表单。
    ///
    /// 两处细节是有讲究的：
    ///
    /// - 用 `name` 属性定位输入框（`lastName`/`firstName`/`emailAddress`/
    ///   `fullDaytimePhone`/`nationalIdSelf`）。那串层级很深的 `id` 里还有一层
    ///   `selfPickupContact`，照着字段标签的长度截断去猜，少一段就会变成
    ///   「表单找不到」——这一条是踩过的。
    /// - 赋值必须走原生 setter 再派发 `input`/`change`，直接改 `value` 会被
    ///   React 丢掉：页面上看着填上了，提交时还是空的。
    pub async fn fill_pickup_info(&mut self, info: &PickupInfo) -> Result<String, String> {
        let values = json!({
            "lastName": info.last_name.trim(),
            "firstName": info.first_name.trim(),
            "emailAddress": info.email.trim(),
            "fullDaytimePhone": info.phone.trim(),
            "nationalIdSelf": info.id_last4.trim(),
        });
        let expression = format!(
            r#"(() => {{
                const values = {values};
                const setNativeValue = (el, value) => {{
                    const proto = el instanceof HTMLTextAreaElement
                        ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
                    Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
                    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
                    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
                    el.dispatchEvent(new Event("blur", {{ bubbles: true }}));
                }};
                const filled = [];
                const missing = [];
                for (const [name, value] of Object.entries(values)) {{
                    const el = document.querySelector('input[name="' + name + '"]');
                    if (!el) {{ missing.push(name); continue; }}
                    el.focus();
                    setNativeValue(el, value);
                    if ((el.value || "").trim().length > 0) filled.push(name);
                    else missing.push(name);
                }}
                return JSON.stringify({{ filled: filled, missing: missing }});
            }})()"#,
            values = values
        );
        let raw = self.evaluate(&expression, false).await?;
        let parsed: Value = serde_json::from_str(raw.as_str().unwrap_or_default())
            .map_err(|e| format!("填写结果无法解析：{e}"))?;
        let filled: Vec<String> = parsed
            .get("filled")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let missing: Vec<String> = parsed
            .get("missing")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        if filled.is_empty() {
            return Err(
                "没找到取货信息表单。请先在买家窗口里走到「继续填写取货详情」那一步。".to_owned(),
            );
        }
        if missing.is_empty() {
            Ok(format!(
                "已填入 {} 项取货信息（姓氏、名字、电子邮箱、手机、身份证件后四位），请核对后自己完成付款",
                filled.len()
            ))
        } else {
            Ok(format!(
                "已填入 {} 项；页面上没有：{}",
                filled.len(),
                missing.join("、")
            ))
        }
    }

    /// 检测当前页面是不是「订单已创建」的确认页；是则返回订单号。
    ///
    /// 下单成功后订单只是**待付款**，付款窗口通常只有 30 分钟。程序盯到这一步就
    /// 推一条到手机，用户可以直接在手机上付款，不必守着电脑看二维码。
    pub async fn detect_order(&mut self) -> Result<Option<String>, String> {
        let raw = self
            .evaluate(
                r#"(() => {
                    const href = location.href;
                    if (!href.includes("thankyou") && !href.includes("/shop/order/")) return "";
                    const text = document.body.innerText;
                    const match = text.match(/(W\d{8,})/);
                    return match ? match[1] : "unknown";
                })()"#,
                false,
            )
            .await?;
        let order = raw.as_str().unwrap_or_default().trim().to_owned();
        if order.is_empty() {
            Ok(None)
        } else {
            Ok(Some(order))
        }
    }

    async fn command(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_command_id;
        self.next_command_id = self.next_command_id.wrapping_add(1).max(1);
        let request = json!({ "id": id, "method": method, "params": params });
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.socket.send(Message::Text(request.to_string().into())),
        )
        .await
        .map_err(|_| format!("发送浏览器命令 {method} 超时"))?
        .map_err(|e| format!("发送浏览器命令 {method} 失败：{e}"))?;

        let wait = async {
            while let Some(message) = self.socket.next().await {
                let message = message.map_err(|e| format!("读取浏览器响应失败：{e}"))?;
                let Message::Text(text) = message else {
                    continue;
                };
                let response: Value =
                    serde_json::from_str(&text).map_err(|e| format!("浏览器响应无法解析：{e}"))?;
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = response.get("error") {
                    return Err(format!("浏览器命令 {method} 失败：{error}"));
                }
                return Ok(response.get("result").cloned().unwrap_or(Value::Null));
            }
            Err("浏览器调试连接意外关闭".to_owned())
        };

        tokio::time::timeout(COMMAND_TIMEOUT, wait)
            .await
            .map_err(|_| format!("浏览器命令 {method} 超时"))?
    }

    async fn evaluate(&mut self, expression: &str, await_promise: bool) -> Result<Value, String> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": await_promise,
                    "returnByValue": true
                }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            // 和库存查询一样：exceptionDetails 带着整段 stack，适合开发诊断，
            // 不适合铺到用户的活动日志里。
            eprintln!("买家浏览器 Runtime.evaluate exceptionDetails: {details}");
            return Err("页面脚本执行出错（详见日志）".to_owned());
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 轮询一个返回布尔的表达式，直到为真或超时。
    async fn wait_for(
        &mut self,
        expression: &str,
        timeout: Duration,
        what: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.evaluate(expression, false).await?.as_bool() == Some(true) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("等待{what}超时（{}秒）", timeout.as_secs()));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// 该 profile 的浏览器当前是否活着（有可用的调试端口）。
///
/// 内部逻辑就是 [`live_port`]：端口文件存在**且真发一次请求能通**才算活着，
/// 因为浏览器退出后不会删掉那个文件。
pub async fn live_port_of(profile: &Path) -> Option<u16> {
    live_port(profile).await
}

/// 读该 profile 上一次留下的调试端口，并确认它真的还活着。
///
/// `DevToolsActivePort` 在浏览器退出后并不会被删掉，直接拿来用会得到一个
/// 连不上的端口。所以必须真发一次请求才算数。
async fn live_port(profile: &Path) -> Option<u16> {
    let contents = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let port: u16 = contents.lines().next()?.trim().parse().ok()?;
    let url = format!("http://127.0.0.1:{port}/json/version");
    let response = tokio::time::timeout(Duration::from_secs(3), reqwest::get(&url))
        .await
        .ok()?
        .ok()?;
    response.json::<Value>().await.ok()?;
    Some(port)
}

async fn wait_for_port(profile: &Path, child: &mut Child) -> Result<u16, String> {
    let deadline = Instant::now() + CHROME_START_TIMEOUT;
    loop {
        if let Some(port) = live_port(profile).await {
            return Ok(port);
        }
        if let Some(status) = child.try_wait().ok().flatten() {
            return Err(format!(
                "买家浏览器启动后立即退出（{status}）。\
                 若该数据目录正被另一个 Chrome 窗口占用，请先关掉它再试。"
            ));
        }
        if Instant::now() >= deadline {
            return Err("等待买家浏览器启动超时".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn attach(port: u16) -> Result<Socket, String> {
    let socket_url = match pick_page_target(port).await? {
        Some((url, _)) => url,
        None => {
            // 浏览器进程还在、但一个页面都没有：macOS 上关掉最后一个窗口不会退出
            // Chrome。这时必须先开一个标签页，否则会被误判成「浏览器不可用」而
            // 去重开 —— 重开会被已有实例接管并立即退出，最后表现为一个莫名其妙的
            // 启动失败。注意 /json/new 只接受 PUT，GET 会返回 405。
            reqwest::Client::new()
                .put(format!(
                    "http://127.0.0.1:{port}/json/new?url=about:blank"
                ))
                .send()
                .await
                .map_err(|e| format!("无法新建浏览器标签页：{e}"))?;
            pick_page_target(port)
                .await?
                .map(|(url, _)| url)
                .ok_or("浏览器没有可用页面目标，且无法新建标签页")?
        }
    };
    let (socket, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(&socket_url))
        .await
        .map_err(|_| "连接浏览器调试连接超时".to_owned())?
        .map_err(|e| format!("无法连接浏览器页面：{e}"))?;
    Ok(socket)
}

/// 在多个标签页里挑一个页面目标。
///
/// **优先结账页**：自动加购和代填取货信息都发生在结账流程里，而用户可能同时
/// 开着商品页、购物袋、结账页好几个标签。抓第一个目标等于赌运气，赌错的表现是
/// 「明明走到结账页了，程序却说找不到表单」。
/// Inspect all buyer tabs, not merely the cached CDP target. Fail closed.
pub async fn ensure_no_checkout_pages(profile: &Path) -> Result<(), String> {
    let port = live_port(profile).await.ok_or("买家浏览器状态不可确认")?;
    let pages: Vec<DebugTarget> = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::get(format!("http://127.0.0.1:{port}/json/list")).await?
            .json().await
    }).await.map_err(|_| "检查买家页面超时")?
        .map_err(|_: reqwest::Error| "检查买家页面失败")?;
    if pages.iter().filter(|p| p.kind == "page")
        .any(|p| !crate::automation::safe_to_navigate(p.url.as_deref().unwrap_or_default())) {
        return Err("买家窗口含结账、订单或未知页面；自动加购已跳过".into());
    }
    Ok(())
}

async fn pick_page_target(port: u16) -> Result<Option<(String, String)>, String> {
    let targets: Vec<DebugTarget> = tokio::time::timeout(
        Duration::from_secs(10),
        reqwest::get(format!("http://127.0.0.1:{port}/json/list")),
    )
    .await
    .map_err(|_| "读取浏览器页面列表超时".to_owned())?
    .map_err(|e| format!("无法连接浏览器调试端口：{e}"))?
    .json()
    .await
    .map_err(|e| format!("浏览器页面列表无法解析：{e}"))?;

    let pages: Vec<(String, String)> = targets
        .into_iter()
        .filter(|target| target.kind == "page")
        .filter_map(|target| {
            Some((target.web_socket_debugger_url?, target.url.unwrap_or_default()))
        })
        .collect();

    Ok(pages
        .iter()
        .find(|(_, url)| url.contains("/checkout"))
        .cloned()
        .or_else(|| pages.into_iter().next()))
}

/// 买家浏览器对应的应用包路径，用于把它带到前台。
#[cfg(target_os = "macos")]
fn chromium_app_bundle() -> Option<&'static str> {
    [
        "/Applications/Google Chrome.app",
        "/Applications/Microsoft Edge.app",
    ]
    .into_iter()
    .find(|path| Path::new(path).is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真跑一遍自动加购：开一个真实浏览器、走完 Apple 购买流程、确认商品进了购物袋。
    ///
    /// 标 `#[ignore]` 是刻意的：它要联网、要开可见窗口、一次约一分钟，不适合放进
    /// 常规测试。但它是这个模块**唯一**能证明「选择器还对得上 Apple 当前页面」的
    /// 手段 —— Apple 一改版，`data-autom` 就可能失效，而失效的方式是静默超时。
    /// 所以售卖季之前、或者加购开始报超时的时候，手动跑一次：
    ///
    /// ```text
    /// cargo test -p apple-store-inventory-monitor -- --ignored 真实加购
    /// ```
    #[tokio::test]
    #[ignore = "需要联网、会打开可见浏览器窗口，按需手动运行"]
    async fn 真实加购链路能把商品放进购物袋() {
        // 靶子可换：Apple 有时会把可配置商品的购买流程整体禁用（例如深夜维护），
        // 那时连 AppleCare 单选框本身都是 disabled，任何点击都不可能成功。
        // 用环境变量指向当时真正可购买的商品，就不用为了「验证代码」而去改代码。
        let product_url = std::env::var("FRUIT_RADAR_BAG_TEST_URL").unwrap_or_else(|_| {
            "https://www.apple.com.cn/shop/buy-iphone/iphone-17/mg724ch/a".to_owned()
        });
        let expect = std::env::var("FRUIT_RADAR_BAG_TEST_EXPECT")
            .unwrap_or_else(|_| "iPhone 17".to_owned());
        eprintln!("测试靶子：{product_url}（期望购物袋里出现「{expect}」）");

        // 用固定前缀的独立目录，**失败时不删除**：出问题时要能进去看
        // chrome-stderr.log。目录名带进程号，避免和上一轮残留的浏览器抢同一个
        // profile（Chrome 遇到已在使用的 profile 会直接退出）。
        let profile = std::env::temp_dir().join(format!("fruit-radar-bag-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).expect("创建测试 profile 失败");
        eprintln!("测试 profile：{}（失败时可在此查看 chrome-stderr.log）", profile.display());

        let mut session = BagSession::start_visible(&profile)
            .await
            .expect("启动买家浏览器失败");

        let outcome = session
            .add_to_bag(
                &product_url,
                "https://www.apple.com.cn/shop/bag",
                false,
            )
            .await
            .expect("加购失败");

        assert!(
            outcome.url.contains("shop/bag"),
            "没有停在购物袋页面：{}",
            outcome.url
        );

        // 光看 URL 不够：购物袋页面打开也可能是个空的袋子。
        let body = session
            .evaluate(r#"document.body.innerText.replace(/\s+/g," ")"#, false)
            .await
            .expect("读取购物袋内容失败");
        let text = body.as_str().unwrap_or_default();
        assert!(
            text.contains(&expect),
            "购物袋里没有「{expect}」：{}",
            &text[..text.len().min(300)]
        );

        session.shutdown();
    }

    /// 回归：用户把买家窗口关掉（Chrome 进程仍在）之后，必须还能用。
    ///
    /// macOS 上关掉最后一个窗口并不会退出 Chrome，此时 `DevToolsActivePort` 还在、
    /// 调试端口也活着，但一个页面目标都没有。早期实现会把这判成「浏览器不可用」
    /// 而去重开，而重开会被已有实例接管并立即退出 —— 用户看到的是一句莫名其妙
    /// 的启动失败。
    #[tokio::test]
    #[ignore = "会打开可见浏览器窗口，按需手动运行"]
    async fn 关掉所有窗口后仍能接管买家浏览器() {
        let profile = std::env::temp_dir()
            .join(format!("fruit-radar-bag-nopage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).expect("创建测试 profile 失败");

        let mut first = BagSession::start_visible(&profile)
            .await
            .expect("首次启动买家浏览器失败");

        close_all_pages(&profile).await.expect("关闭页面目标失败");
        assert_eq!(
            pick_page_target(read_port(&profile)).await.expect("查询页面目标失败"),
            None,
            "页面目标本应已全部关闭"
        );

        let second = BagSession::start_visible(&profile).await;
        assert!(
            second.is_ok(),
            "关掉所有窗口后应当能自己开回标签页，实际失败：{:?}",
            second.err()
        );
        if let Ok(mut session) = second {
            session.shutdown();
        }
        first.shutdown();
    }

    fn read_port(profile: &Path) -> u16 {
        std::fs::read_to_string(profile.join("DevToolsActivePort"))
            .expect("读取端口文件失败")
            .lines()
            .next()
            .expect("端口文件为空")
            .trim()
            .parse()
            .expect("端口无法解析")
    }

    /// 用浏览器级调试连接关掉所有页面目标，模拟「用户把窗口关了」。
    async fn close_all_pages(profile: &Path) -> Result<(), String> {
        let contents = std::fs::read_to_string(profile.join("DevToolsActivePort"))
            .map_err(|e| e.to_string())?;
        let mut lines = contents.lines();
        let port: u16 = lines
            .next()
            .ok_or("端口文件缺少端口号")?
            .trim()
            .parse()
            .map_err(|e: std::num::ParseIntError| e.to_string())?;
        let path = lines.next().ok_or("端口文件缺少浏览器路径")?.trim();
        let browser_ws = format!("ws://127.0.0.1:{port}{path}");

        let targets: Vec<serde_json::Value> = tokio::time::timeout(
            Duration::from_secs(10),
            reqwest::get(format!("http://127.0.0.1:{port}/json/list")),
        )
        .await
        .map_err(|_| "读取页面列表超时".to_owned())?
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
        let ids: Vec<String> = targets
            .into_iter()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();

        let (mut socket, _) = connect_async(&browser_ws)
            .await
            .map_err(|e| format!("连接浏览器级调试端口失败：{e}"))?;
        for (index, id) in ids.iter().enumerate() {
            let request = json!({
                "id": index + 1,
                "method": "Target.closeTarget",
                "params": { "targetId": id }
            });
            SinkExt::send(&mut socket, Message::Text(request.to_string().into()))
                .await
                .map_err(|e| e.to_string())?;
            let _ = tokio::time::timeout(Duration::from_secs(5), socket.next()).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(())
    }
}
